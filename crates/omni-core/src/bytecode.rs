//! The bytecode: an instruction set, a lowering pass, and a disassembler.
//!
//! The VM is a stack machine. Source positions live in each instruction rather
//! than in a side table, so a runtime failure points at the operator or name
//! that caused it without a second lookup.
//!
//! ## Why the operands are offsets
//!
//! A literal instruction carries the span of its text in the source, and a name
//! instruction carries an offset into [`Chunk::names`]. Neither owns a
//! `String`. The chunk is owned by [`crate::Engine`] and cleared for each
//! compilation, so a warm program rewrites bytes into buffers that already have
//! the room; it never asks the system allocator for one `String` per literal.
//! Strings are decoded when their instruction runs, which also keeps the order
//! of "division by zero" before "unknown escape" identical to the fused
//! evaluator's.

use crate::ast::{Ast, NONE, NodeKind};
use crate::engine::{Builtin, ErrorKind, MAX_ARGS, MAX_CALL_DEPTH, OmniError, Pos};
use crate::lexer::BinOp;

/// The complete instruction set for language version 2.
///
/// `PushNil`, `Jump`, and `JumpIfFalse` have no lowering source yet: the
/// grammar has no `nil` literal and no `if`, so nothing emits them. They are
/// defined here because they are part of the VM's contract and because tests
/// build chunks by hand to exercise the dispatch loop the compiler will use once
/// control flow reaches the language.
/// Public because the JIT tier reads the stream the VM executes; the module
/// doc explains the encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OpCode {
    PushNumber,
    PushStr,
    PushFolded,
    PushNil,
    PushUndef,
    Load,
    Store,
    Pop,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Neg,
    Jump,
    JumpIfFalse,
    Call,
    Ret,
    Import,
}

/// How many opcodes exist. The size of every profile heatmap array.
pub const OPCODE_COUNT: usize = 21;

/// Every opcode in discriminant order, indexed the way profile heatmaps and
/// [`opcode_name`] are. A test keeps this table and the enum in lockstep.
pub(crate) const OPCODES: [OpCode; OPCODE_COUNT] = [
    OpCode::PushNumber,
    OpCode::PushStr,
    OpCode::PushFolded,
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

/// The mnemonic for a heatmap index, or `None` past [`OPCODE_COUNT`].
///
/// This is how a foreign host turns a profile's array indices into names
/// without shipping its own table; the FFI wraps it as `omni_opcode_name`.
pub fn opcode_name(index: u32) -> Option<&'static str> {
    OPCODES.get(index as usize).map(|op| op.mnemonic())
}

#[allow(dead_code)]
impl OpCode {
    pub(crate) fn mnemonic(self) -> &'static str {
        match self {
            OpCode::PushNumber => "PUSH_NUMBER",
            OpCode::PushStr => "PUSH_STR",
            OpCode::PushFolded => "PUSH_FOLDED",
            OpCode::PushNil => "PUSH_NIL",
            OpCode::PushUndef => "PUSH_UNDEF",
            OpCode::Load => "LOAD",
            OpCode::Store => "STORE",
            OpCode::Pop => "POP",
            OpCode::Add => "ADD",
            OpCode::Sub => "SUB",
            OpCode::Mul => "MUL",
            OpCode::Div => "DIV",
            OpCode::Mod => "MOD",
            OpCode::Eq => "EQ",
            OpCode::Ne => "NE",
            OpCode::Neg => "NEG",
            OpCode::Jump => "JUMP",
            OpCode::JumpIfFalse => "JUMP_IF_FALSE",
            OpCode::Call => "CALL",
            OpCode::Ret => "RET",
            OpCode::Import => "IMPORT",
        }
    }

    /// How many values this instruction pops and pushes, when that is fixed.
    ///
    /// `Call` depends on its argument count and `Ret` returns whatever is on
    /// top, so both answer `None`. A compiler backend that needs a stack depth
    /// can use this to verify a chunk; the VM itself does not need it because
    /// lowering has already balanced the stack.
    pub fn stack_effect(self) -> Option<(u8, u8)> {
        Some(match self {
            OpCode::PushNumber
            | OpCode::PushStr
            | OpCode::PushFolded
            | OpCode::PushNil
            | OpCode::PushUndef
            | OpCode::Load
            | OpCode::Import => (0, 1),
            OpCode::Store | OpCode::Neg => (1, 1),
            OpCode::Pop => (1, 0),
            OpCode::Add
            | OpCode::Sub
            | OpCode::Mul
            | OpCode::Div
            | OpCode::Mod
            | OpCode::Eq
            | OpCode::Ne => (2, 1),
            OpCode::Jump => (0, 0),
            OpCode::JumpIfFalse => (1, 0),
            OpCode::Call | OpCode::Ret => return None,
        })
    }
}

