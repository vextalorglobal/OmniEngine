//! A semantics-preserving ahead-of-time pass over lowered bytecode.
//!
//! Runs once per execution, between [`crate::bytecode::compile`] and the VM,
//! and only on streams the compiler produced — never on hand-built chunks. Two
//! transformations, both keyed to one rule: **anything the pass removes or
//! rewrites must be provably unable to fail or to have a side effect**, so the
//! sequence of runtime errors, each with its line and column, is identical
//! with the pass on and off.
//!
//! ## Constant folding
//!
//! A linear scan mirrors the VM's operand stack, remembering which slots hold
//! compile-time-known numbers. When an arithmetic operator's operands are both
//! constants, the three instructions collapse into one [`OpCode::PushFolded`],
//! which carries the `f64`'s bits in its operands and cannot fail at runtime —
//! there is no source text left to parse. `Div` and `Mod` only fold when the
//! divisor is nonzero, because "division by zero" and "remainder by zero" are
//! runtime errors that must keep firing at their own positions.
//!
//! ## Dead code elimination
//!
//! A non-final statement whose whole computation is one constant push followed
//! by the statement-separating `Pop` is erased. A string literal qualifies
//! only after its escapes decode at compile time, so a bad escape keeps its
//! runtime error. Calls, loads, stores, and imports are never touched.
//!
//! Positions are never rewritten: folded constants inherit the operator's
//! position, and every surviving instruction keeps its own `line`/`column`.

use crate::bytecode::{Chunk, Instr, OpCode};
use crate::engine::{Pos, decode_literal, literal_span};
use crate::lexer::BinOp;

/// What the most recent optimization pass did, for tests and for hosts that
/// want the number rather than a disassembly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OptimizeStats {
    /// Arithmetic operations replaced by a folded constant.
    pub folded: usize,
    /// Instructions removed by dead code elimination.
    pub elided: usize,
}

/// The pass and its scratch buffers, owned by the engine and recycled the way
/// every other engine buffer is: a warm execution asks the system allocator
/// for nothing, because the rewrite buffers keep the capacity of the largest
/// program the engine has compiled.
///
/// Two buffers serve both passes — the folded stream is rewritten into `code`
/// and swapped in, then the dead-code pass rewrites *that* into `code` again.
/// A swap hands the freshly compiled stream back to the optimizer for next
/// time, so its capacity is never lost either.
#[derive(Default)]
pub(crate) struct Optimizer {
    code: Vec<Instr>,
    consts: Vec<Option<Const>>,
}

impl Optimizer {
    /// Run both passes over `chunk`, rewriting it in place. `source` is needed
    /// to read literal spans. Returns the work done.
    pub(crate) fn optimize(&mut self, chunk: &mut Chunk, source: &str) -> OptimizeStats {
        let mut stats = OptimizeStats::default();
        fold_constants(
            &chunk.code,
            source,
            &mut self.code,
            &mut self.consts,
            &mut stats,
        );
        if stats.folded > 0 {
            std::mem::swap(&mut chunk.code, &mut self.code);
        }
        eliminate_dead_code(&chunk.code, source, &mut self.code, &mut stats);
        if stats.elided > 0 {
            std::mem::swap(&mut chunk.code, &mut self.code);
        }
        stats
    }
}

/// The compile-time value of one stack slot. Only numbers fold today: string
/// concatenation folding waits for a non-span literal encoding (see the
/// Phase 9 spec, "Deliberately deferred").
#[derive(Clone, Copy)]
enum Const {
    Num(f64),
}

