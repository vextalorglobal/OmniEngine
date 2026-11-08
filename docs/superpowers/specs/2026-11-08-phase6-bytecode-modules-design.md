# Phase 6 — Bytecode VM and Module System

**Status:** implemented in 0.1.0 (ABI 4, language version 2)
**Scope:** `omni-core`; no FFI signature changes
**Supersedes:** the fused parse-and-evaluate note in `engine.rs`

## 1. Why a VM now

Phases 1–5 shipped a working language by evaluating while parsing. That was the
right shape while the grammar was one expression per statement: no node type to
keep in sync, no second walk, and the warm path never allocated. It stops being
the right shape as soon as a program can be assembled from more than one file.
A module has to be parsed before it can be imported, its exports have to be
inspected after it runs, and a caller wants to see the instructions a program
lowered to instead of guessing from the source.

Phase 6 keeps the observable language identical and splits the engine into the
classic four stages:

```
source ──lex──▶ tokens ──parse──▶ flat AST ──lower──▶ Chunk ──execute──▶ value
```

The VM is the only executor. `omni_exec` and `omni_exec_many` call the same
`Engine::exec` / `Engine::exec_many` they always did, so every ABI 4 signature
and every SDK call site is untouched.

## 2. Constraints the design must honour

1. **No new heap traffic on the warm path.** Existing tests assert that the
   thousandth execution of a numeric program, a literal, a concatenation, and
   a batch asks the system allocator zero times. The AST, the chunk, the
   operand stack, and the name buffer are therefore owned by `Engine` and
   cleared rather than reallocated. They are taken out with `mem::take` for the
   duration of one execution so the borrow checker can see that the buffers
   and `&mut self` are disjoint, then put back on both the success and the
   error path.
2. **No per-node allocation.** The AST is a flat `Vec<Node>` with `u32`
   indices; children are indices, call arguments are a sibling chain. There is
   no `Box`, no `Rc`, and no child `Vec` per node.
3. **Diagnostics do not move.** Message text, error kind, and line/column are
   part of the public contract asserted by the Python and Node suites. The
   lowering pass reproduces them exactly; §6 is the table that pins each one.
4. **No collection value at ABI 4.** There is no array or object on the wire.
   Modules export scalar bindings, JSON accepts and produces scalars, and both
   say so instead of inventing a value the FFI cannot carry.
5. **Sandbox by default.** Imports are refused unless a module root is
   configured. File I/O is refused unless a root is granted. The FFI gains no
   new way to reach the filesystem.

## 3. The instruction set

`OpCode` is a stack machine. Every instruction has an implicit effect on the
value stack; `a`, `b`, `c`, and `d` are unsigned operands whose meaning is
listed below. Source positions travel in the instruction, so a runtime failure
deep in a lowered expression still points at the operator or name that caused
it.

| OpCode | Operands | Stack effect | Meaning |
| --- | --- | --- | --- |
| `PushNumber` | `a` = source offset, `b` = length | `[] → [n]` | Parse the decimal literal and push it. |
| `PushStr` | `a` = source offset, `b` = length | `[] → [s]` | Decode escapes, arena-allocate, push. |
| `PushNil` | — | `[] → [nil]` | Push `null`. |
| `PushUndef` | — | `[] → [undef]` | Push `undefined`. |
| `Load` | `a` = name offset in `names`, `b` = length | `[] → [v]` | Latest `let` binding, then host global. |
| `Store` | `a`, `b` = name | `[v] → [v]` | Bind `v` without popping, because `let x = 5` evaluates to `5`. |
| `Pop` | — | `[v] → []` | Discard the value of a non-final statement. |
| `Add` | — | `[l, r] → [l + r]` | Numeric add or string concatenation; the typed-`+` rule is unchanged. |
| `Sub` `Mul` `Div` `Mod` | — | `[l, r] → [l op r]` | Numeric only, with the existing zero-division errors. |
| `Eq` `Ne` | — | `[l, r] → [bool]` | Engine equality, which compares string contents. |
| `Neg` | — | `[v] → [-v]` | Unary minus; the operand position is the instruction's position. |
| `Jump` | `a` = target ip | `[] → []` | Unconditional relative-to-chunk absolute jump. |
| `JumpIfFalse` | `a` = target ip | `[v] → []` | Pop; jump when the value is falsy. |
| `Call` | `a`, `b` = name, `c` = argument count | `[arg…] → [r]` | Resolve host function first, then builtin; check arity *after* arguments are on the stack; invoke. |
| `Ret` | — | `[v] → [v]` | Return from the chunk. A chunk without a value returns `undefined`. |
| `Import` | `a` = source offset, `b` = length | `[] → [undef]` | Decode the path literal, load the module, bind `ns.export` names. |