/// One instruction. `Copy`, so the VM may hold it while the chunk is borrowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instr {
    pub op: OpCode,
    /// Source span for literals and imports, name offset for loads/stores/calls,
    /// jump target for branches, the low 32 bits of a folded `f64` for
    /// [`OpCode::PushFolded`], and unused otherwise.
    pub a: u32,
    /// Length for source spans and names, the high 32 bits of a folded `f64`.
    pub b: u32,
    /// Argument count for `Call`.
    pub c: u32,
    pub line: u32,
    pub column: u32,
}

impl Instr {
    #[allow(dead_code)]
    pub fn new(op: OpCode, a: u32, b: u32, c: u32, at: Pos) -> Instr {
        Instr {
            op,
            a,
            b,
            c,
            line: at.line,
            column: at.column,
        }
    }

    pub fn pos(self) -> Pos {
        Pos::new(self.line, self.column)
    }
}

/// A compiled program: instructions plus the identifier bytes they address.
#[derive(Debug, Default)]
pub struct Chunk {
    pub code: Vec<Instr>,
    /// Concatenated UTF-8 identifier bytes. `Load`, `Store`, and `Call` address
    /// a slice of this buffer, so an instruction stays fixed-size.
    pub names: Vec<u8>,
}

impl Chunk {
    pub fn new() -> Chunk {
        Chunk::default()
    }

    pub fn clear(&mut self) {
        self.code.clear();
        self.names.clear();
    }

    /// The identifier bytes an instruction names.
    pub fn name(&self, instr: Instr) -> &str {
        let start = instr.a as usize;
        let end = start.saturating_add(instr.b as usize);
        self.names
            .get(start..end)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }
}

/// One pending step of lowering: either descend into a node, or emit the
/// instruction that node's parent put off until its children were done.
///
/// Lowering walks this stack instead of the native one because a left-deep
/// spine — `1 + 1 + … + 1`, which the Python SDK's long-source test builds two
/// thousand operators deep — would otherwise recurse two thousand frames and
/// overflow a thread stack sized for the host language. The depth in `Visit` is
/// the number of enclosing calls, counted syntactically as the fused parser
/// counted it.
#[derive(Debug, Clone, Copy)]
pub enum Task {
    Visit(u32, u32),
    Emit(OpCode, u32, u32, u32, Pos),
}

