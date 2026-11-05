# OmniEngine Phase 3 — Host Callbacks, Re-entrancy, ABI v3

Date: 2026-11-03
Status: approved by the requester, executed as written below
Supersedes: the "there are no functions" line `docs/GUIDE.md` section 3.6 carried
at the end of Phase 2; sections 3.4, 3.6 and 4.7 now describe what this spec built

## 0. What this phase is, and what it is not

Phases 1 and 2 made the bridge run one way: a host calls in, the engine answers,
the host reads a value. Phase 3 makes it run both ways. A program written in
`.omni` can now call a function that lives in Python, in JavaScript, or in the C
that embedded the engine in the first place — and the engine survives the fact
that foreign code is now executing in the middle of one of its own evaluations.

It is **not** a user-defined-function feature. There is no `fn`, no closure, no
`return`, and no way for an `.omni` program to define anything callable. Every
call site resolves to a registration made from outside the language. The
distinction matters for the memory model (§3) and it is the line this phase holds.

Requested, verbatim, as the requirements:

1. Engine and re-entrancy guard: the opaque handle behind a `RefCell`, mutations
   through `try_borrow_mut()`, a new `OMNI_ERR_REENTRANT` status, `IDENT '(' args? ')'`
   parsing, `MAX_ARGS` 16, `MAX_CALL_DEPTH` 64, ABI bumped to 3.
2. FFI exports: `omni_register_host_fn`, `omni_unregister_host_fn`,
   `omni_host_fn_count`, with host exceptions converted to data — a status plus a
   message — and never unwound into Rust.
3. SDKs: Python (`ctypes`) and Node (`koffi`) register and unregister callables,
   holding strong references to their trampolines.
4. Tests across all four surfaces: call parsing, arity, depth, re-entrancy, plus
   documentation and verification.

Four shapes in the request were corrected before implementation, each recorded
here so the difference is a decision rather than a drift.

- The callback returns `i32`, not the ABI's `u32` status. `OMNI_HOST_OK` 0 and
  `OMNI_HOST_ERR` 1 are a **callback's** vocabulary, kept separate from
  `OMNI_ERR_*` so a host never has to ask which namespace a `5` came from. `5` is
  `OMNI_ERR_HOST`, and it is what the *call* reports when the callback returned 1.
- Re-entrancy is a refusal, not a support. The request said "guard"; this is what
  guarding costs. `RefCell` can hand out either a shared borrow or a mutable one,
  so a call attempted from inside a callback returns `OMNI_ERR_REENTRANT` instead
  of aliasing an in-flight `&mut Engine` — which would be undefined behaviour, not
  a bug with an error message.
- Four surfaces cannot report a status: `omni_str_copy`, `omni_last_error`,
  `omni_unregister_host_fn` and `omni_host_fn_count` all return `size_t`, and 0 is
  already the answer they give for "nothing here", so the refusal reuses it;
  `omni_engine_free` returns nothing at all and frees nothing. The header says so,
  because a caller that assumes a code will read a stale one.
- `arity` is explicit at registration, not discovered. Node may default it from
  `fn.length` because JavaScript declares a parameter count; Python requires it as
  a keyword argument, because `lambda *a: …` declares nothing and
  `inspect.signature` cannot tell a variadic callable from a one-argument one. An
  engine that guesses arity guesses in front of the user.

## 1. The registry: a fourth block with a fourth lifetime

Phase 2 documented three memory blocks and their reclaim rules. Host functions are
a fourth, and the reason the set grew rather than one of the three changing is the
callback's own lifetime requirement:

| Block | Contents | Reclaimed |
| --- | --- | --- |
| `arena` | `let` bindings, every string built during evaluation | `reset()` at the entry of each `exec` |
| `egress` | the promoted copy of the *result* string | `rewind()` at the entry of each `exec` |
| `host` | strings injected by `omni_define_string` | only on redefine or engine free |
| **`host_fns`** | name, fixed `arity`, and the callback, as a plain `Vec<HostFunction>` | only on `omni_unregister_host_fn` or engine free |

