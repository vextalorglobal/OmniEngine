//! OmniEngine's JIT tier: hot bytecode chunks become native machine code.
//!
//! ## What compiles
//!
//! The tier accepts a strict subset of the bytecode — the straight-line
//! arithmetic core: constant pushes, `+ - * /`, `== !=`, unary minus, `Pop`,
//! and `Ret`. Every other instruction (calls, loads, stores, strings, imports,
//! jumps — and `%`, see below) makes the whole chunk ineligible, because those
//! instructions are the ones that allocate, touch the arena, or branch; a
//! chunk containing any of them runs on the VM unchanged. Nothing is compiled
//! selectively.
//!
//! `%` is excluded deliberately: Cranelift 0.121 has no `frem` instruction,
//! and the exact `fmod` semantics the VM promises would need a libcall — a
//! relocation, which the tier's leaf-function contract rules out. The VM owns
//! `%` until a host-native remainder lands in Cranelift.
//!
//! ## How errors keep their positions
//!
//! The compiled function implements only the happy path. Any condition the VM
//! would answer with a positioned error — a non-number operand, division by
//! zero — branches to a deopt exit instead of raising. The caller sees
//! [`Jit::run`] return `Err(Deopt)` and re-runs the **same chunk** through
//! [`Engine::run_compiled`](omni_core::Engine::run_compiled), which produces
//! the canonical error, position included, with no message duplicated between
//! the two tiers. Deopt is the only error path out of native code; a chunk the
//! tier accepts either returns a value or deopts, it cannot fail on its own.
//!
//! ## Bit-for-bit parity
//!
//! The compiled code implements the VM's exact value semantics:
//! [`Value::from_f64`](omni_core::Value::from_f64) NaN canonicalization (any
//! NaN collapses to the canonical quiet NaN), the pointer-row and singleton-row
//! bit tests behind `is_double`, IEEE equality (`NaN != NaN`, `0.0 == -0.0`),
//! and the singleton tags for `nil`, `true`, `false`, `undef`. Constants are
//! folded at compile time, exactly as the VM would box them.
//!
//! ## The machine-code contract
//!
//! The function is a leaf: no calls, no relocations, no imports. Cranelift
//! emits it as `() -> I64` with no parameters; the return is the value's bits,
//! and a deopt is the same channel carrying a sentinel bit pattern
//! ([`DEOPT_BITS`]) the tier can never produce as a value — the canonical-NaN
//! row with a payload outside both the singleton slots and the canonical
//! quiet NaN, which `Value::from_f64` collapses to slot 0. A single `i64`
//! return lands in `rax` on SysV and Win64 x86-64 alike and in `x0` on
//! aarch64, so the entry point transmutes to `unsafe extern "C" fn() -> u64`
//! with no ABI variance across hosts. (A two-register `u128` return was
//! measured and rejected: this host's toolchain returns `u128` in `xmm0`,
//! while Cranelift hands back register pairs.) The executable memory is
//! allocated, written, and released by [`exec_mem`], which is hand-rolled per
//! platform rather than pulled from a JIT-linker crate, because a leaf
//! function has nothing to link.

#![deny(unsafe_op_in_unsafe_fn)]

mod exec_mem;

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block, Function, InstBuilder, MemFlags, UserFuncName, Value as ClValue, types,
};
use cranelift_codegen::isa::{CallConv, OwnedTargetIsa, TargetIsa};
use cranelift_codegen::settings;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use omni_core::ast::Ast;
use omni_core::bytecode::{self, Chunk, Instr, OpCode};
use omni_core::optimizer::optimize_chunk;
use omni_core::{Engine, OmniError, Value, lex_into};

/// One compiled chunk: an executable allocation and its entry point.
///
/// The allocation outlives every call through `entry`, and the code it holds
/// is immutable once written, so a `Jit` may move to another thread and be
/// called from there.
pub struct Jit {
    entry: unsafe extern "C" fn() -> u64,
    // Declared last so it is freed last: `entry` must never outlive the code.
    _code: exec_mem::ExecMem,
}