/// Lower an AST into `chunk`, replacing its contents.
///
/// This is the only place that knows the mapping from grammar to opcodes. It
/// borrows the engine immutably for one question — does a call name resolve? —
/// because the fused parser answered that before it evaluated arguments and the
/// error ordering is part of the contract.
///
/// The work stack is the caller's, like every other buffer in the engine: it is
/// cleared on entry and keeps its capacity, so a warm program allocates nothing
/// to lower.
///
/// Public because the JIT tier lowers through the same door: it compiles a
/// chunk and runs it natively, then hands the same chunk to
/// [`crate::Engine::run_compiled`] when a deopt needs the VM's error.
pub fn compile(
    ast: &Ast,
    source: &str,
    engine: &crate::Engine,
    chunk: &mut Chunk,
    tasks: &mut Vec<Task>,
) -> Result<(), OmniError> {
    chunk.clear();
    let mut compiler = Compiler {
        ast,
        source,
        engine,
        chunk,
        tasks,
    };
    if ast.statements.is_empty() {
        compiler.emit(OpCode::PushUndef, 0, 0, 0, Pos::new(1, 1));
    }
    for (index, statement) in ast.statements.iter().copied().enumerate() {
        compiler.lower(statement)?;
        if index + 1 < ast.statements.len() {
            let pos = compiler.ast.nodes[statement as usize].pos;
            compiler.emit(OpCode::Pop, 0, 0, 0, pos);
        }
    }
    let pos = ast
        .statements
        .last()
        .map_or(Pos::new(1, 1), |node| ast.nodes[*node as usize].pos);
    compiler.emit(OpCode::Ret, 0, 0, 0, pos);
    Ok(())
}

struct Compiler<'a> {
    ast: &'a Ast,
    source: &'a str,
    engine: &'a crate::Engine,
    chunk: &'a mut Chunk,
    tasks: &'a mut Vec<Task>,
}

