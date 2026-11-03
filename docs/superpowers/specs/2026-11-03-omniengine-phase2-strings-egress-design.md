# OmniEngine Phase 2 — Strings, Egress Memory, ABI v2

Date: 2026-11-03
Status: approved by the requester, executed as written below
Supersedes: the "What Phase 2 is" list in `docs/GUIDE.md` section 10, items 1 and 2

## 0. What this phase is, and what it is not

Phase 1 shipped a C ABI, a Python `ctypes` SDK, and a Node `koffi` SDK that all
return one thing: a number. Phase 2 adds the second value type the bridge was
designed for — strings — and the memory model that lets a string cross a foreign
boundary without either a garbage collector or a use-after-free.

It is **not** a new bridge. The exports, the wrapper APIs, the discovery order,
and the panic boundary all exist and stay. This phase changes the shape of one
struct, adds four functions, and teaches four layers the same new type.

Requested, verbatim, as the requirements:

1. `omni-core`: string variant in the value layout, string literals with `\n`,
   `\t`, `\"`, `\\`, `\u{XXXX}` validated against lone surrogates, `+` concatenation
   with strict typed errors, content-based `==` and `!=`.
2. Egress memory: engine strings copied into an egress block that **rewinds**
   rather than frees, so a host-facing pointer never dangles; the execution arena
   stays bulk-freed per `exec`.
3. `omni-ffi`: `omni_abi_version`, `omni_str_copy`, `omni_define_string`,
   `omni_debug_stats`, all `#[unsafe(no_mangle)] extern "C"`.
4. SDKs: Python and Node handle string conversion and boundary copying.
5. Tests for parsing, boundary copying, type-mismatch messages, and ABI agreement.

Two shapes in the request were corrected before implementation, both recorded
here so the difference is a decision rather than a drift:

- `omni_define_string(engine, name, bytes, len)` became
  `omni_define_string(engine, name, name_len, bytes, bytes_len)`. Every other
  call in this ABI passes an explicit length for every buffer; a NUL-terminated
  `name` would be the one exception, and the one exception is the one that gets
  forgotten.
- `omni_debug_stats(engine, …)` became
  `omni_debug_stats(engine, out: *mut OmniStats) -> u32`, with `#[repr(C)]
  OmniStats` pinned by the same compile-time asserts as `OmniValue`. A stats
  struct whose layout nobody checks reports wrong numbers silently.

## 1. Value layout: the pointer row finally gets a tenant

`Value` stays a NaN-boxed eight bytes. No 16-byte `{ tag, len, payload }` box
enters the core: the request allowed either, and 8 bytes is what the FFI struct
and every stack copy already pay for. A length does not fit in the remaining 48
address bits next to a pointer, so it lives **with the string**, not in the box.

```
Value (u64)
  0x7FF7_0000_0000_0000 | 48-bit address   -> a Str, in one of the engine's blocks
  0x7FF8_0000_0000_000N (N in 1..=15)      -> nil / true / false / undefined
  anything else                            -> an f64
```

The string object, laid out once and shared by all three blocks:

```
offset 0   u32   len
offset 4   u32   zero  (a canary: reading it non-zero means type confusion)
offset 8   [u8; len]   UTF-8 bytes
offset 8+len  u8   0    (a terminator for convenience; bytes may contain NUL)
```

`Value::is_pointer()` is therefore `Value::is_string()`, and the doc comment says
so explicitly: the pointer row has exactly one tenant today, and adding a second
compound type means splitting the row with a tag nibble, not overloading it again.

Reading a string is unsafe, so the unsafe goes somewhere that can be audited, in
one place, the same way `Slot<T>` confines the arena's:

- `object.rs` owns `StrRef` — the only type that dereferences a `Value`'s address.
- `Engine::string_of(&self, v: Value) -> Option<&[u8]>` is the **safe** accessor,
  and it is safe for a reason: taking `&self` means no `exec` can run, and only
  `exec` resets or rewinds the blocks those bytes live in. The borrow checker is
  the enforcement, not a comment.