/// Fold arithmetic over constant operands into `PushFolded` instructions.
///
/// Writes the rewritten stream into the caller's recycled `out` buffer; the
/// caller swaps it in when anything changed.
fn fold_constants(
    code: &[Instr],
    source: &str,
    out: &mut Vec<Instr>,
    consts: &mut Vec<Option<Const>>,
    stats: &mut OptimizeStats,
) {
    out.clear();
    consts.clear();
    // Mirrors the VM's operand stack: one entry per value currently on it,
    // `Some` when that value is a compile-time-known number. The tail is the
    // top of the stack.
    for instr in code.iter().copied() {
        match instr.op {
            OpCode::PushNumber => {
                let value = literal_span(source, instr).parse::<f64>().ok();
                out.push(instr);
                consts.push(value.map(Const::Num));
            }
            OpCode::PushFolded => {
                out.push(instr);
                consts.push(Some(Const::Num(folded_bits(instr))));
            }
            OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Mod => {
                let op = match instr.op {
                    OpCode::Add => BinOp::Add,
                    OpCode::Sub => BinOp::Sub,
                    OpCode::Mul => BinOp::Mul,
                    OpCode::Div => BinOp::Div,
                    _ => BinOp::Mod,
                };
                let rhs = consts.pop().and_then(|slot| slot);
                let lhs = consts.pop().and_then(|slot| slot);
                // The const mirror tracks the runtime stack; the tail check
                // proves the two entries really are the last two emitted
                // instructions, which is what `out.pop()` below relies on.
                let tail_is_pushes = out.len() >= 2
                    && is_constant_push(out.last())
                    && is_constant_push(out.get(out.len() - 2));
                match (lhs, rhs) {
                    (Some(Const::Num(a)), Some(Const::Num(b))) if tail_is_pushes => {
                        // `None` means the runtime would raise an error here
                        // (div/rem by zero); leave the stream alone so it does.
                        if let Some(value) = fold_arithmetic(a, op, b) {
                            out.pop();
                            out.pop();
                            out.push(push_folded(value, instr.pos()));
                            consts.push(Some(Const::Num(value)));
                            stats.folded += 1;
                            continue;
                        }
                    }
                    _ => {}
                }
                out.push(instr);
                consts.push(None);
            }
            OpCode::Neg => {
                match consts.pop().and_then(|slot| slot) {
                    Some(Const::Num(value)) if is_constant_push(out.last()) => {
                        out.pop();
                        out.push(push_folded(-value, instr.pos()));
                        consts.push(Some(Const::Num(-value)));
                        stats.folded += 1;
                        continue;
                    }
                    _ => consts.push(None),
                }
                out.push(instr);
            }
            OpCode::Pop => {
                out.push(instr);
                consts.pop();
            }
            OpCode::Store => {
                // `Store` pops its operand and pushes the value back, so the
                // slot's constancy survives it.
                let value = consts.pop().and_then(|slot| slot);
                out.push(instr);
                consts.push(value);
            }
            OpCode::Call => {
                out.push(instr);
                for _ in 0..instr.c as usize {
                    consts.pop();
                }
                consts.push(None);
            }
            OpCode::Eq | OpCode::Ne => {
                out.push(instr);
                consts.pop();
                consts.pop();
                consts.push(None);
            }
            OpCode::JumpIfFalse => {
                out.push(instr);
                consts.pop();
            }
            OpCode::Ret => {
                out.push(instr);
                consts.pop();
            }
            // Pushes of values the pass does not fold: mirrored on the const
            // stack, carrying no constant.
            OpCode::PushStr
            | OpCode::PushNil
            | OpCode::PushUndef
            | OpCode::Load
            | OpCode::Import => {
                out.push(instr);
                consts.push(None);
            }
            OpCode::Jump => out.push(instr),
        }
    }
}

/// Erase `push; Pop` pairs whose push is provably side-effect-free and
/// error-free: a pure constant statement whose value is discarded. Writes into
/// the caller's recycled `out` buffer; the caller swaps it in when anything
/// changed.
fn eliminate_dead_code(
    code: &[Instr],
    source: &str,
    out: &mut Vec<Instr>,
    stats: &mut OptimizeStats,
) {
    out.clear();
    let mut index = 0usize;
    while index < code.len() {
        let instr = code[index];
        if is_elidable(instr, source)
            && code
                .get(index + 1)
                .is_some_and(|next| next.op == OpCode::Pop)
        {
            index += 2;
            stats.elided += 2;
            continue;
        }
        out.push(instr);
        index += 1;
    }
}

fn is_constant_push(instr: Option<&Instr>) -> bool {
    instr.is_some_and(|instr| matches!(instr.op, OpCode::PushNumber | OpCode::PushFolded))
}

/// Can this instruction be erased together with the `Pop` that discards its
/// value? It must be a constant with no remaining way to fail.
fn is_elidable(instr: Instr, source: &str) -> bool {
    match instr.op {
        OpCode::PushNumber => literal_span(source, instr).parse::<f64>().is_ok(),
        OpCode::PushFolded | OpCode::PushNil | OpCode::PushUndef => true,
        // A literal whose escapes decode is a value with no error left in it;
        // one whose escapes fail must keep its runtime error.
        OpCode::PushStr => decode_literal(literal_span(source, instr), instr.pos()).is_ok(),
        _ => false,
    }
}

/// The runtime semantics of one arithmetic operator on two numbers, `None`
/// exactly when the runtime would raise an error instead of answering.
fn fold_arithmetic(a: f64, op: BinOp, b: f64) -> Option<f64> {
    Some(match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div if b == 0.0 => return None,
        BinOp::Mod if b == 0.0 => return None,
        BinOp::Div => a / b,
        BinOp::Mod => a % b,
    })
}

fn folded_bits(instr: Instr) -> f64 {
    f64::from_bits(u64::from(instr.a) | (u64::from(instr.b) << 32))
}

fn push_folded(value: f64, at: Pos) -> Instr {
    let bits = value.to_bits();
    Instr {
        op: OpCode::PushFolded,
        a: bits as u32,
        b: (bits >> 32) as u32,
        c: 0,
        line: at.line,
        column: at.column,
    }
}