impl Compiler<'_> {
    fn emit(&mut self, op: OpCode, a: u32, b: u32, c: u32, at: Pos) -> u32 {
        let index = self.chunk.code.len() as u32;
        self.chunk.code.push(Instr::new(op, a, b, c, at));
        index
    }

    /// Lower one statement or expression subtree, post-order.
    fn lower(&mut self, root: u32) -> Result<(), OmniError> {
        self.tasks.clear();
        self.tasks.push(Task::Visit(root, 0));
        while let Some(task) = self.tasks.pop() {
            match task {
                Task::Visit(index, depth) => self.visit(index, depth)?,
                Task::Emit(op, a, b, c, at) => {
                    self.emit(op, a, b, c, at);
                }
            }
        }
        Ok(())
    }

    /// Push the work for one node.
    ///
    /// Children go on the stack in reverse execution order so the last thing
    /// pushed — and therefore the first thing popped — is the leftmost operand.
    /// The instruction that combines them sits underneath, so it emits only once
    /// every operand above it has been lowered, which is post-order.
    fn visit(&mut self, index: u32, depth: u32) -> Result<(), OmniError> {
        let node = self.ast.nodes[index as usize];
        match node.kind {
            NodeKind::Number => {
                self.tasks.push(Task::Emit(
                    OpCode::PushNumber,
                    node.start,
                    node.len,
                    0,
                    node.pos,
                ));
            }
            NodeKind::Str => {
                self.tasks.push(Task::Emit(
                    OpCode::PushStr,
                    node.start,
                    node.len,
                    0,
                    node.pos,
                ));
            }
            NodeKind::Var => {
                let (offset, len) = self.intern_span(node.start, node.len);
                self.tasks
                    .push(Task::Emit(OpCode::Load, offset, len, 0, node.pos));
            }
            NodeKind::Member => {
                let (offset, len) =
                    self.intern_member(node.start, node.len, node.start2, node.len2);
                self.tasks
                    .push(Task::Emit(OpCode::Load, offset, len, 0, node.pos));
            }
            NodeKind::Let => {
                let (offset, len) = self.intern_span(node.start, node.len);
                self.tasks
                    .push(Task::Emit(OpCode::Store, offset, len, 0, node.pos));
                self.tasks.push(Task::Visit(node.lhs, depth));
            }
            NodeKind::Import => {
                self.tasks.push(Task::Emit(
                    OpCode::Import,
                    node.start,
                    node.len,
                    0,
                    node.pos,
                ));
            }
            NodeKind::UnaryNeg => {
                self.tasks.push(Task::Emit(OpCode::Neg, 0, 0, 0, node.pos));
                self.tasks.push(Task::Visit(node.lhs, depth));
            }
            NodeKind::Binary(op) => {
                let opcode = match op {
                    BinOp::Add => OpCode::Add,
                    BinOp::Sub => OpCode::Sub,
                    BinOp::Mul => OpCode::Mul,
                    BinOp::Div => OpCode::Div,
                    BinOp::Mod => OpCode::Mod,
                };
                self.tasks.push(Task::Emit(opcode, 0, 0, 0, node.pos));
                self.tasks.push(Task::Visit(node.rhs, depth));
                self.tasks.push(Task::Visit(node.lhs, depth));
            }
            NodeKind::Eq => {
                self.tasks.push(Task::Emit(OpCode::Eq, 0, 0, 0, node.pos));
                self.tasks.push(Task::Visit(node.rhs, depth));
                self.tasks.push(Task::Visit(node.lhs, depth));
            }
            NodeKind::Ne => {
                self.tasks.push(Task::Emit(OpCode::Ne, 0, 0, 0, node.pos));
                self.tasks.push(Task::Visit(node.rhs, depth));
                self.tasks.push(Task::Visit(node.lhs, depth));
            }
            NodeKind::Call => {
                let (offset, len) = self.intern_span(node.start, node.len);
                self.call(offset, len, node, depth)?;
            }
            NodeKind::CallMember => {
                let (offset, len) =
                    self.intern_member(node.start, node.len, node.start2, node.len2);
                self.call(offset, len, node, depth)?;
            }
        }
        Ok(())
    }

    /// Schedule a call after the parser has already built its argument chain.
    ///
    /// The order is the fused parser's: nesting limit, then existence, then the
    /// arguments. Existence before arguments is observable — `nope(1 / 0)`
    /// must name `nope`, not report a division by zero — and the nesting limit
    /// is measured while descending so `id(id(…))` spends the same number of
    /// frames it used to. Scheduling the call instruction first and the argument
    /// visits above it keeps that order: the checks happen here, at the node's
    /// turn, and the operands still lower underneath the `Call`.
    fn call(
        &mut self,
        offset: u32,
        len: u32,
        node: crate::ast::Node,
        depth: u32,
    ) -> Result<(), OmniError> {
        if depth >= MAX_CALL_DEPTH {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                node.pos,
                format!(
                    "calls are nested {MAX_CALL_DEPTH} deep, the limit; a host function \
                     cannot call itself"
                ),
            ));
        }
        let name = self
            .chunk
            .names
            .get(offset as usize..offset as usize + len as usize)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("");
        if !self.engine.is_callable(name) {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                node.pos,
                format!("unknown function '{name}' (no host function or builtin has that name)"),
            ));
        }
        // The parser caps a call at `MAX_ARGS`, so `count` fits the array; the
        // cap is repeated here so a malformed AST cannot index past it.
        let count = (node.rhs as usize).min(MAX_ARGS);
        let mut arguments = [NONE; MAX_ARGS];
        let mut argument = node.lhs;
        for slot in arguments.iter_mut().take(count) {
            if argument == NONE {
                break;
            }
            *slot = argument;
            argument = self.ast.nodes[argument as usize].next;
        }
        self.tasks
            .push(Task::Emit(OpCode::Call, offset, len, node.rhs, node.pos));
        for slot in arguments[..count].iter().rev() {
            self.tasks.push(Task::Visit(*slot, depth + 1));
        }
        Ok(())
    }

    fn intern_span(&mut self, start: u32, len: u32) -> (u32, u32) {
        let offset = self.chunk.names.len() as u32;
        let start = start as usize;
        let text = self
            .source
            .get(start..start.saturating_add(len as usize))
            .unwrap_or("");
        self.chunk.names.extend_from_slice(text.as_bytes());
        let len = self.chunk.names.len() as u32 - offset;
        (offset, len)
    }

    fn intern_member(&mut self, start: u32, len: u32, start2: u32, len2: u32) -> (u32, u32) {
        let offset = self.chunk.names.len() as u32;
        let first = start as usize;
        let second = start2 as usize;
        let base = self
            .source
            .get(first..first.saturating_add(len as usize))
            .unwrap_or("");
        let member = self
            .source
            .get(second..second.saturating_add(len2 as usize))
            .unwrap_or("");
        self.chunk.names.extend_from_slice(base.as_bytes());
        self.chunk.names.push(b'.');
        self.chunk.names.extend_from_slice(member.as_bytes());
        (offset, self.chunk.names.len() as u32 - offset)
    }
}