`Engine::host_fns` is a `Vec`, and `Slot<T>`-style arena residency would have been
the wrong reuse of the bump allocator: a registration is one per name per process
lifetime, the lookup is a linear scan over a handful of entries, and an arena
reset that must not touch it would be a second special case inside `reset()`.

The load-bearing property is the one Phase 2 set up. `arena.reset()` runs at the
entry of `exec`, and a re-entrant call can never reach that entry — so the arena
cannot be reset while a callback holds addresses into it. That is not a lucky
coincidence of ordering; it is why rule 8 (§3) can be stated at all.

`register_host_fn` validates before it stores: non-empty name, name within
`MAX_IDENTIFIER_LEN`, `arity <= MAX_ARGS`, each mapping to `HostError` and from
there to `OMNI_ERR_ARG`. Redefining a name replaces the callback and its
`user_data` together and releases nothing the previous registration owned — the
engine cannot know whether that pointer is still live, so it does not try. Both
SDKs unregister before dropping, which is the caller's half (§7).

## 2. Grammar: the call sits at the primary level

```
factor   := ( '-' | '+' )? primary
primary  := NUMBER | STRING | call | IDENT | '(' expr ')'
call     := IDENT '(' ( expr ( ',' expr )* )? ')'      // <= MAX_ARGS arguments
```

Placing it in `primary` — not a new precedence row — means a call is exactly as
tight as a literal. `square(6) + 1` is 37, `"say: " + shout("hey")` concatenates
the answer with a literal, and `-square(4)` negates the *answer* rather than the
name, because the call is a value by the time the operator looks for its operands.
`square(6) == 36` compares 36 to 36, not 6 to 36.

The one design decision worth arguing for is the **lookahead**. After an
`IDENT`, a following `(` makes it a call and nothing else; otherwise the name is a
binding or global lookup, unchanged from Phase 2. Position is the only
disambiguation, and that is deliberate: a host can register a function called
`print` and define a string global called `print` in the same engine, and neither
registration reserves the identifier or shadows the other. A name-keyed call table
that also had to consult the binding scope would have made `let f = 1` change what
`f(2)` means.

Three limits, all enforced before foreign code runs:

- **Depth.** `call_depth` increments for the whole of `parse_call`, argument
  evaluation included, because the stack the recursion spends is the thing being
  limited — `f(g(x))` nests here, not only `f(f(x))`. At `MAX_CALL_DEPTH` 64 the
  error is a runtime error that names the limit and the consequence: *"calls are
  nested 64 deep, the limit; a host function cannot call itself"*. `exec` resets
  the counter, so a refused program cannot leave the engine at depth 64.
- **Arity.** The registered count is checked against the received count after the
  arguments evaluate, and the message names the function and both numbers —
  *"host function 'square' takes 1 argument, got 2"*. A call site with more than 16
  arguments is refused at parse (`MAX_ARGS`), not truncated into the fixed buffer.
- **Unknown name.** A call to a name nobody registered is *"unknown function
  'nope' (no host function is registered with that name)"* — the parenthetical
  exists because "unknown function" alone reads like it might have been defined in
  the script.

One token is new: `Comma`. A comma was previously a lex error, and giving it its
own variant is what lets `f(1 2)` report *"expected ',' or ')' in the call to 'f'"*
instead of a generic token complaint — the parser can only name what the lexer
admits is a thing. `'('` and `')'` already existed for grouping, so `peek_kind`
after `IDENT` is the only other lexer-facing change.

## 3. Arguments in: a borrowed view, and one rule that follows from re-entrancy

`call_host` lowers each `Value` through `encode_argument`, which differs from
Phase 2's `encode` in exactly one field. For a result crossing outward,
`payload.ptr` addresses the engine's string block and `omni_str_copy` reads through
it. For an argument crossing inward, **`payload.ptr` addresses the first byte of
the text** and `len` bytes of UTF-8 follow it:

```c
memcpy(dst, args[i].payload.ptr, args[i].len);   /* correct */
omni_str_copy(engine, &args[i], dst, cap);       /* OMNI_ERR_REENTRANT, 0 bytes */
```