impl Jit {
    /// Compile `chunk` for the host machine, or return `None` when the chunk
    /// is not in the JIT-safe subset, exceeds the stack-depth bound, or the
    /// host ISA cannot be targeted. `source` is the text the chunk's literal
    /// spans address; the tier reads it exactly as the VM would.
    pub fn compile(chunk: &Chunk, source: &str) -> Option<Jit> {
        let isa = build_host_isa()?;
        let bytes = compile_bytes(chunk, source, &*isa)?;
        let mem = exec_mem::ExecMem::new(&bytes)?;
        // Safety: the allocation holds machine code produced for the host ISA,
        // and `mem` (stored in the returned struct) keeps it alive and mapped
        // for as long as `entry` can be called.
        let entry = unsafe {
            std::mem::transmute::<*const u8, unsafe extern "C" fn() -> u64>(mem.as_ptr())
        };
        Some(Jit { entry, _code: mem })
    }

    /// Run the compiled code.
    ///
    /// `Err(Deopt)` means execution reached a condition only the VM may
    /// answer; the caller re-runs the same chunk through
    /// [`Engine::run_compiled`](omni_core::Engine::run_compiled) to get the
    /// canonical positioned error.
    pub fn run(&self) -> Result<Value, Deopt> {
        // Safety: `entry` points at the executable allocation held alive by
        // this struct, containing code produced for the host ISA.
        let bits = unsafe { (self.entry)() };
        if bits == DEOPT_BITS {
            return Err(Deopt);
        }
        Ok(unbox(bits))
    }
}

/// The bits a deopt returns: the canonical-NaN row (top 32 bits
/// `0x7FF8_0000`) with a low-word payload of `0xFF`. The tier can never
/// produce it as a value — singleton tags are slots `1..=4`, `from_f64`
/// collapses every NaN to slot 0, and no real double carries a NaN exponent —
/// so it is unambiguous on the single-`i64` return channel.
const DEOPT_BITS: u64 = 0x7FF8_0000_0000_00FF;

/// The native code reached a condition only the VM may answer. See
/// [`Jit::run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deopt;

/// Compile `chunk` into machine-code bytes for `isa`.
///
/// Hidden because it exists for the cross-target smoke test and the JIT
/// itself; embedders compile through [`Jit::compile`].
#[doc(hidden)]
pub fn compile_bytes(chunk: &Chunk, source: &str, isa: &dyn TargetIsa) -> Option<Vec<u8>> {
    let func = build_function(chunk, source)?;
    let mut ctx = Context::for_function(func);
    ctx.compile(isa, &mut cranelift_control::ControlPlane::default())
        .ok()?;
    let code = ctx.compiled_code()?;
    let bytes = code.code_buffer();
    if bytes.is_empty() {
        return None;
    }
    Some(bytes.to_vec())
}

/// Run `source` through the JIT tier when the program qualifies, and through
/// the VM otherwise: on a JIT-safe chunk a deopt re-runs the same chunk via
/// [`Engine::run_compiled`], so every error is the canonical VM error.
///
/// `optimize` runs the bytecode optimizer before lowering, mirroring the
/// engine's own flag. This is the tier's door, not a replacement for
/// [`Engine::exec`], and it deliberately does not do the engine's
/// bookkeeping: no execution counter, no timing, no arena reclamation (the
/// JIT path allocates nothing), and no step budget (a straight-line chunk
/// cannot loop, so its cost is bounded by its length).
pub fn exec_with(engine: &mut Engine, source: &str, optimize: bool) -> Result<Value, OmniError> {
    let mut tokens = Vec::new();
    lex_into(source, &mut tokens)?;
    let mut ast = Ast::new();
    ast.parse_into(source, &tokens)?;
    let mut chunk = Chunk::new();
    let mut tasks = Vec::new();
    bytecode::compile(&ast, source, engine, &mut chunk, &mut tasks)?;
    if optimize {
        optimize_chunk(&mut chunk, source);
    }
    match Jit::compile(&chunk, source) {
        Some(jit) => match jit.run() {
            Ok(value) => Ok(value),
            Err(Deopt) => engine.run_compiled(&chunk, source),
        },
        None => engine.run_compiled(&chunk, source),
    }
}