`Jump`, `JumpIfFalse`, and `Ret` are exercised by hand-built chunks in the
bytecode tests. The Phase 6 surface grammar has no `if` or loop, so the
compiler does not synthesize branches yet; the instructions exist because the
VM contract is smaller than the language and the next grammar addition should
not have to change the VM.

`Call` carries the argument count rather than reading a sentinel, so the stack
slice handed to a host function is exactly the arguments of that call. A
builtin's arity is read from the same table the old parser used, and the
comparison happens after lowering — matching the old order, where the
arguments were evaluated before a mismatch was reported.

## 4. AST and lowering

```rust
enum NodeKind {
    Number, Str, Var,          // span in the source
    Member,                    // base and member identifiers
    Let,                       // name + value
    Import,                    // raw string-literal span
    UnaryNeg,
    Binary(BinOp),
    Call, CallMember,          // name (or base + member), argument chain
}

struct Node {
    kind: NodeKind,
    pos: Pos,                  // 1-based line and byte column
    start: u32, len: u32,      // primary span
    start2: u32, len2: u32,    // member span, when the node has one
    lhs: u32, rhs: u32,        // child indices
    next: u32,                 // next call argument
}
```

Statements are a `Vec<u32>` of roots. Arguments form a singly linked list
through `next`; no node owns a buffer. All indices use `u32::MAX` as `NONE`.

Lowering is a post-order walk:

- a literal emits `PushNumber` / `PushStr`;
- a variable emits `Load`;
- `base.member` emits `Load` with the name assembled as `base.member` in the
  chunk's reusable name buffer;
