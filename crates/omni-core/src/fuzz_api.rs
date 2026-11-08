//! Hidden entry points for fuzzing each stage of the pipeline.
//!
//! A cargo-fuzz target is a separate crate, so it can only see `pub` items. The
//! stable door for embedders is [`crate::Engine`]; these functions exist to let
//! a fuzz binary drive the lexer, parser, lowering pass, VM, and instruction
//! decoder independently, and they are `#[doc(hidden)]` so they do not promise
//! API stability.
//!
//! `execute_bytes` is the decoder's fuzz input: every eight bytes become one
//! [`crate::bytecode::Instr`] and the bytes themselves are also used as the
//! source text an instruction's span addresses. Every span is read through
//! `str::get`, so an index that lands mid-character or past the end is an empty
//! literal rather than a panic.

use crate::Engine;
use crate::ast::Ast;
use crate::bytecode::{Chunk, Instr, OpCode};
use crate::lexer;

/// Every opcode in discriminant order, so an arbitrary byte always selects a
/// valid instruction.
const OPS: [OpCode; 20] = [
    OpCode::PushNumber,
    OpCode::PushStr,
    OpCode::PushNil,
    OpCode::PushUndef,
    OpCode::Load,
    OpCode::Store,
    OpCode::Pop,
    OpCode::Add,
    OpCode::Sub,
    OpCode::Mul,
    OpCode::Div,
    OpCode::Mod,
    OpCode::Eq,
    OpCode::Ne,
    OpCode::Neg,
    OpCode::Jump,
    OpCode::JumpIfFalse,
    OpCode::Call,
    OpCode::Ret,
    OpCode::Import,
];

/// Lex only. Returns the token count on success.
pub fn lex_only(source: &str) -> Result<usize, String> {
    let mut tokens = Vec::new();
    lexer::lex_into(source, &mut tokens).map_err(|e| e.to_string())?;
    Ok(tokens.len())
}

/// Lex and parse. Returns the AST node count on success.
pub fn parse_only(source: &str) -> Result<usize, String> {
    let mut tokens = Vec::new();
    lexer::lex_into(source, &mut tokens).map_err(|e| e.to_string())?;
    let mut ast = Ast::new();
    ast.parse_into(source, &tokens).map_err(|e| e.to_string())?;
    Ok(ast.nodes.len())
}

/// Lex, parse, and lower. Returns the instruction count on success.
pub fn lower_only(source: &str) -> Result<usize, String> {
    let mut tokens = Vec::new();
    lexer::lex_into(source, &mut tokens).map_err(|e| e.to_string())?;
    let mut ast = Ast::new();
    ast.parse_into(source, &tokens).map_err(|e| e.to_string())?;
    let engine = Engine::new();
    let mut chunk = Chunk::new();
    let mut tasks = Vec::new();
    crate::bytecode::compile(&ast, source, &engine, &mut chunk, &mut tasks)
        .map_err(|e| e.to_string())?;
    Ok(chunk.code.len())
}