/// The opcodes the tier compiles. Everything else — anything that branches,
/// allocates, or names an identifier — disqualifies the whole chunk. `Mod` is
/// out because Cranelift 0.121 has no `frem`, and emulating `fmod` exactly
/// would take a libcall, breaking the leaf-function contract (see the module
/// docs).
fn is_safe(op: OpCode) -> bool {
    matches!(
        op,
        OpCode::PushNumber
            | OpCode::PushFolded
            | OpCode::PushNil
            | OpCode::PushUndef
            | OpCode::Add
            | OpCode::Sub
            | OpCode::Mul
            | OpCode::Div
            | OpCode::Eq
            | OpCode::Ne
            | OpCode::Neg
            | OpCode::Pop
            | OpCode::Ret
    )
}

/// The span an instruction addresses, read the way the VM reads it: through
/// `str::get`, so a span that lands mid-character or past the end is empty
/// rather than a panic.
fn span(source: &str, instr: Instr) -> &str {
    source
        .get(instr.a as usize..(instr.a + instr.b) as usize)
        .unwrap_or("")
}

fn build_host_isa() -> Option<OwnedTargetIsa> {
    let builder = cranelift_native::builder().ok()?;
    let flags = settings::Flags::new(settings::builder());
    builder.finish(flags).ok()
}