That is header rule 8, and it is stated as code because prose descriptions of
offsets get misread. The reason it is safe is the shared borrow: `invoke_host`
passes `&Engine` — not `&mut` — to the callback, so nothing that could reset or
rewind a block runs while the callback holds those addresses. The reason a host
cannot *also* call `omni_str_copy` is the same borrow, taken from the other side.
An empty string carries a null pointer with length 0, the same rule `encode`
already applies: no address is handed out for nothing, so a host that sees null
sees zero bytes rather than an unmapped guess.

`HostCall` is `dyn Fn(&Engine, &[Value]) -> HostOutcome + Send`. The `&Engine` is
the whole contract, in a type: a callback may read a string's bytes because they
are still on hand, and it may not allocate, bind, or reset — the engine is busy
running the very call that invoked it.

## 4. Results out: primitives, copied before the frame is gone

A callback writes one `OmniValue` into `out`. `call_host` reads it back into
`HostOutcome`, which carries **primitives only** — `Number`, `Bool`, `Nil`,
`Undefined`, `String(Vec<u8>)`, `Failure(Vec<u8>)` — never a `Value`. A `Value` is
an address the engine minted; letting the FFI fabricate one would mean trusting a
boundary layer to NaN-box a pointer correctly, and the same rule already governs
`Engine::define_string` from the other side. So the host returns bytes or a double
and `Engine` does its own boxing, into the exec arena, like any string the program
built. Result promotion to egress is unchanged from Phase 2.

Returning a string is the mirror of rule 8: `out.payload.ptr` addresses **the
host's** bytes, and `host_bytes` copies them into an owned `Vec<u8>` before
`call_host` returns — while the callback's frame, and everything it points at, is
still alive. The host's buffer is reusable the moment the callback returns.

Three readings are worth naming rather than leaving implicit:

- `OMNI_BOOL` is `payload.number != 0.0`. Returning `true` as `1.0` and `1.0` as
  `true` are the same eight bytes; nonzero is the rule, and it is in the header.
- A tag outside 0..=4 is a failure, not a default: *"the host returned tag 9,
  which is not a value"*.
- A null `user_data` is fine — the engine never reads it. `HostContext` is a
  newtype over `*mut c_void` with one `unsafe impl Send`, so a callback cannot
  silently make the handle non-`Send`; the safety comment restates the ABI rule it
  depends on (one handle, one thread, pointee alive at least as long as the
  registration).

## 5. A host failure is data

A callback reports failure by returning `OMNI_HOST_ERR` with its message in `out`
as an `OMNI_STRING`. The engine turns that into `ErrorKind::Host`, which maps to
status 5, and the script's error text is the host's own sentence at the line of
the call:

```
line 3: host function 'boom' failed: host said no
```

A failure with no message still names the function, because *"host function
failed"* without the name cannot be acted on.

Nothing unwinds: a C++ exception, a Python exception and a JavaScript throw must
be caught at the trampoline before the boundary, because unwinding through the
Rust frames that are calling the host is not recoverable. Both SDKs do exactly
that. Python catches `BaseException`,
not `Exception`, because a `ctypes` frame that lets anything propagate —
`KeyboardInterrupt` included — returns 0 silently and the engine reads a
half-written `out` as a successful result; the message is `TypeName:` followed by
the exception's own text. Node catches everything, formats an `Error` as
`name: message`, and falls back to `String(err)` for a thrown non-`Error`. Both
convert a **return value the ABI cannot carry** (a Python object, a JS `{}`) into
the same kind of host failure, with a message that names the type it found.

## 6. Re-entrancy: refused, not supported

`OmniEngine` is `RefCell<Engine>`; every export takes the handle through
`borrow_mut`/`borrow`, and a failure there returns `OMNI_ERR_REENTRANT` (6) rather
than handing out a second mutable reference. Two helpers carry that status into the
shapes that can hold it — `guarded` for a `u32` and `guarded_len` for a `usize` —
and the four surfaces whose only answer is a count refuse in place:
`omni_str_copy` and `omni_last_error` return 0 before they read anything, and
`omni_host_fn_count` and `omni_unregister_host_fn` return 0 (§0).

Two consequences a host can actually observe:

- **`omni_engine_free` from inside a callback does not free.** The engine is
  mid-evaluation, its borrow is live, and freeing it there would have the borrow
  read freed memory on the way out. The call returns having done nothing, and the
  documented way to know is `omni_host_fn_count`.