`Value`'s own `PartialEq` and `Display` deliberately do *not* dereference. Two
string boxes compare by identity, and `Display` prints `<string>`; language
equality is `==`, implemented by the engine, which compares content; and text
rendering that needs content goes through `Engine::render(&self, v)`. A `Display`
impl that followed a raw pointer would be an unsafe-in-disguise callable from
anywhere, including a `println!` in a panic handler.

## 2. Three blocks, three lifetimes

| Block | Contents | Reclaimed | Pointer may outlive it? |
| --- | --- | --- | --- |
| `arena` | `let` bindings, every string built during evaluation | `reset()` at the start of each `exec`: truncate to one warm chunk, free oversize chunks | no — and nothing points there after `exec` returns |
| `egress` | the promoted copy of the *result* string | `rewind()` at the start of each `exec`: `used = 0`, **every chunk retained** | yes, forever. The bytes stay mapped for the life of the handle |
| `host` | strings injected by `omni_define_string` | never, except when the same name is redefined or the engine is freed | yes |

Promotion is the pivot. `Engine::exec` copies the result string into `egress`
before returning, so the `Value` handed to the caller — and the one kept in
`last_value` — addresses egress, never the exec arena. That single rule is what
retires Phase 1's "no arena pointer crosses the boundary" and replaces it.

The consequence, stated plainly because it is the interesting part: **a stale
string is a wrong read, never a crash.** Rewind does not unmap anything, so an
`OmniValue` kept across executions still addresses live, owned memory with the
previous contents in it. The documented contract is "valid until your next
`omni_exec()` on that engine"; violating it loses bytes, not process stability.

Memory is bounded by the high-water mark of a single execution, not by the number
of executions — which is exactly what the 100,000-iteration leak test measures.

### 2.1 The arena had to change first

`Arena::alloc_raw` asserted `size + align <= chunk_size`, so a 128 KiB literal
would have **panicked**. That assert is now a branch: a request that cannot fit
the active chunk becomes its own exact-size chunk in a separate `oversize` list,
so the bump cursor's invariants over the normal chunks do not move and pointer
stability still holds. `reset()` frees them; `rewind()` does not.

`rewind()` is new. `reset()` is unchanged in meaning.

## 3. Grammar

New tokens: `Str` (the raw inner slice, quotes removed, escapes **not** decoded),
`EqEq`, `NotEq`.

- `"` scans to the closing `"`, honouring `\` as an escape-introducer; a newline
  or end of input before the closing quote is a `LexError` with line and column.
- `=` peeks one byte ahead; `==` is `EqEq`, and a lone `=` stays the binding
  operator. `!` is only ever the start of `!=`, so a bare `!` is a lex error
  naming the character rather than a confusing parse error later.
- Bytes, not chars, throughout — multi-byte UTF-8 inside a literal passes through
  untouched because `"` and `\` are ASCII and no continuation byte can imitate
  them. There are tests for it.