/// Render a chunk as one line per instruction.
///
/// `source` is needed because literal instructions carry spans. This allocates
/// and is for tests, diagnostics, and anyone reading a compilation; the VM never
/// calls it.
#[allow(dead_code)]
pub fn disassemble(chunk: &Chunk, source: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for (index, instr) in chunk.code.iter().copied().enumerate() {
        let operand = match instr.op {
            OpCode::PushNumber | OpCode::PushStr | OpCode::Import => {
                let start = instr.a as usize;
                let text = source
                    .get(start..start.saturating_add(instr.b as usize))
                    .unwrap_or("");
                if instr.op == OpCode::PushStr {
                    format!("\"{text}\"")
                } else {
                    text.to_string()
                }
            }
            OpCode::PushFolded => format!(
                "{}",
                f64::from_bits(u64::from(instr.a) | (u64::from(instr.b) << 32))
            ),
            OpCode::Load | OpCode::Store => chunk.name(instr).to_string(),
            OpCode::Call => format!("{}({})", chunk.name(instr), instr.c),
            OpCode::Jump | OpCode::JumpIfFalse => format!("-> {}", instr.a),
            _ => String::new(),
        };
        let _ = if operand.is_empty() {
            writeln!(out, "{index:04}  {}", instr.op.mnemonic())
        } else {
            writeln!(out, "{index:04}  {:<16}{operand}", instr.op.mnemonic())
        };
    }
    out
}