- **A refused call leaves the engine usable.** The borrow is dropped when the
  callback returns, so `omni_exec("1+1")` inside a callback fails and the
  evaluation that follows the callback succeeds. `verify-all.sh` asserts both halves
  in the same row, because "refused" without "still works" is only half a contract.

`omni_debug_panic` keeps its meaning: called from inside a callback it reports
`OMNI_ERR_REENTRANT` instead of panicking, so the test that a panic cannot unwind
cannot itself become the re-entrancy bug.

## 7. The ABI, version 3

```c
typedef int32_t (*omni_host_fn)(void *user_data,
                                const OmniValue *args, uint32_t argc,
                                OmniValue *out);

OMNI_API uint32_t omni_register_host_fn(OmniEngine *engine, const char *name,
                                        size_t name_len, uint32_t arity,
                                        omni_host_fn callback, void *user_data);
OMNI_API size_t   omni_unregister_host_fn(OmniEngine *engine, const char *name,
                                          size_t name_len);
OMNI_API size_t   omni_host_fn_count(const OmniEngine *engine);
```

`name_len` is explicit like every other buffer in this ABI (§0 of the Phase 2
spec made that rule; a NUL-terminated name would have been its one exception). The
`OmniValue` layout, the tags and the statuses through 4 keep their version-2
meaning, so v3 is purely additive — and that made the ABI rule itself worth
restating, because Phase 2 wrote it as "changes only when the layout or the
meaning of an existing signature changes". Adding a surface moves the number too.
A host compiled against 2 reads correct *values* from a library that reports 3, but
it cannot resolve the three new symbols, and the load-time version check is the
only place that can be noticed before the first call rather than at it. The header
says this where a host will read it, and all four layout pins from Phase 2 still
fire unchanged.

The export list is fourteen entries, up from eleven; `objdump -p` is the check the
guide cites.

**The one rule the header cannot write.** The engine stores a function pointer and
an opaque context; it owns neither. A koffi `koffi.register` handle and a ctypes
trampoline are objects in the *host's* runtime, and if the host's GC collects one
while the engine still points at it, the next call is a jump into freed memory with
nothing on either side able to warn. So both SDKs key a table on the name — Python
holds a `_HostBinding` of `(name, arity, fn, trampoline, keep)`, Node holds
`{ arity, fn, handle, nameBytes }` — and release the trampoline only after
`omni_unregister_host_fn` returns, or when the engine closes. Python's `keep` is
the same idea one level down: the engine reads `out` *after* the trampoline
returns to C, so the buffer holding a returned string is held until the next call
replaces it — one slot per function, not a growing list, because a callback
returns one value at a time and re-entrancy is refused. Node additionally must not
let `koffi.unregister` run while the engine is mid-call, which is the same ordering
the C surface gets from calling `omni_unregister_host_fn` explicitly.

## 8. Hosts

Python: `Engine.register_function(name, fn, *, arity)` and
`Engine.unregister_function(name) -> bool`. A name over 32 bytes, a non-callable,
a non-integer `arity` and an over-`MAX_ARGS` count are all rejected client-side,
before the ABI sees them, with the same `OmniError` machinery the rest of the
package uses (`kind == "argument"`). The non-callable check is the one worth a
line: Node had it from the start because `typeof` is free, Python did not, and a
`ctypes` closure can be built around anything — so a mistyped callback registered
against Python would have sat in the registry and failed as a host error inside
whichever script happened to call it. Both now refuse at registration, which is
where the mistake was made.

Node: `Engine.registerFunction(name, fn, { arity })` — `fn.length` is the default —
and `Engine.unregisterFunction(name) -> bool`, idempotent, and honest about a closed
engine (it releases the trampoline and returns `false`, because nothing left a live
registry).

Both gained `host_function_count` / `hostFunctionCount`, reading `omni_host_fn_count`
rather than the SDK's own table. That replaced a Phase-3-introduced
`host_functions`/`hostFunctions` that returned names from one side and a count from
the other; a count the engine itself answers makes a lost registration a visible
difference instead of a surprise at the next call. After `close()` there is no
engine to ask, so the Node test asserts its own map is empty instead.