Escapes are decoded at evaluation, not at lexing, so a token stays a borrowed
slice and lexing allocates nothing. That defers the error, not the line number:
the token carries its line, and a bad escape is an `ErrorKind::Parse`, which the
ABI reports as `OMNI_ERR_COMPILE`. Decoded: `\n`, `\r`, `\t`, `\"`, `\\`, and
`\u{...}` with 1 to 6 hex digits. Anything else after `\` is a parse error that
quotes the offending character.

`\u{...}` accepts `0x00..=0xD7FF` and `0xE000..=0x10FFFF`. `0xD800..=0xDFFF` is
rejected as *a lone surrogate*, and a value above `0x10FFFF` as out of range.
There is no surrogate-pair joining: `\uD83D\uDE00` is two errors, not one
emoji, because the language's strings are UTF-8 and a code-unit escape sequence
would be a second encoding rule nobody asked for. Write `"😀"` or `"\u{1F600}"`.

Precedence gains one level below `+`:

```
compare  := additive ( ('==' | '!=') additive )*
additive := term (('+' | '-') term)*
term     := factor (('*' | '/' | '%') factor)*
```

## 4. Semantics

`+` is now typed rather than numeric-only:

| left | right | result |
| --- | --- | --- |
| number | number | sum, as before |
| string | string | concatenation, newly allocated in the arena |
| either | the other | runtime error: `cannot add string to number: "hi" + 42` |

The message names the two types **and** shows both operands, because "type error"
without the value that caused it is the message that makes people read the
runtime's source instead of theirs. Rendering an operand needs its bytes, which
is why `arith` reaches content through `self.string_of(..)` rather than through
`Value`'s `Display`.

`-`, `*`, `/`, `%` on a string keep the existing shape, extended with the types
it found: `'-' needs numbers on both sides (found string and number)`.

`==` and `!=`: equal when the types match and the contents match. Across types
`==` is false and `!=` is true — including `undefined == null`, which is false,
because they are different values and pretending otherwise is JS's mistake, not
a convention to copy. Numbers compare by IEEE rules, so `NaN == NaN` stays false
and `0.0 == -0.0` stays true.

`truthy()` gains strings: a string is true when non-empty. Note what this fixes —
the pointer arm used to be truthy-or-falsy by accident, because a string matched
neither the bool nor the number arm and fell through to `false`. Every non-empty
string would have been falsy.

`MAX_STRING_LEN` is 16 MiB. Exceeding it is a **runtime error**, not a new status
code: see §6.

Identifiers stay ASCII and capped at 32 bytes. That is a Phase 1 decision this
phase does not revisit.

## 5. The ABI, version 2

```c
typedef struct OmniValue {
  uint32_t tag;      /* OMNI_NUMBER 0, OMNI_NIL 1, OMNI_BOOL 2, OMNI_UNDEF 3, OMNI_STRING 4 */
  uint32_t len;      /* bytes of payload for OMNI_STRING, else 0 */
  union OmniPayload { double number; const char *ptr; } payload;
} OmniValue;         /* 16 bytes: 0, 4, 8 */
```

Still 16 bytes, but `number` moves from offset 8 to `payload.number` at offset 8
and `len` occupies the hole that padding already had. Four independent pins keep
the sides from drifting: a Rust `const _: () = assert!`, a C `_Static_assert`,
`ctypes.sizeof` at Python import, and `koffi.sizeof`/`offsetof` at Node load.
All four now check `len`'s offset too.

New exports:

- `uint32_t omni_abi_version(void)` — returns `2`. The value is a wire format
  number, not the crate version: it changes only when the layout or the meaning
  of an existing signature changes. Both SDKs call it at load and refuse to
  continue on a mismatch, which converts "wrong numbers" into "one clear error at
  import".
- `size_t omni_str_copy(OmniEngine*, const OmniValue*, char*, size_t)` — copies
  `min(value->len, cap)` bytes, **without** NUL-terminating (a string may contain
  NUL, and `len` already tells the caller where it ends), and returns the number
  of bytes written. Returns 0 when the value is not an `OMNI_STRING`, when any
  pointer is null, or when the address it carries is **not owned by that engine**.
  Taking the struct by pointer rather than by value is deliberate: it keeps a
  16-byte struct out of three different FFI toolkits' by-value calling
  conventions, and the host already has the struct in memory because that is where
  it read `len` from.
- `uint32_t omni_define_string(OmniEngine*, const char* name, size_t name_len,
  const char* bytes, size_t bytes_len)` — `OMNI_OK` or `OMNI_ERR_ARG`. Rejects a
  name longer than `MAX_IDENTIFIER_LEN`, an empty name, non-UTF-8 in either
  argument, and a size over `MAX_STRING_LEN`. Redefining a name replaces the
  binding and releases the previous backing allocation, so a host that defines
  the same string in a loop does not grow — and the ownership check in
  `omni_str_copy` turns any use of the superseded value into a 0 rather than a
  read of freed memory.
- `uint32_t omni_debug_stats(OmniEngine*, OmniStats*)` — fills a `#[repr(C)]`
  struct of eight `u64` counters plus a leading `u32 size` that the callee writes,
  so a host detects a layout mismatch by reading back a `size` that is not the
  one it compiled against.