/// Lower a JIT-safe chunk to a parameterless Cranelift function returning one
/// `I64`: the value's bits, or [`DEOPT_BITS`] from the deopt block. `None`
/// refuses the chunk: an unsafe opcode, a constant whose text does not parse,
/// a stack that would overflow [`omni_core::MAX_STACK_DEPTH`] or underflow —
/// every case the VM would answer with an error, which is exactly the tier's
/// fallback.
///
/// The operand stack is a compile-time SSA mirror: each stack slot is one
/// Cranelift value holding the boxed bit pattern, so the generated code has
/// no stack array and cannot overflow one. Any condition the VM would error
/// on branches to a deopt block that returns the sentinel; the last
/// statement's value leaves through its own bits.
fn build_function(chunk: &Chunk, source: &str) -> Option<Function> {
    if chunk.code.iter().any(|instr| !is_safe(instr.op)) {
        return None;
    }
    let mut sig = cranelift_codegen::ir::Signature::new(CallConv::SystemV);
    sig.returns.push(AbiParam::new(types::I64));
    let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut func, &mut builder_ctx);

    // The first block switched to is the function's entry, and a block must
    // be filled before switching away from it — so the entry is claimed
    // first, the body (which ends every path in a terminator) is emitted
    // next, and the deopt block is filled last.
    let entry = builder.create_block();
    builder.switch_to_block(entry);
    builder.seal_block(entry);
    let deopt = builder.create_block();

    let mut stack: Vec<ClValue> = Vec::new();
    let mut returned = false;
    for instr in chunk.code.iter().copied() {
        match instr.op {
            OpCode::PushNumber => {
                // A span that does not parse as a number is the VM's error to
                // raise; refusing sends the chunk there.
                let value = span(source, instr).parse::<f64>().ok()?;
                let boxed = iconst_bits(&mut builder, Value::from_f64(value).bits());
                push(&mut stack, boxed)?;
            }
            OpCode::PushFolded => {
                let bits = u64::from(instr.a) | (u64::from(instr.b) << 32);
                let boxed = iconst_bits(&mut builder, bits);
                push(&mut stack, boxed)?;
            }
            OpCode::PushNil => {
                let boxed = iconst_bits(&mut builder, Value::NIL.bits());
                push(&mut stack, boxed)?;
            }
            OpCode::PushUndef => {
                let boxed = iconst_bits(&mut builder, Value::UNDEF.bits());
                push(&mut stack, boxed)?;
            }
            OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div => {
                let rhs = stack.pop()?;
                let lhs = stack.pop()?;
                require_number(&mut builder, lhs, deopt);
                require_number(&mut builder, rhs, deopt);
                let lhs_f = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
                let rhs_f = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
                if instr.op == OpCode::Div {
                    // The VM raises here; the JIT deopts and the VM raises on
                    // the rerun, so the zero check only needs to be exact
                    // (`-0.0` is zero under IEEE equality, as in the VM).
                    let zero_f = builder.ins().f64const(0.0);
                    let is_zero = builder.ins().fcmp(FloatCC::Equal, rhs_f, zero_f);
                    let cont = builder.create_block();
                    builder.ins().brif(is_zero, deopt, &[], cont, &[]);
                    builder.switch_to_block(cont);
                    builder.seal_block(cont);
                }
                let result = match instr.op {
                    OpCode::Add => builder.ins().fadd(lhs_f, rhs_f),
                    OpCode::Sub => builder.ins().fsub(lhs_f, rhs_f),
                    OpCode::Mul => builder.ins().fmul(lhs_f, rhs_f),
                    _ => builder.ins().fdiv(lhs_f, rhs_f),
                };
                stack.push(canonicalize(&mut builder, result));
            }
            OpCode::Eq | OpCode::Ne => {
                let rhs = stack.pop()?;
                let lhs = stack.pop()?;
                // `Engine::eq_values` for everything this tier can produce:
                // two numbers compare by IEEE, two tagged values by bits, and
                // a mixed pair is false. (Strings cannot appear in a
                // JIT-safe chunk, so the content-comparison arm is moot.)
                let lhs_num = is_number(&mut builder, lhs);
                let rhs_num = is_number(&mut builder, rhs);
                let lhs_f = builder.ins().bitcast(types::F64, MemFlags::new(), lhs);
                let rhs_f = builder.ins().bitcast(types::F64, MemFlags::new(), rhs);
                let f_eq = builder.ins().fcmp(FloatCC::Equal, lhs_f, rhs_f);
                let both_num = builder.ins().band(lhs_num, rhs_num);
                let num_eq = builder.ins().band(both_num, f_eq);
                let bits_eq = builder.ins().icmp(IntCC::Equal, lhs, rhs);
                let not_lhs_num = not(&mut builder, lhs_num);
                let not_rhs_num = not(&mut builder, rhs_num);
                let both_tagged = builder.ins().band(not_lhs_num, not_rhs_num);
                let tagged_eq = builder.ins().band(both_tagged, bits_eq);
                let eq = builder.ins().bor(num_eq, tagged_eq);
                let true_bits = builder.ins().iconst(types::I64, Value::TRUE.bits() as i64);
                let false_bits = builder.ins().iconst(types::I64, Value::FALSE.bits() as i64);
                let result = match instr.op {
                    OpCode::Eq => builder.ins().select(eq, true_bits, false_bits),
                    _ => builder.ins().select(eq, false_bits, true_bits),
                };
                stack.push(result);
            }
            OpCode::Neg => {
                let value = stack.pop()?;
                require_number(&mut builder, value, deopt);
                let f = builder.ins().bitcast(types::F64, MemFlags::new(), value);
                let negated = builder.ins().fneg(f);
                stack.push(canonicalize(&mut builder, negated));
            }
            OpCode::Pop => {
                stack.pop()?;
            }
            OpCode::Ret => {
                let top = stack
                    .pop()
                    .unwrap_or_else(|| iconst_bits(&mut builder, Value::UNDEF.bits()));
                builder.ins().return_(&[top]);
                returned = true;
                break;
            }
            // The subset check above made these unreachable; the match stays
            // exhaustive for when the opcode set grows.
            OpCode::Mod
            | OpCode::PushStr
            | OpCode::Load
            | OpCode::Store
            | OpCode::Jump
            | OpCode::JumpIfFalse
            | OpCode::Call
            | OpCode::Import => return None,
        }
    }
    if !returned {
        // A Ret-less stream ends with its top of stack, like the VM.
        let top = stack
            .pop()
            .unwrap_or_else(|| iconst_bits(&mut builder, Value::UNDEF.bits()));
        builder.ins().return_(&[top]);
    }
    // Every body path ended in a terminator, so the deopt block is filled
    // only now; its branches from the body already exist.
    builder.switch_to_block(deopt);
    let sentinel = builder.ins().iconst(types::I64, DEOPT_BITS as i64);
    builder.ins().return_(&[sentinel]);
    builder.seal_block(deopt);
    builder.finalize();
    Some(func)
}