`examples/c/embed.c` is the third host and the strictest: `square_cb`, `shout_cb`
(a borrowed-argument read with `memcpy`, per rule 8) and `reentrant_cb`, asserted
byte-for-byte among 39 checks under `-Wall -Wextra`.

`crates/omni-wasm` mirrors the export shape in its scaffold and keeps the honest
doc comment: wasm has no re-entrancy guard because it has no shared handle, and
callbacks across a wasm boundary are a different design, not this one translated.

## 9. How each claim is proved

| Claim | Test |
| --- | --- |
| a call parses at primary tightness, with and without args | `omni-core` unit tests, including `2 * shout("hi") + 1` and `a_missing_comma_names_the_comma` |
| a call left open says which one | *"expected ',' or ')' in the call to 'square' (found 'end of input')"* — the lexer ends the stream with an `Eof` token rather than running out, so the call's own message fires instead of a bare end-of-input |
| a name and a call to the same name coexist | `a_global_and_a_host_function_can_share_a_name`: `square` reads the global, `square(4)` calls the host function |
| arity names function and both counts | exact message asserted for 0-given-1-registered and 2-given-1-registered |
| depth 64 is refused and the engine recovers | 64-deep nesting via a self-calling host function, then a normal `exec` succeeds |
| 17 arguments are refused, not truncated | parse-time error, and `MAX_ARGS`-exact call works with all 16 |
| a host failure is status 5 with the host's sentence | asserted in Rust, C, Python and Node; empty-message case names the function |
| arguments are the text, not the header | C `memcpy` and Python `string_at` and Node `koffi.decode` all read the same bytes as the same program |
| a returned string is copied before the frame dies | host returns a stack buffer's address; Python reuses the buffer after returning; the read still matches |
| a bad registration is refused where it is made | Python and Node each reject a non-callable, a long name and an out-of-range `arity` with `kind == 'argument'`, and assert `host_function_count` / `hostFunctionCount` is still 0 |
| re-entrancy is refused on every surface | `omni_exec`, `omni_str_copy` (0), `omni_last_error` (empty), `omni_register_host_fn` (6), `omni_unregister_host_fn` (0), `omni_host_fn_count` (0) — asserted from inside a callback |
| a refused call does not break the engine | `verify-all.sh` row: the refusal text **and** `6 * 7` = 42 from the same engine |
| `omni_engine_free` refuses mid-callback | `host_callbacks.rs`: count still nonzero and the handle still usable |
| registrations are per-engine | two engines, one registration, asserted through both SDKs |
| closing releases every trampoline | Python and Node assert their own table empty and the ABI count at 0 before free |
| no leak across 2,000 callback executions | `omni_debug_stats` deltas, in the existing verify-all memory row |
| the four surfaces agree on v3 | `omni_abi_version`, header `_Static_assert`, `ctypes`, `koffi`, and `verify-all.sh`'s ABI row |
| the same bytes from different hosts | two new `verify-all.sh` rows compare Python's and Node's stdout byte-for-byte for a callback program and a failing callback |

187 Rust tests (128 core, 45 FFI: 18 unit + 20 contract + 7 `host_callbacks`), 7
CLI, 7 wasm-scaffold, 92 Python, 74 Node. `scripts/verify-all.sh` is 17 rows, all
passing, nothing skipped.

## 10. Explicitly not in this phase

User-defined functions, closures and `return`; control flow (`if`, `while`, `for`);
variadic or optional host-function arity; overloading by type; calling *back* into
a host function from inside a host function (that is what the refusal guards, and a
real design for it needs a re-entrant engine, not a comment); `Send`+`Sync` handles
or any multi-thread story; a callback that returns a `Value` the engine did not
mint; named or keyword arguments; and a `.d.ts` surface. Each is a line in the
roadmap in `docs/GUIDE.md` section 10, not a gap here.

One cost is worth stating because it is a property of the design, not a bug:
arguments are passed by copying each into a fixed 16-slot `OmniValue` array on the
stack, so a call is a handful of stores and a string argument costs nothing but an
address. It is what keeps the callback boundary allocation-free on the way in — and
what makes `MAX_ARGS` a hard number rather than a soft default.