`omni_exec`, `omni_exec_number`, `omni_last_error`, `omni_version`,
`omni_engine_new`, `omni_engine_free`, `omni_debug_panic` are unchanged in
signature. `omni_exec_number` still returns `OMNI_ERR_RUNTIME` for a string; a
host that wants a string asks for `omni_exec` and reads the tag.

## 6. No new status code

The obvious addition — `OMNI_ERR_MEMORY = 5` for an out-of-memory egress copy —
is deliberately absent. Every failure this phase can produce is already
classifiable: a too-long string built by a *program* is the program's fault and
is `OMNI_ERR_RUNTIME`; a too-long argument from a *host* is a bad call and is
`OMNI_ERR_ARG`; a genuine allocation failure in Rust aborts rather than returning.
A fifth code would let hosts branch on a condition that cannot tell them anything
`OMNI_ERR_RUNTIME` does not, and every host in the repository would have to learn
it. If a recoverable memory condition ever arrives, it arrives with a code then.

## 7. Hosts

Python reads a string with `omni_str_copy` into a `create_string_buffer` and
decodes UTF-8, so `Result.value` is `str` for `tag == "string"`; `define_string`
and `stats` are methods on `Engine`. Node does the same through koffi, with the
BigInt rule already established for handles applied to `payload.ptr`, and adds
`defineString` and `stats`. Both copy rather than expose an address: a Python
`int` or a JS `Buffer` wrapping an engine address is how a wrapper turns a
documented staleness rule into a segfault.

`examples/c/embed.c` gains the string cases and a stats non-growth assertion, and
must still compile clean under `-Wall -Wextra`.

`crates/omni-wasm` gains a `TAG_STRING` arm that keeps the scaffold compiling and
states, in its doc comment, that wasm has no egress problem because it passes
bytes rather than addresses — a real difference, written down rather than papered
over.

## 8. How each claim is proved

| Claim | Test |
| --- | --- |
| a 100k-execution loop does not grow the heap | `omni-core` unit test with a `cfg(test)` counting `GlobalAlloc`: warm up, snapshot live allocations, run 100,000 string `exec`s, assert the delta is zero |
| the same from outside Rust | `omni_debug_stats` before and after 10,000 string execs, asserted in Rust, C, Python, and Node: `arena_bytes`, `egress_bytes`, and the chunk counts stop growing while `strings` keeps counting |
| a string survives exactly one execution boundary as documented | FFI contract test: exec, copy the bytes, exec again, copy again, both copies match the same content |
| a stale value cannot read foreign memory | `omni_str_copy` returns 0 for a fabricated address and for a pointer belonging to another engine |
| the layout cannot drift silently | four pins (§5) plus `omni_abi_version` checks in both SDKs |
| escapes and surrogates behave | lexer and engine tables, including `\u{D800}` rejected, `\u{1F600}` accepted, raw `"😀"` accepted, unterminated literal a lex error |
| mixed-type `+` tells you something | exact message asserted, with both operand renderings present |
| a panic still cannot unwind | unchanged, and all four new bodies run inside `catch_unwind` like the existing seven |

## 9. Explicitly not in this phase

Ordering and `<`/`>`/`&`/`|`, `len()`, `str()`/number→string formatting, string
indexing and slicing, characters as a type, interning or rope representation,
functions and control flow, host callbacks, a `.d.ts` surface, and Linguist tier
2. Each is a line in the roadmap in `docs/GUIDE.md` section 10, not a gap here.

Concatenation is a copy, not a rope, and `let s = s + "x"` in a loop is O(n²).
That is a known property of the design, chosen over pretending otherwise: one
clear complexity story beats a half-built optimisation.