/// `Callable` is the VM's dispatch decision, made after the compiler has
/// already confirmed the name exists. It lives beside the opcode because it is
/// the meaning of `Call`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Callable {
    Host(usize),
    Builtin(Builtin),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Ast;
    use crate::lexer::lex;

    fn lowered(source: &str, engine: &crate::Engine) -> (Chunk, Ast) {
        let tokens = lex(source).expect("lexed");
        let mut ast = Ast::new();
        ast.parse_into(source, &tokens).expect("parsed");
        let mut chunk = Chunk::new();
        let mut tasks = Vec::new();
        compile(&ast, source, engine, &mut chunk, &mut tasks).expect("lowered");
        (chunk, ast)
    }

    fn ops(chunk: &Chunk) -> Vec<OpCode> {
        chunk.code.iter().map(|instr| instr.op).collect()
    }

    #[test]
    fn arithmetic_lowers_in_post_order() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("1 + 2 * 3", &engine);
        assert_eq!(
            ops(&chunk),
            vec![
                OpCode::PushNumber,
                OpCode::PushNumber,
                OpCode::PushNumber,
                OpCode::Mul,
                OpCode::Add,
                OpCode::Ret
            ]
        );
    }

    #[test]
    fn precedence_shows_up_as_instruction_order() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("(1 + 2) * 3", &engine);
        assert_eq!(
            ops(&chunk),
            vec![
                OpCode::PushNumber,
                OpCode::PushNumber,
                OpCode::Add,
                OpCode::PushNumber,
                OpCode::Mul,
                OpCode::Ret
            ]
        );
    }

    #[test]
    fn a_let_stores_without_popping_and_non_final_statements_pop() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("let x = 1\nx", &engine);
        assert_eq!(
            ops(&chunk),
            vec![
                OpCode::PushNumber,
                OpCode::Store,
                OpCode::Pop,
                OpCode::Load,
                OpCode::Ret
            ]
        );
        assert_eq!(chunk.name(chunk.code[1]), "x");
    }

    #[test]
    fn a_call_carries_its_name_and_argument_count() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("clamp(12, 0, 10)", &engine);
        let call = chunk
            .code
            .iter()
            .find(|instr| instr.op == OpCode::Call)
            .expect("one call");
        assert_eq!(chunk.name(*call), "clamp");
        assert_eq!(call.c, 3);
        let position = chunk
            .code
            .iter()
            .position(|instr| instr.op == OpCode::Call)
            .unwrap();
        assert_eq!(position, 3, "three pushes and no other instruction");
    }

    #[test]
    fn a_qualified_name_is_interned_with_its_dot() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("math.answer", &engine);
        let load = chunk.code[0];
        assert_eq!(load.op, OpCode::Load);
        assert_eq!(chunk.name(load), "math.answer");
    }

    #[test]
    fn an_empty_program_still_returns_a_value() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("", &engine);
        assert_eq!(ops(&chunk), vec![OpCode::PushUndef, OpCode::Ret]);
    }

    #[test]
    fn a_unknown_call_is_refused_before_its_arguments_are_lowered() {
        let engine = crate::Engine::new();
        let tokens = lex("nope(1 / 0)").unwrap();
        let mut ast = Ast::new();
        ast.parse_into("nope(1 / 0)", &tokens).unwrap();
        let mut chunk = Chunk::new();
        let mut tasks = Vec::new();
        let error = compile(&ast, "nope(1 / 0)", &engine, &mut chunk, &mut tasks).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Runtime);
        assert_eq!(
            error.message,
            "unknown function 'nope' (no host function or builtin has that name)"
        );
    }

    #[test]
    fn a_qualified_builtin_resolves() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("json.stringify(1)", &engine);
        let call = chunk
            .code
            .iter()
            .find(|instr| instr.op == OpCode::Call)
            .expect("one call");
        assert_eq!(chunk.name(*call), "json.stringify");
        assert_eq!(call.c, 1);
    }

    #[test]
    fn stack_effects_describe_the_fixed_instructions() {
        assert_eq!(OpCode::PushNumber.stack_effect(), Some((0, 1)));
        assert_eq!(OpCode::PushFolded.stack_effect(), Some((0, 1)));
        assert_eq!(OpCode::Add.stack_effect(), Some((2, 1)));
        assert_eq!(OpCode::Store.stack_effect(), Some((1, 1)));
        assert_eq!(OpCode::JumpIfFalse.stack_effect(), Some((1, 0)));
        assert_eq!(OpCode::Call.stack_effect(), None);
        assert_eq!(OpCode::Ret.stack_effect(), None);
    }

    #[test]
    fn the_opcode_table_names_every_opcode_in_discriminant_order() {
        for (index, op) in OPCODES.iter().copied().enumerate() {
            assert_eq!(
                op as usize,
                index,
                "{} sits at {}",
                op.mnemonic(),
                op as usize
            );
            assert_eq!(opcode_name(index as u32), Some(op.mnemonic()));
        }
        assert_eq!(opcode_name(OPCODE_COUNT as u32), None);
        assert_eq!(opcode_name(u32::MAX), None);
    }

    #[test]
    fn disassembly_names_instructions_and_operands() {
        let engine = crate::Engine::new();
        let (chunk, _) = lowered("let x = 40 + 2", &engine);
        let text = disassemble(&chunk, "let x = 40 + 2");
        assert!(text.contains("PUSH_NUMBER"), "{text}");
        assert!(text.contains("STORE"), "{text}");
        assert!(text.contains("ADD"), "{text}");
        assert!(text.contains("RET"), "{text}");
        assert!(text.contains('x'), "{text}");
    }
}