fn iconst_bits(builder: &mut FunctionBuilder, bits: u64) -> ClValue {
    builder.ins().iconst(types::I64, bits as i64)
}

fn push(stack: &mut Vec<ClValue>, value: ClValue) -> Option<()> {
    if stack.len() >= omni_core::MAX_STACK_DEPTH {
        return None;
    }
    stack.push(value);
    Some(())
}

/// Branch to `deopt` when `value` is not a number, mirroring the VM's
/// `is_double` classification: not in the pointer row (`bits >> 48 == 0x7FF7`)
/// and not a singleton slot in the canonical-NaN row
/// (`bits >> 32 == 0x7FF8_0000` with a low word in `1..16`).
///
/// The branch terminates the current block; emission continues in a fresh
/// continuation block this helper switches to.
fn require_number(builder: &mut FunctionBuilder, value: ClValue, deopt: Block) {
    let num = is_number(builder, value);
    let cont = builder.create_block();
    builder.ins().brif(num, cont, &[], deopt, &[]);
    builder.switch_to_block(cont);
    builder.seal_block(cont);
}

/// A boolean (I8) that is 1 exactly when `value`'s bits classify as a number.
fn is_number(builder: &mut FunctionBuilder, value: ClValue) -> ClValue {
    let high = builder.ins().ushr_imm(value, 48);
    let pointer_row = builder.ins().icmp_imm(IntCC::Equal, high, 0x7FF7);
    let row = builder.ins().ushr_imm(value, 32);
    let nan_row = builder.ins().icmp_imm(IntCC::Equal, row, 0x7FF8_0000);
    let low = builder.ins().ireduce(types::I32, value);
    let low_nonzero = builder.ins().icmp_imm(IntCC::UnsignedGreaterThan, low, 0);
    let low_in_slots = builder.ins().icmp_imm(IntCC::UnsignedLessThan, low, 16);
    let in_nan_row = builder.ins().band(nan_row, low_nonzero);
    let singleton = builder.ins().band(in_nan_row, low_in_slots);
    let tagged = builder.ins().bor(pointer_row, singleton);
    builder.ins().icmp_imm(IntCC::Equal, tagged, 0)
}

fn not(builder: &mut FunctionBuilder, value: ClValue) -> ClValue {
    builder.ins().icmp_imm(IntCC::Equal, value, 0)
}

/// Box a float result the way [`Value::from_f64`](omni_core::Value::from_f64)
/// does: any NaN collapses to the canonical quiet NaN, everything else keeps
/// its exact bits. The self-compare is IEEE's NaN test; it is false for every
/// value a NaN is not.
fn canonicalize(builder: &mut FunctionBuilder, value: ClValue) -> ClValue {
    let is_nan = builder.ins().fcmp(FloatCC::NotEqual, value, value);
    let qnan = builder
        .ins()
        .iconst(types::I64, Value::from_f64(f64::NAN).bits() as i64);
    let bits = builder.ins().bitcast(types::I64, MemFlags::new(), value);
    builder.ins().select(is_nan, qnan, bits)
}