- `lhs op rhs` lowers both sides then emits the operator;
- unary minus lowers its operand then emits `Neg`;
- a call checks that the callee exists **before** lowering arguments (the old
  parser's order), tracks syntactic call nesting against `MAX_CALL_DEPTH`, then
  lowers each argument in order and emits `Call` with the count;
- `let` lowers the value and emits `Store`;
- `import` emits `Import`.

Every statement except the last is followed by `Pop`; the chunk ends with
`Ret`.

The call-depth check stays a compile-time check even though execution is now a
loop. The old parser incremented its counter while descending into arguments,
so `id(id(1))` consumed two frames. Post-order bytecode executes the *inner*
call first and would see one frame at a time; moving the counter to the VM
would silently remove the `calls are nested 64 deep` failure that the test
suite pins. Syntactic nesting is the property being limited, so it is measured
while lowering.

## 5. Execution

The VM is a loop over `chunk.code` with an `ip` and a reused `Vec<Value>`
stack. Arithmetic, equality, concatenation, truthiness, binding, and host calls
are the existing `Engine` helpers — the VM chooses *when* to apply them, it
does not reimplement them. This is deliberate: one `numeric`, one `concat`,
one `eq_values`, so the string lifetime rules and the divide-by-zero rule stay
stated once.

Each `Chunk` owns:

| field | reuse |
| --- | --- |
| `code: Vec<Instr>` | cleared per compile |
| `names: Vec<u8>` | cleared per compile; identifier bytes are copied here so instructions carry offsets, not `String`s |
| `numbers`, `disassembly` | test and diagnostic helpers only |

`disassemble(&Chunk) -> String` renders one line per instruction
(`0000  PUSH_NUMBER  40`) and is used by lowering tests and by anyone reading
a compilation. It allocates, so it is not on the execution path.

## 6. Diagnostic preservation

| Situation | Kind | Position | Message |
| --- | --- | --- | --- |
| division by zero | `Runtime` | operator | `division by zero` |
| remainder by zero | `Runtime` | operator | `remainder by zero` |
| undefined name | `Runtime` | name | `undefined variable 'x'` |
| unknown call | `Runtime` | call name | `unknown function 'f' (no host function or builtin has that name)` |
| wrong arity | `Runtime` | call name | `Mismatched arity in call 'f': takes N argument(s), got M` |
| call nesting | `Runtime` | offending call name | `calls are nested 64 deep, the limit; a host function cannot call itself` |
| too many arguments | `Parse` | 17th argument | `a call takes at most 16 arguments` |
| unary minus on a non-number | `Runtime` | operand | `type mismatch: cannot apply unary '-' to <kind>: <shown>` |
| string literal escapes | `Parse` | literal | unchanged |
| missing comma / paren | `Parse` | cursor | unchanged |

String literals stay raw token spans in the chunk and are decoded when the
`PushStr` instruction executes, exactly as the fused evaluator decoded them
while parsing that operand. Error *order* therefore stays the same: `1 / 0`
fails before a later bad escape is even looked at.

## 7. Modules

### 7.1 Syntax

```omni
import "math"
math.answer
```

`import` is a statement. Its operand is one string literal; anything else is
`import needs a string literal naming a module` at that token. The namespace is
the file stem: `lib/math` and `lib/math.omni` both produce `math`.

A module reference resolves to `<module_root>/<request>[.omni]`, is
canonicalized, and must start with the canonical module root. Absolute paths,
`..` escapes, and symlinks that leave the root are therefore refused by one
check rather than three. Imports are denied with
`import is disabled: this engine has no module root` unless
`Engine::set_module_root` was called.

### 7.2 Execution and visibility

Each module runs in a **fresh child engine**:

- it may use builtins, including the sandboxed `fs` and `json` namespaces;
- it inherits the parent's filesystem permission roots;
- it does **not** inherit the parent's host globals or host function
  registrations, so a module cannot call a callback the embedder never exposed
  to it;
- its bindings do not leak into the parent except through exports.

After the module executes, every top-level `let` whose name contains no dot and
does not start with `_` is exported. A leading underscore is the entire privacy
rule: `let _counter = 0` stays inside the module, `let answer = 42` becomes
`math.answer`. Names imported by the module itself are dotted and are therefore
not re-exported.

Values are copied into the importing execution: numbers, booleans, `null`, and
`undefined` by value; strings by copying the bytes out of the child engine's
arena before it is reclaimed. The parent binds each as `namespace.name` in its
current execution arena, so the usual rule applies — readable until the next
execution.

There are no function values at language version 2. A module can export data,
not behaviour. `math.answer` is a `Load`; `math.answer()` reports the ordinary
unknown-function error naming `math.answer`.

### 7.3 Cycles and limits

Import depth is limited to `MAX_MODULE_DEPTH = 16` and reported as
`imports are nested 16 deep, the limit`. The loader keeps the canonical path of
every module currently loading; importing one already on that stack is a
`Runtime` error, `circular import: a -> b -> a`. A module that was imported and
finished is not cached in Phase 6: importing it again executes it again. That
is a documented cost, not an accident, and it keeps exported strings owned by
the execution that used them.

Errors raised inside a module keep the module's line and column and gain a
prefix in the message: `in module 'math': undefined variable 'nope'`.

## 8. Sandboxed filesystem

`Engine::allow_fs_read(path)` and `Engine::allow_fs_write(path)` canonicalize a
directory and add it to the permission roots. Both return `std::io::Result`
because a root that does not exist is a host mistake with an OS error to show.
The default is no roots, so both builtins fail closed:

```
fs.read_text(path)          -> string
fs.write_text(path, text)   -> null
```

A read canonicalizes the target and requires it to be inside a read root. A
write canonicalizes the target's parent (the file may not exist yet) and
requires that parent to be inside a write root. A target outside every root is
`path '<p>' is outside the allowed read roots`, not an OS error, so the
diagnostic does not depend on the platform's wording. Reads above
`MAX_STRING_LEN` are refused before allocating; non-UTF-8 reads are refused
with a message that says so because every string the language carries is
UTF-8.

## 9. JSON at ABI 4

`json.parse` and `json.stringify` handle the four scalar kinds the ABI can
carry:

| JSON | OmniEngine |
| --- | --- |
| number | number (finite; overflow is an error) |
| string | string |
| `true` / `false` | boolean |
| `null` | `null` |

Arrays and objects are a **deliberate error**:

```
json.parse: arrays and objects are not supported at ABI 4; OmniEngine has no
collection value
```

Faking a collection as a string would make `json.parse` unusable for anything
but printing, and inventing an object value would break the 16-byte
`OmniValue` contract. `json.stringify(undefined)` is an error for the same
reason: JSON has no `undefined`, and dropping it silently would produce text
that parses back as a different program's value. Stringification escapes
quotes, backslashes, and control characters; non-ASCII UTF-8 passes through.

## 10. Tests

- `ast.rs`: statement shapes, precedence, argument chains, import nodes,
  qualified names, and each preserved parse error.
- `bytecode.rs`: lowering of each expression form, `Call` operand layout,
  name interning, stack-effect metadata, disassembly, and VM control flow
  (`Jump`, `JumpIfFalse`, `Ret`) on hand-built chunks.
- `engine.rs`: every pre-existing test unchanged, plus module resolution,
  namespace isolation, private exports, cycles, depth, fs permission allow and
  deny, JSON round trips, and the ABI-4 collection refusal.
- `scripts/verify-all.sh`: the same thirty-four rows, unchanged, because the
  FFI surface is unchanged.
- Python and Node suites unchanged.

## 11. Compatibility statement

- C ABI: version 4, unchanged structs and exports.
- SDKs: no signature changes; `eval`/`eval_str` keep returning native
  primitives.
- Language: `LANGUAGE_VERSION` becomes 2 because `import` is now a keyword.
  A program that used `import` as an identifier stops lexing; no other
  previously valid program changes meaning.