/// Run the whole engine, including the VM, on one program.
pub fn execute(source: &str) -> Result<(), String> {
    Engine::new()
        .exec(source)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Decode eight bytes per instruction and execute the chunk.
///
/// The same bytes are the source the instructions' spans address, so literal
/// instructions see arbitrary text while jump targets and argument counts see
/// arbitrary numbers. A malformed chunk must return an error, never abort.
pub fn execute_bytes(input: &[u8]) -> Result<(), String> {
    execute_bytes_with_limit(input, crate::DEFAULT_STEP_LIMIT)
}

/// The decoder path with an explicit budget, so a test can prove the step limit
/// catches a chunk that jumps to itself without running sixteen million
/// instructions first.
pub(crate) fn execute_bytes_with_limit(input: &[u8], steps: u64) -> Result<(), String> {
    let source = String::from_utf8_lossy(input);
    let mut chunk = Chunk::new();
    for word in input.chunks_exact(8) {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(word);
        chunk.code.push(Instr {
            op: OPS[(bytes[0] as usize) % OPS.len()],
            a: u32::from(u16::from_le_bytes([bytes[1], bytes[2]])),
            b: u32::from(u16::from_le_bytes([bytes[3], bytes[4]])),
            c: u32::from(u16::from_le_bytes([bytes[5], bytes[6]])),
            line: 1 + u32::from(bytes[7] & 0x0F),
            column: 1 + u32::from(bytes[7] >> 4),
        });
    }
    let mut engine = Engine::new();
    engine.set_step_limit(steps);
    engine
        .run_chunk_for_fuzz(&chunk, &source)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::panic::{UnwindSafe, catch_unwind};

    use super::*;

    /// A fixed-seed xorshift64: the harness must be reproducible, so no clock
    /// or OS randomness reaches it.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    const SEEDS: &[&str] = &[
        "",
        "1",
        "1 + 1",
        "let a = 1",
        "\"hi\"",
        "\"a\\n\\u{1F600}\"",
        "let x = 1\nx + 2",
        "len(\"abc\")",
        "clamp(1, 2, 3)",
        "1 == 2 != 3",
        "((((1))))",
        "--5",
        "1 +",
        "\"unterminated",
        "\"\\u{D800}\"",
        "let 1 = 2",
        "a.b",
        "f(1, 2, 3)",
        "import \"m\"",
        "fs.read_text(\"x\")",
        "fs.write_text(\"x\", \"y\")",
        "json.parse(\"[1,2]\")",
        "json.stringify(1)",
        "1 / 0",
        "@#$%",
    ];

    const MAX_INPUT: usize = 512;
    const ITERATIONS: usize = 1_200;

    fn mutate(rng: &mut Rng, seed: &str) -> Vec<u8> {
        let mut bytes = seed.as_bytes().to_vec();
        let edits = 1 + rng.below(4);
        for _ in 0..edits {
            if bytes.is_empty() {
                bytes.push(b'1');
            }
            if bytes.len() >= MAX_INPUT {
                let cut = rng.below(bytes.len());
                bytes.remove(cut);
            }
            match rng.below(4) {
                0 => {
                    let at = rng.below(bytes.len());
                    bytes[at] = (rng.next() & 0xFF) as u8;
                }
                1 => {
                    let at = rng.below(bytes.len() + 1);
                    bytes.insert(at, (rng.next() & 0xFF) as u8);
                }
                2 => {
                    let at = rng.below(bytes.len());
                    bytes.remove(at);
                }
                _ => {
                    let at = rng.below(bytes.len());
                    let run = 1 + rng.below(8);
                    let byte = bytes[at];
                    for offset in 0..run {
                        let at = (at + offset).min(bytes.len());
                        bytes.insert(at, byte);
                    }
                }
            }
        }
        bytes
    }

    fn outcomes(source: &str) -> [String; 4] {
        [
            describe(|| lex_only(source).map(|n| n.to_string())),
            describe(|| parse_only(source).map(|n| n.to_string())),
            describe(|| lower_only(source).map(|n| n.to_string())),
            describe(|| execute(source).map(|()| String::new())),
        ]
    }

    fn describe<F>(stage: F) -> String
    where
        F: FnOnce() -> Result<String, String> + UnwindSafe,
    {
        match catch_unwind(stage) {
            Ok(Ok(value)) => format!("ok:{value}"),
            Ok(Err(message)) => format!("err:{message}"),
            Err(_) => "PANIC".to_string(),
        }
    }

    /// The `[Line L, Col C]` prefix every engine error carries.
    fn position(message: &str) -> Option<(u32, u32)> {
        let rest = message.strip_prefix("[Line ")?;
        let (line, rest) = rest.split_once(", Col ")?;
        let (column, _) = rest.split_once(']')?;
        Some((line.parse().ok()?, column.parse().ok()?))
    }

    #[test]
    fn mutating_a_corpus_never_panics_and_always_positions_its_errors() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut executed = 0usize;
        let mut decoder_inputs = 0usize;
        for _ in 0..ITERATIONS {
            let seed = SEEDS[rng.below(SEEDS.len())];
            let bytes = mutate(&mut rng, seed);
            let source = String::from_utf8_lossy(&bytes);

            let first = outcomes(&source);
            for found in &first {
                assert_ne!(
                    found, "PANIC",
                    "a stage panicked on {source:?}: {:?}",
                    bytes
                );
            }
            for found in &first {
                if let Some(message) = found.strip_prefix("err:") {
                    if let Some((line, column)) = position(message) {
                        assert!(line >= 1, "line 0 in {message:?} from {source:?}");
                        assert!(column >= 1, "column 0 in {message:?} from {source:?}");
                    }
                }
            }
            // Determinism: the same bytes must produce the same four outcomes.
            assert_eq!(first, outcomes(&source), "nondeterministic on {source:?}");

            decoder_inputs += 1;
            assert!(
                catch_unwind(|| execute_bytes(&bytes)).is_ok(),
                "the instruction decoder panicked on {:?}",
                bytes
            );
            if first[3].starts_with("ok") {
                executed += 1;
            }
        }
        println!(
            "fuzz harness: {ITERATIONS} inputs x 4 stages, {decoder_inputs} decoder chunks, \
             {executed} executed, 0 panics"
        );
    }

    #[test]
    fn the_decoder_survives_arbitrary_words_and_refuses_impossible_ones() {
        assert!(execute_bytes(&[]).is_ok(), "an empty chunk is empty");
        assert!(
            execute_bytes(&[0; 8]).is_err(),
            "PushNumber with an empty span"
        );
        // A jump to itself loops; a small budget turns it into an error.
        let mut jump = [0u8; 8];
        jump[0] = 15;
        let error = execute_bytes_with_limit(&jump, 32).unwrap_err();
        assert!(error.contains("step limit"), "{error}");
        // `Call` with a huge argument count underflows the stack; that is an
        // error, not an abort.
        let mut call = [0u8; 8];
        call[0] = 17;
        call[5] = 0xFF;
        call[6] = 0xFF;
        let error = execute_bytes(&call).unwrap_err();
        assert!(error.contains("underflow"), "{error}");
    }
}