/// Box the bit pattern the native code returned. The compiled code only ever
/// produces the four singleton tags and canonical doubles, so the singleton
/// test cannot misfire on a number and the double fallback cannot collide
/// with a tag.
fn unbox(bits: u64) -> Value {
    match bits {
        b if b == Value::NIL.bits() => Value::NIL,
        b if b == Value::TRUE.bits() => Value::TRUE,
        b if b == Value::FALSE.bits() => Value::FALSE,
        b if b == Value::UNDEF.bits() => Value::UNDEF,
        _ => Value::from_f64(f64::from_bits(bits)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_number(chunk: &mut Chunk, text: &str, offset: usize) {
        chunk.code.push(Instr {
            op: OpCode::PushNumber,
            a: offset as u32,
            b: text.len() as u32,
            c: 0,
            line: 1,
            column: 1,
        });
    }

    fn push_op(chunk: &mut Chunk, op: OpCode) {
        chunk.code.push(Instr {
            op,
            a: 0,
            b: 0,
            c: 0,
            line: 1,
            column: 1,
        });
    }

    fn run_chunk(source: &str, chunk: &Chunk) -> (Option<Value>, Option<Deopt>) {
        match Jit::compile(chunk, source) {
            Some(jit) => match jit.run() {
                Ok(value) => (Some(value), None),
                Err(deopt) => (None, Some(deopt)),
            },
            None => (None, None),
        }
    }

    #[test]
    fn arithmetic_matches_the_vm() {
        // spans: 21 at 0..2, 2 at 3..4
        let source = "21 2";
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "21", 0);
        push_number(&mut chunk, "2", 3);
        push_op(&mut chunk, OpCode::Mul);
        push_op(&mut chunk, OpCode::Ret);
        let (value, deopt) = run_chunk(source, &chunk);
        assert!(deopt.is_none());
        assert_eq!(value.unwrap().as_f64(), Some(42.0));
        let mut engine = Engine::new();
        assert_eq!(
            engine.run_compiled(&chunk, source).unwrap().as_f64(),
            Some(42.0)
        );
    }

    #[test]
    fn division_by_zero_deopts_instead_of_erroring() {
        let source = "1 0";
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "1", 0);
        push_number(&mut chunk, "0", 2);
        push_op(&mut chunk, OpCode::Div);
        push_op(&mut chunk, OpCode::Ret);
        let (value, deopt) = run_chunk(source, &chunk);
        assert!(value.is_none());
        assert!(deopt.is_some());
        // The rerun through the VM is the canonical positioned error.
        let mut engine = Engine::new();
        let error = engine.run_compiled(&chunk, source).unwrap_err();
        assert_eq!(error.message, "division by zero");
    }

    #[test]
    fn a_non_number_operand_deopts() {
        let source = "1";
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "1", 0);
        push_op(&mut chunk, OpCode::PushNil);
        push_op(&mut chunk, OpCode::Add);
        push_op(&mut chunk, OpCode::Ret);
        let (value, deopt) = run_chunk(source, &chunk);
        assert!(value.is_none());
        assert!(deopt.is_some());
    }

    #[test]
    fn singletons_and_canonical_nan_round_trip() {
        for op in [OpCode::PushNil, OpCode::PushUndef] {
            let mut chunk = Chunk::new();
            push_op(&mut chunk, op);
            push_op(&mut chunk, OpCode::Ret);
            let (value, deopt) = run_chunk("", &chunk);
            assert!(deopt.is_none());
            let expected = if op == OpCode::PushNil {
                Value::NIL
            } else {
                Value::UNDEF
            };
            assert_eq!(value.unwrap().bits(), expected.bits());
        }
        // 0.0 / 0.0 is the VM's error, not a value: the tier must deopt and
        // let the VM raise it.
        let source = "0.0 0.0";
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "0.0", 0);
        push_number(&mut chunk, "0.0", 4);
        push_op(&mut chunk, OpCode::Div);
        push_op(&mut chunk, OpCode::Ret);
        let (value, deopt) = run_chunk(source, &chunk);
        assert!(value.is_none());
        assert!(deopt.is_some());
        let mut engine = Engine::new();
        let error = engine.run_compiled(&chunk, source).unwrap_err();
        assert_eq!(error.message, "division by zero");

        // A NaN that survives arithmetic — (1e308 * 10) - (1e308 * 10), both
        // sides +inf — must come back as the canonical quiet NaN, exactly as
        // from_f64 boxes it.
        let source = "1e308 10 1e308 10";
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "1e308", 0);
        push_number(&mut chunk, "10", 6);
        push_op(&mut chunk, OpCode::Mul);
        push_number(&mut chunk, "1e308", 9);
        push_number(&mut chunk, "10", 15);
        push_op(&mut chunk, OpCode::Mul);
        push_op(&mut chunk, OpCode::Sub);
        push_op(&mut chunk, OpCode::Ret);
        let (value, deopt) = run_chunk(source, &chunk);
        assert!(deopt.is_none());
        assert_eq!(value.unwrap().bits(), Value::from_f64(f64::NAN).bits());
        let mut engine = Engine::new();
        assert_eq!(
            engine.run_compiled(&chunk, source).unwrap().bits(),
            Value::from_f64(f64::NAN).bits()
        );
    }

    #[test]
    fn equality_matches_the_vm_rules() {
        // 0.0 == -0.0 is true; both engines agree on every pairing here.
        let source = "0.0 -0.0 0.0 0.0";
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "0.0", 0);
        push_number(&mut chunk, "-0.0", 4);
        push_op(&mut chunk, OpCode::Eq); // 0.0 == -0.0 -> true
        push_number(&mut chunk, "0.0", 9);
        push_number(&mut chunk, "0.0", 13);
        push_op(&mut chunk, OpCode::Ne); // 0.0 != 0.0 -> false
        push_op(&mut chunk, OpCode::Eq); // true == false -> false
        push_op(&mut chunk, OpCode::Ret);
        let (value, deopt) = run_chunk(source, &chunk);
        assert!(deopt.is_none());
        assert_eq!(value.unwrap().bits(), Value::FALSE.bits());
        let mut engine = Engine::new();
        assert_eq!(
            engine.run_compiled(&chunk, source).unwrap().bits(),
            Value::FALSE.bits()
        );
    }

    #[test]
    fn unsafe_opcodes_refuse_the_whole_chunk() {
        for op in [OpCode::PushStr, OpCode::Mod, OpCode::Load, OpCode::Call] {
            let mut chunk = Chunk::new();
            push_number(&mut chunk, "1", 0);
            push_op(&mut chunk, op);
            push_op(&mut chunk, OpCode::Ret);
            assert!(Jit::compile(&chunk, "1").is_none(), "{op:?} must refuse");
        }
    }

    #[test]
    fn stack_limits_refuse_the_chunk() {
        let source = "1";
        let mut chunk = Chunk::new();
        for _ in 0..=omni_core::MAX_STACK_DEPTH {
            push_number(&mut chunk, "1", 0);
        }
        push_op(&mut chunk, OpCode::Ret);
        assert!(Jit::compile(&chunk, source).is_none());

        let mut chunk = Chunk::new();
        push_op(&mut chunk, OpCode::Pop);
        push_op(&mut chunk, OpCode::Ret);
        assert!(Jit::compile(&chunk, "").is_none());
    }

    #[test]
    fn an_unparsable_constant_refuses_the_chunk() {
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "not a number", 0);
        push_op(&mut chunk, OpCode::Ret);
        assert!(Jit::compile(&chunk, "not a number").is_none());
    }

    #[test]
    fn a_ret_less_stream_returns_its_top_of_stack() {
        let source = "7";
        let mut chunk = Chunk::new();
        push_number(&mut chunk, "7", 0);
        let (value, deopt) = run_chunk(source, &chunk);
        assert!(deopt.is_none());
        assert_eq!(value.unwrap().as_f64(), Some(7.0));
    }

    #[test]
    fn the_empty_chunk_is_undefined() {
        let chunk = Chunk::new();
        let (value, deopt) = run_chunk("", &chunk);
        assert!(deopt.is_none());
        assert_eq!(value.unwrap().bits(), Value::UNDEF.bits());
    }
}
