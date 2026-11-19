/*
 * omni.h -- the C ABI of OmniEngine, version 4.
 *
 * Copyright (c) 2026 Tamjid Abdur Rahman. All Rights Reserved.
 * Engineered by Vextalor Global. Licensed under VGPIL 1.0; see LICENSE.
 *
 * Hand-written and authoritative. It mirrors
 * crates/omni-ffi/src/lib.rs, and the Rust side has compile-time asserts
 * pinning OmniValue's and OmniStats' size and field offsets so the two cannot
 * drift silently. C11 _Static_asserts here pin the same numbers from this side.
 *
 * crates/omni-ffi/cbindgen.toml can regenerate this surface with:
 *
 *     cbindgen --config crates/omni-ffi/cbindgen.toml crates/omni-ffi \
 *              --output crates/omni-ffi/include/omni.h
 *
 * That command overwrites this file, so review the diff before committing it:
 * the caller rules below are the part cbindgen cannot write.
 *
 * ## What a caller must know
 *
 * 1. A string result is an address the engine owns, not a copy. omni_exec()
 *    returns tag OMNI_STRING with `payload.ptr` pointing into the engine's
 *    egress block. That block is rewound -- never freed or unmapped -- at the
 *    start of the next omni_exec(), omni_exec_many() or omni_exec_parallel()
 *    on the same engine, so the bytes stay valid memory for the life of the
 *    handle and the documented contract is:
 *
 *        valid until your next omni_exec(), omni_exec_many() or
 *        omni_exec_parallel() on this engine.
 *
 *    Reading it after that is a wrong read, never a crash: you get whatever the
 *    later execution wrote at those addresses. To keep a string, copy it with
 *    omni_str_copy() into storage you own.
 *
 *    omni_exec_many() and omni_exec_parallel() are the calls where several
 *    results are live at once: they rewind the block once for the whole batch,
 *    so all `count` string results stay readable together until your next
 *    execution of any kind. See rule 11.
 *
 *    `payload.ptr` identifies the string, it is not the first byte of the text:
 *    the engine keeps a length word in front of the bytes it stores. That is
 *    why you call omni_str_copy() instead of memcpy'ing from it -- the copy
 *    knows where the payload starts, checks that the address belongs to the
 *    engine you name, and bounds itself by the engine's own header rather than
 *    by a `len` that may be stale. A host callback's arguments are the one
 *    place that pointer IS the text; see rule 8.
 *
 * 2. Error text is copied into storage you own. Call omni_last_error() with a
 *    buffer; it truncates rather than overflowing, and always NUL-terminates.
 *
 * 3. `src` is not required to be NUL-terminated -- `len` decides what is read.
 *    It must be valid UTF-8. Malformed UTF-8 returns OMNI_ERR_ARG without
 *    touching the engine.
 *
 * 4. Strings are not NUL-terminated either, and may contain an interior NUL.
 *    `len` is the only end-of-string. omni_str_copy() writes no terminator; if
 *    you want a C string, allocate len + 1 and write the byte yourself.
 *
 * 5. Check omni_abi_version() before anything else. It is the wire format, not
 *    the crate version, and it changes whenever a struct layout or the meaning
 *    of an existing signature changes. A host compiled against 2 and a library
 *    that reports 1 must refuse to run, not read plausible garbage.
 *
 * 6. A Rust panic is converted to OMNI_ERR_PANIC at this boundary and never
 *    unwinds into your process. After OMNI_ERR_PANIC the engine is still
 *    usable, but treat the handle as suspect and recreate it if you can.
 *
 * 7. One engine handle is one thread at a time. Do not call two functions in
 *    this header on the same handle concurrently without your own lock.
 *    omni_exec_parallel() is the supported way to use several cores from one
 *    handle: it forks isolated engines internally and joins them before it
 *    returns. Host callbacks registered on that handle may then run on worker
 *    threads, so anything their user_data points at must be usable from those
 *    threads; a host that cannot promise this must not use the parallel entry
 *    point (or must not register that callback).
 *
 * 8. A host function's arguments are a borrowed view, and the one place a
 *    pointer is the text itself. Inside an omni_host_fn callback, an args[i]
 *    tagged OMNI_STRING has `payload.ptr` at its FIRST BYTE and `len` bytes of
 *    UTF-8 following it -- copy with memcpy(), not through omni_str_copy(),
 *    which is a call into the engine and would answer OMNI_ERR_REENTRANT with a
 *    zero-length read. The view is good only until the callback returns, and
 *    returning a string is its mirror: put the address of YOUR bytes in `out`,
 *    and the engine copies them into its own memory before it uses them, so
 *    your buffer may be reused the moment you return.
 *
 * 9. Re-entrancy is refused, not supported. Calling any function in this header
 *    from inside a callback, on the same handle, returns OMNI_ERR_REENTRANT
 *    rather than handing out a second mutable reference to the engine.
 *    omni_str_copy(), omni_unregister_host_fn(), omni_last_error() and
 *    omni_batch_error() report it as 0, because a size_t and a bool carry no
 *    status. Rule 8 holds precisely because this rule does: the arena is reset
 *    at the entry of omni_exec(), omni_exec_many() and omni_exec_parallel(), and
 *    a nested call can never reach that entry.
 *
 * 10. Your callback reports failure by returning OMNI_HOST_ERR, never by
 *    unwinding. A C++ exception, a Python exception and a JavaScript throw all
 *    have to be caught before this boundary, because unwinding through the Rust
 *    frames that are calling you is not recoverable. Put the message in `out` as
 *    an OMNI_STRING and return 1; the script's error becomes
 *    "host function 'name' failed: <message>" at the line of the call.
 *
 * 11. A batch is N independent executions, not one long program. Each item
 *    starts with the bindings the previous item left behind cleared, so item 2
 *    cannot read item 1's `let`. Host globals and registered functions survive
 *    every item, as they survive every omni_exec().
 *
 *    A failing item does not stop the batch: `out[i]` and `statuses[i]` are
 *    written for every i, a failed item's value is OMNI_UNDEF, and the call
 *    returns the status of the FIRST failure (OMNI_OK if there was none). Read
 *    omni_batch_error(engine, i, buf, cap) for that item's rendered message;
 *    omni_last_error() only ever holds the final item's.
 *
 *    The whole batch shares one egress rewind, so every string result is
 *    readable when the call returns and stays readable until your next
 *    execution of any kind. The trade is memory: reserved egress bytes are
 *    bounded by one batch's total demand rather than by one item's, so a batch
 *    of 4096 long strings reserves more than 4096 single calls would.
 *
 *    omni_exec_parallel() follows every rule in this item. Items run on forked
 *    engines, one per worker, but results are still written in source order and
 *    all live together in the caller's egress block. Each item gets its own
 *    execution gas, so one runaway program cannot spend another item's budget.
 *
 *    Argument errors refuse the whole call before any item runs. A NULL array,
 *    a count above OMNI_MAX_BATCH, an over-long `lengths[i]`, or non-UTF-8 in
 *    any item returns OMNI_ERR_ARG and writes nothing -- the same answer
 *    omni_exec() gives for a malformed source, rather than a half-run batch.
 *    `count` of 0 is a no-op returning OMNI_OK: nothing is written and nothing
 *    is reclaimed, so polling an empty queue does not expire held results.
 *
 * ## Linking
 *
 * cargo build -p omni-ffi produces, for x86_64-pc-windows-gnu:
 *   omni.dll          (load dynamically: ctypes, koffi, LoadLibrary)
 *   libomni.dll.a     (link at build time with MinGW)
 *   libomni.a         (static archive, MinGW)
 * MSVC emits omni.dll.lib instead of the import archive, and the C runtime
 * required is UCRT (api-ms-win-crt-*), which is present on Windows 10 and later.
 */

#ifndef OMNI_H
#define OMNI_H

#include <stddef.h>  /* size_t, offsetof */
#include <stdint.h>  /* uint32_t, uint64_t */

#ifdef _WIN32
#define OMNI_CALL __cdecl
#else
#define OMNI_CALL
#endif

#if defined(_WIN32) && !defined(OMNI_STATIC)
/* Consumers link the import library; dllimport is how the compiler knows. */
#define OMNI_API __declspec(dllimport)
#else
#define OMNI_API
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* ---- the ABI level -------------------------------------------------- */

/* Wire format, not the crate version. See caller rule 5.
 *
 * Version 4 changed no layout: OmniValue is still 16 bytes and OmniStats still
 * 72, and every struct, tag and status keeps its version-3 meaning. What changed
 * is the surface -- omni_exec_many() and omni_batch_error(), which a host
 * compiled against 3 cannot declare -- and one field's meaning: OmniStats.execs
 * now counts batch items as well as single calls, so a v3 host reading it as
 * "number of omni_exec() calls" would draw a wrong conclusion from a correct
 * number. That is why the version moved rather than staying at 3.
 *
 * The DevPack update added OmniStats.last_exec_micros inside version 4 rather
 * than after it: the field reuses the compiler padding after `size`, so sizeof
 * and every other offset are unchanged and a v4 host stays correct. A field that
 * had grown the struct would have moved the version to 5.
 *
 * Version 3 was the callback increment: the omni_host_fn type, the three
 * registration functions, and statuses 5 and 6.
 *
 * Phase 9 is additive inside version 4, exactly as the DevPack timer and the
 * parallel entry point were: OmniProfile is a new struct, and the three
 * profiling exports change no existing signature or layout, so a host that
 * only calls the older functions remains correct.
 *
 * Version 5 is the permission increment: omni_engine_allow_fs_read,
 * omni_engine_allow_fs_write and omni_engine_set_module_root make the
 * sandbox configurable through the ABI. Every existing export keeps its
 * exact signature and layout; the bump says the permission surface is part
 * of the contract by name, so a v5 host refuses an engine that cannot
 * configure a sandbox rather than silently running with none. */
#define OMNI_ABI_VERSION 6u

/* The longest path, in bytes, the permission exports accept. A sandbox
 * configuration longer than this is a configuration bug, not a path; the
 * ceiling is the same one plugin load paths get, and it keeps the
 * canonicalization work bounded. Mirrors OMNI_MAX_PATH in omni_ffi. */
#define OMNI_MAX_PATH 4096u

/* ---- status codes: the u32 returned by every call ---- */

#define OMNI_OK            0u /* the call succeeded                       */
#define OMNI_ERR_COMPILE   1u /* source did not lex or parse              */
#define OMNI_ERR_RUNTIME   2u /* parsed, then failed while evaluating     */
#define OMNI_ERR_PANIC     3u /* a Rust panic was caught at the boundary  */
#define OMNI_ERR_ARG       4u /* null pointer, bad length, or bad UTF-8   */
#define OMNI_ERR_HOST      5u /* a host function returned OMNI_HOST_ERR   */
#define OMNI_ERR_REENTRANT 6u /* from a callback, on this same handle     */
#define OMNI_ERR_CLUSTER   12u /* transport, protocol, or worker-pool failure */

/* There is deliberately no OMNI_ERR_MEMORY. A program that builds too long a
 * string is the program's fault (OMNI_ERR_RUNTIME); a host that passes too long
 * an argument is a bad call (OMNI_ERR_ARG); a real allocation failure aborts.
 * The two codes version 3 adds are not memory codes either: 5 names whose
 * callback refused, and 6 names a call the engine will not honour. Neither tells
 * a caller anything the older codes did not already cover. */

/* ---- value tags: OmniValue::tag ---- */

#define OMNI_NUMBER        0u /* payload.number holds the value           */
#define OMNI_NIL           1u /* no payload                               */
#define OMNI_BOOL          2u /* payload.number is 0.0 or 1.0             */
#define OMNI_UNDEF         3u /* no payload; an empty program             */
#define OMNI_STRING        4u /* payload.ptr + len, engine-owned          */

/* ---- limits: mirrors of omni_core::{object::MAX_STRING_LEN,
 *       engine::{MAX_IDENTIFIER_LEN, MAX_ARGS, MAX_CALL_DEPTH},
 *       lexer::MAX_SOURCE_LEN} and omni_ffi::MAX_BATCH ----
 *
 * Restated here because C cannot read a Rust constant; the engine enforces them
 * and refuses a longer payload (OMNI_ERR_ARG from a host, OMNI_ERR_RUNTIME from
 * a program), so a wrong number in this header costs a rejected call rather
 * than memory corruption. */

#define OMNI_MAX_STRING_LEN     (16u * 1024u * 1024u)
#define OMNI_MAX_IDENTIFIER_LEN 32u
#define OMNI_MAX_ARGS           16u /* arguments a call may pass, and an arity may claim */
#define OMNI_MAX_CALL_DEPTH     64u /* nested calls one execution may reach              */
#define OMNI_MAX_SOURCE_LEN     (256u * 1024u * 1024u) /* bytes one program may carry */
#define OMNI_MAX_BATCH          4096u /* items one batch call may carry            */
#define OMNI_MAX_OPCODES        64u   /* entries in an OmniProfile heatmap array   */

/* ---- plugin limits: mirrors of omni_ffi::plugin ----
 *
 * A plugin manifest that exceeds one of these is refused at load with
 * OMNI_ERR_ARG, before any of its code is entered. */
#define OMNI_PLUGIN_MAGIC         0x4F4D4E49u /* "OMNI", the manifest's first word */
#define OMNI_PLUGIN_ABI_VERSION   1u    /* manifest wire version; bumps with layout */
#define OMNI_MAX_PLUGIN_FUNCTIONS 64u   /* functions one manifest may declare       */
#define OMNI_MAX_PLUGIN_NAME      32u   /* bytes of the manifest's name, NUL-padded */
#define OMNI_MAX_PLUGIN_PATH      4096u /* bytes of a load path the engine accepts  */

/* The payload of an OmniValue: whichever arm its tag says is live. */
typedef union OmniPayload {
  double number;       /* OMNI_NUMBER, OMNI_BOOL                     */
  const char *ptr;     /* OMNI_STRING: `len` bytes, owned by engine  */
} OmniPayload;

/* A single 16-byte result, passed by pointer or written by the callee.
 *
 * tag at 0, len at 4, payload at 8, size 16. `len` is a byte count for
 * OMNI_STRING and zero for every other tag; it fills padding that was already
 * there, so adding strings cost nothing. */
typedef struct OmniValue {
  uint32_t tag;        /* one of the OMNI_* tags above        */
  uint32_t len;        /* payload bytes for OMNI_STRING, else 0 */
  OmniPayload payload;
} OmniValue;

/* The engine's memory counters, for omni_debug_stats().
 *
 * `size` is written by the callee with sizeof(OmniStats). A host that compiled
 * against a different layout reads back a size it does not recognise and knows
 * to stop, rather than reading a counter from the wrong offset forever.
 *
 * `last_exec_micros` occupies bytes the compiler already padded after `size`, so
 * adding it kept the struct at 72 bytes and every other field at its v4 offset.
 * That is why the stats struct stayed put through version 4: a host compiled
 * against the original header reads the same size and the same fields, and only
 * sees a counter it did not ask for in bytes it was already skipping. */
typedef struct OmniStats {
  uint32_t size;                /* sizeof(OmniStats), written by the callee */
  uint32_t last_exec_micros;    /* wall-clock us the last execution spent in
                                 * the engine, lexing included; saturates at
                                 * UINT32_MAX. Zero before the first run.     */
  uint64_t execs;               /* executions started: one per omni_exec(),
                                 * one per item of every omni_exec_many()   */
  uint64_t strings;             /* string blocks built, cumulative          */
  uint64_t arena_bytes_in_use;
  uint64_t arena_bytes_reserved;
  uint64_t arena_chunks;
  uint64_t egress_bytes_in_use;
  uint64_t egress_bytes_reserved;
  uint64_t egress_high_water;   /* largest egress demand between two rewinds:
                                 * one execution, or one whole batch        */
} OmniStats;

/* The most recent execution's profile, for omni_profile().
 *
 * `size` follows the OmniStats rule: written by the callee with
 * sizeof(OmniProfile), compared by the caller against the size it compiled
 * with. The heatmap arrays index by opcode discriminant -- omni_opcode_name()
 * translates an index into the mnemonic a host prints. Entries at
 * opcode_count and above are always zero.
 *
 * total_instructions is always the sum of opcode_calls. opcode_ns is the
 * per-opcode self-time split and stays all-zero unless profiling was enabled
 * with omni_engine_set_profiling(); the whole-run wall clock elapsed_ns is
 * recorded either way. peak_bytes is the execution arena's live bytes when the
 * run ended -- allocation is monotonic between reclamations, so that is the
 * run's peak live heap, not reserved space. */
typedef struct OmniProfile {
  uint32_t size;               /* sizeof(OmniProfile), written by the callee */
  uint32_t opcode_count;       /* live opcodes; entries past this read zero  */
  uint64_t total_instructions; /* dispatches of the most recent execution,
                                * failed runs included                     */
  uint64_t elapsed_ns;         /* wall-clock ns of the chunk run, dispatch
                                * loop only                                */
  uint64_t peak_bytes;         /* live arena bytes at the end of the run    */
  uint64_t reserved;           /* zero; room to grow without moving offsets */
  uint64_t opcode_calls[OMNI_MAX_OPCODES]; /* dispatch count per opcode     */
  uint64_t opcode_ns[OMNI_MAX_OPCODES];    /* self-time per opcode, opt-in  */
} OmniProfile;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
#define OMNI_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#elif defined(__cplusplus) && (__cplusplus >= 201103L)
#define OMNI_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
#else
/* Pre-C11: the layout is still pinned by the Rust compile-time asserts and by
 * both SDKs, so what is lost here is this header's own check, not the
 * guarantee. A C90 host should be reading omni_abi_version() instead. */
#define OMNI_STATIC_ASSERT(cond, msg)
#endif

OMNI_STATIC_ASSERT(sizeof(OmniValue) == 16, "OmniValue must stay 16 bytes");
OMNI_STATIC_ASSERT(offsetof(OmniValue, tag) == 0, "OmniValue::tag must stay at offset 0");
OMNI_STATIC_ASSERT(offsetof(OmniValue, len) == 4, "OmniValue::len must stay at offset 4");
OMNI_STATIC_ASSERT(offsetof(OmniValue, payload) == 8, "OmniValue::payload must stay at offset 8");
OMNI_STATIC_ASSERT(sizeof(OmniPayload) == 8, "OmniPayload must stay 8 bytes");
OMNI_STATIC_ASSERT(offsetof(OmniPayload, number) == 0, "OmniPayload::number starts at 0");
OMNI_STATIC_ASSERT(offsetof(OmniPayload, ptr) == 0, "both arms of OmniPayload start at 0");
OMNI_STATIC_ASSERT(sizeof(OmniStats) == 72, "OmniStats must stay 72 bytes");
OMNI_STATIC_ASSERT(offsetof(OmniStats, size) == 0, "OmniStats::size must stay at offset 0");
OMNI_STATIC_ASSERT(offsetof(OmniStats, last_exec_micros) == 4,
                   "OmniStats::last_exec_micros reuses the padding at offset 4");
OMNI_STATIC_ASSERT(offsetof(OmniStats, execs) == 8, "OmniStats::execs must stay at offset 8");
OMNI_STATIC_ASSERT(offsetof(OmniStats, egress_high_water) == 64, "OmniStats tail must stay at 64");
OMNI_STATIC_ASSERT(sizeof(OmniProfile) == 1064, "OmniProfile must stay 1064 bytes");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, size) == 0, "OmniProfile::size must stay at offset 0");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, opcode_count) == 4,
                   "OmniProfile::opcode_count must stay at offset 4");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, total_instructions) == 8,
                   "OmniProfile::total_instructions must stay at offset 8");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, elapsed_ns) == 16,
                   "OmniProfile::elapsed_ns must stay at offset 16");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, peak_bytes) == 24,
                   "OmniProfile::peak_bytes must stay at offset 24");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, reserved) == 32,
                   "OmniProfile::reserved must stay at offset 32");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, opcode_calls) == 40,
                   "OmniProfile::opcode_calls must stay at offset 40");
OMNI_STATIC_ASSERT(offsetof(OmniProfile, opcode_ns) == 552,
                   "OmniProfile::opcode_ns must stay at offset 552");

/* ---- native plugins: a shared library the engine loads and verifies ---- */

/* The calling convention of one plugin function: the omni_host_fn contract
 * with a C-unwind boundary. A plugin panic unwinds only as far as the engine's
 * trampoline, which turns it into a host-call failure. */
typedef int32_t(OMNI_CALL *omni_plugin_call)(void *user_data,
                                             const OmniValue *args,
                                             uint32_t argc,
                                             OmniValue *out);

/* One exported function, as the plugin's manifest declares it. The engine
 * reads the table once, at load, and copies everything it needs; nothing here
 * is trusted past verification. */
typedef struct OmniPluginFn {
  const char *name;         /* NUL-terminated UTF-8; read at load, copied */
  uint32_t arity;           /* parameters the engine calls with; <= OMNI_MAX_ARGS */
  uint32_t reserved;        /* must be zero */
  omni_plugin_call call;    /* NULL is refused at load */
} OmniPluginFn;

/* The bytes behind the `omni_plugin_manifest` symbol. 80 bytes, fixed.
 *
 * The manifest is read with unaligned, offset-bounded reads: a hostile library
 * gets itself refused, never a wild read. size is what the plugin claims and
 * must equal sizeof(OmniPluginManifest) or the load fails. */
typedef struct OmniPluginManifest {
  uint32_t magic;                /* OMNI_PLUGIN_MAGIC                     */
  uint32_t size;                 /* sizeof(OmniPluginManifest)            */
  uint32_t abi_version;          /* OMNI_PLUGIN_ABI_VERSION               */
  uint32_t omni_abi;             /* OMNI_ABI_VERSION the plugin was built for */
  char name[OMNI_MAX_PLUGIN_NAME]; /* NUL-padded UTF-8                    */
  uint32_t version_major;
  uint32_t version_minor;
  uint32_t version_patch;
  uint32_t reserved;
  uint32_t function_count;       /* <= OMNI_MAX_PLUGIN_FUNCTIONS          */
  const OmniPluginFn *functions; /* NULL is refused when function_count > 0 */
} OmniPluginManifest;

OMNI_STATIC_ASSERT(sizeof(OmniPluginFn) == 24, "OmniPluginFn must stay 24 bytes");
OMNI_STATIC_ASSERT(offsetof(OmniPluginFn, name) == 0, "OmniPluginFn::name must stay at offset 0");
OMNI_STATIC_ASSERT(offsetof(OmniPluginFn, arity) == 8, "OmniPluginFn::arity must stay at offset 8");
OMNI_STATIC_ASSERT(offsetof(OmniPluginFn, call) == 16, "OmniPluginFn::call must stay at offset 16");
OMNI_STATIC_ASSERT(sizeof(OmniPluginManifest) == 80, "OmniPluginManifest must stay 80 bytes");
OMNI_STATIC_ASSERT(offsetof(OmniPluginManifest, magic) == 0,
                   "OmniPluginManifest::magic must stay at offset 0");
OMNI_STATIC_ASSERT(offsetof(OmniPluginManifest, size) == 4,
                   "OmniPluginManifest::size must stay at offset 4");
OMNI_STATIC_ASSERT(offsetof(OmniPluginManifest, abi_version) == 8,
                   "OmniPluginManifest::abi_version must stay at offset 8");
OMNI_STATIC_ASSERT(offsetof(OmniPluginManifest, omni_abi) == 12,
                   "OmniPluginManifest::omni_abi must stay at offset 12");
OMNI_STATIC_ASSERT(offsetof(OmniPluginManifest, name) == 16,
                   "OmniPluginManifest::name must stay at offset 16");
OMNI_STATIC_ASSERT(offsetof(OmniPluginManifest, function_count) == 64,
                   "OmniPluginManifest::function_count must stay at offset 64");
OMNI_STATIC_ASSERT(offsetof(OmniPluginManifest, functions) == 72,
                   "OmniPluginManifest::functions must stay at offset 72");

/* Opaque. Never dereference; the layout is a Rust private detail. */
typedef struct OmniEngine OmniEngine;

/* ---- host callbacks: the function a host owns and a script calls ---- */

/* The calling convention the engine uses to enter host code.
 *
 * user_data: whatever was passed to omni_register_host_fn(). The engine never
 *            reads it; it only hands it back, so it may point at anything.
 * args:      an array of `argc` OmniValues, one per argument, in source order.
 *            Follows caller rule 8 -- the strings in it are the engine's, and
 *            readable only until this function returns.
 * argc:      how many elements `args` has. Equals the `arity` registered under
 *            the name the script called: the engine checks before entering you.
 * out:       where to write the result (or, on failure, the message). Write it;
 *            the engine reads it the moment you return.
 *
 * Return OMNI_HOST_OK to hand `out` back as the call's value, or OMNI_HOST_ERR
 * to fail the call with `out` read as an OMNI_STRING message. See rule 10: an
 * exception of any kind must be caught here, not unwound into Rust. */
typedef int32_t(OMNI_CALL *omni_host_fn)(void *user_data,
                                         const OmniValue *args,
                                         uint32_t argc,
                                         OmniValue *out);

#define OMNI_HOST_OK  0 /* the value in out is the result                    */
#define OMNI_HOST_ERR 1 /* out holds a message; the call fails with it       */

/* ABI level of the loaded library. Compare with OMNI_ABI_VERSION before
 * trusting any other symbol in this header. */
OMNI_API uint32_t OMNI_CALL omni_abi_version(void);

/* Version string, static and NUL-terminated. Never free it. */
OMNI_API const char *OMNI_CALL omni_version(void);

/* ---- update check ---------------------------------------------------- *
 *
 * The caller fetches a release manifest; omni_check_update() reads it and
 * answers whether it names a newer engine. Keeping the fetch outside this ABI
 * is why the engine still has no network stack and no TLS dependency: a host
 * that already speaks HTTPS hands the document over as bytes.
 *
 * The document is JSON with three required parts and one optional one:
 *
 *   { "version": "0.2.0", "abi": 5, "notes": "https://…",   // notes optional
 *     "assets": { "win32-x64": { "url": "https://…", "sha256": "<64 hex>" } } }
 *
 * A manifest entry is chosen by platform key: win32-x64, linux-x64,
 * linux-arm64, darwin-x64, darwin-arm64. Entries for other keys are ignored.
 * The checksum is not verified here -- verifying bytes is the downloader's job,
 * and this function never sees the binary. */
#define OMNI_UPDATE_UP_TO_DATE   0  /* this build or older, same ABI        */
#define OMNI_UPDATE_AVAILABLE    1  /* a newer build on this ABI            */
#define OMNI_UPDATE_ABI_MISMATCH 2  /* a build on another wire format       */
#define OMNI_UPDATE_INVALID      3  /* not a release manifest               */
#define OMNI_UPDATE_BAD_ARG     -1  /* null with a length, or non-UTF-8     */

/* Compare a manifest with the running library.
 *
 * `manifest` is `len` bytes of UTF-8 and need not be NUL-terminated. On success
 * the version it names is copied into `out_version` as a NUL-terminated string
 * (truncated to out_cap - 1) and the return value is one of the three statuses
 * above. `out_version` may be NULL and `out_cap` may be 0 when only the verdict
 * is wanted. Returns OMNI_UPDATE_BAD_ARG for a null manifest with a nonzero
 * length, non-UTF-8 bytes, or a document over 1 MiB, and OMNI_UPDATE_INVALID
 * when the bytes do not parse as a manifest.
 *
 * This export is additive within ABI version 4: it introduces no struct and
 * changes no existing signature, so a host that only calls the older functions
 * remains correct. */
OMNI_API int32_t OMNI_CALL omni_check_update(const char *manifest,
                                             size_t len,
                                             char *out_version,
                                             size_t out_cap);

/* Create an engine. Returns NULL if allocation itself failed. */
OMNI_API OmniEngine *OMNI_CALL omni_engine_new(void);

/* Destroy an engine. NULL is accepted and does nothing. Do not use the
 * handle again, and do not free it twice -- including for any OMNI_STRING
 * result still pointing into it, and including every host callback registered
 * on it, whose user_data the engine never owned and so never frees.
 *
 * Called from inside one of this engine's own callbacks, it does nothing at
 * all: the handle is borrowed for the duration of that callback, and destroying
 * it there would free memory the caller is still executing on (caller rule 9).
 * Freeing from the ordinary path is always safe. */
OMNI_API void OMNI_CALL omni_engine_free(OmniEngine *engine);

/* Evaluate `src` (exactly `len` bytes, UTF-8, need not be NUL-terminated)
 * and write the result to *out.
 * Returns OMNI_OK on success. out is untouched when the status is not
 * OMNI_OK.
 * A `len` of 0 is a valid empty program and yields OMNI_UNDEF; `src` may then
 * be NULL, which is what hosts such as Node do with an empty buffer.
 * When out->tag is OMNI_STRING, out->payload.ptr follows caller rule 1.
 *
 * A program that calls a name no host function holds is OMNI_ERR_COMPILE, and
 * one whose callback returned OMNI_HOST_ERR is OMNI_ERR_HOST -- read
 * omni_last_error() for the function's name and the line of the call. Called
 * from inside a callback on the same handle, it is OMNI_ERR_REENTRANT and the
 * running execution is left untouched. */
OMNI_API uint32_t OMNI_CALL omni_exec(OmniEngine *engine,
                                      const char *src,
                                      size_t len,
                                      OmniValue *out);

/* Execute a source task on a distributed worker pool. nodes is a caller-owned
 * array of node_count C strings (host:port). psk must point to exactly 32 bytes.
 * The call retries transport faults, writes task errors as OMNI_ERR_COMPILE or
 * OMNI_ERR_RUNTIME, and returns OMNI_ERR_CLUSTER for pool/protocol failures.
 * The optional error buffer receives a NUL-terminated diagnostic. */
OMNI_API uint32_t OMNI_CALL omni_exec_remote(OmniEngine *engine,
                                             const char *const *nodes,
                                             size_t node_count,
                                             const uint8_t *psk,
                                             size_t psk_len,
                                             const char *src,
                                             size_t len,
                                             uint64_t gas,
                                             uint32_t retries,
                                             OmniValue *out,
                                             char *error,
                                             size_t error_cap);

/* Evaluate and require a numeric result.
 * Returns OMNI_ERR_RUNTIME if the program produced a non-number -- including
 * a string, which is what omni_exec() and omni_str_copy() are for. */
OMNI_API uint32_t OMNI_CALL omni_exec_number(OmniEngine *engine,
                                             const char *src,
                                             size_t len,
                                             double *out);

/* Evaluate `count` programs in one crossing. See caller rule 11.
 *
 * `sources` is an array of `count` pointers, `lengths` an array of `count`
 * byte counts, paired by index. Neither string needs a NUL terminator; each
 * must be UTF-8. A length of 0 is a valid empty program and its pointer may
 * then be NULL, exactly as in omni_exec().
 *
 * `out` receives `count` OmniValues and `statuses` `count` uint32_t status
 * codes, both caller-owned and both written for every index even when an
 * earlier item failed. A failed item's value is OMNI_UNDEF.
 *
 * Returns OMNI_OK when every item succeeded, otherwise the status of the FIRST
 * failing item. Returns OMNI_ERR_ARG without writing anything for a NULL
 * engine, out or statuses array, a count above OMNI_MAX_BATCH, a NULL sources
 * or lengths array with a nonzero count, an over-long length, or non-UTF-8 in
 * any item. `count` of 0 returns OMNI_OK, writes nothing, reclaims nothing, and
 * allows sources and lengths to be NULL.
 *
 * Every OMNI_STRING in `out` follows caller rule 1, and all of them are live
 * together until your next omni_exec(), omni_exec_many() or
 * omni_exec_parallel(). From inside a callback on this handle it returns
 * OMNI_ERR_REENTRANT and runs nothing. */
OMNI_API uint32_t OMNI_CALL omni_exec_many(OmniEngine *engine,
                                           const char *const *sources,
                                           const size_t *lengths,
                                           size_t count,
                                           OmniValue *out,
                                           uint32_t *statuses);

/* Evaluate `count` programs across up to max_threads forked engines.
 *
 * The call is synchronous: it returns when every item has finished. It accepts
 * and writes exactly what omni_exec_many() does, in the same source order, and
 * leaves the same per-item errors available to omni_batch_error(). A failing or
 * gas-exhausted item does not stop its neighbours.
 *
 * max_threads of 0 asks for the library's automatic choice -- available
 * parallelism, bounded by count and 64. max_threads of 1 is the sequential
 * path. The handle is borrowed for the whole call, so this is not a licence to
 * call two exports on the same handle concurrently.
 *
 * Host callbacks may run on worker threads. Anything user_data addresses must
 * be safe to use from several threads at once; see caller rule 7.
 *
 * Returns OMNI_OK when every item succeeded, otherwise the status of the FIRST
 * failing item. Argument validation is identical to omni_exec_many(): a NULL
 * engine, out or statuses array, a count above OMNI_MAX_BATCH, a NULL sources
 * or lengths array with a nonzero count, an over-long length, or non-UTF-8 in
 * any item returns OMNI_ERR_ARG and writes nothing. `count` of 0 returns
 * OMNI_OK without writing or reclaiming anything.
 *
 * This export is additive within ABI version 4: it introduces no struct and
 * changes no existing signature. An SDK loading an older library should fall
 * back to omni_exec_many() when the symbol is absent. */
OMNI_API uint32_t OMNI_CALL omni_exec_parallel(OmniEngine *engine,
                                               const char *const *sources,
                                               const size_t *lengths,
                                               size_t count,
                                               uint32_t max_threads,
                                               OmniValue *out,
                                               uint32_t *statuses);

/* Copy the rendered error of item `index` of the most recent batch into buf.
 *
 * Writes at most cap - 1 bytes, always NUL-terminates, and returns the bytes
 * written excluding the terminator -- so a return of cap - 1 means truncated.
 * The text is the rendered error, "line 1: expected a value", as
 * omni_last_error() gives it.
 *
 * Returns 0 without reading or writing when index is past the batch, when that
 * item succeeded, when no batch is current (a single omni_exec() ends its
 * currency), when engine or buf is NULL or cap is 0, and when called from
 * inside a callback on this engine -- a size_t has nowhere to put status 6. */
OMNI_API size_t OMNI_CALL omni_batch_error(const OmniEngine *engine,
                                           size_t index,
                                           char *buf,
                                           size_t cap);

/* Copy a string result into storage the caller owns.
 *
 * Writes min(value->len_bytes, cap) bytes -- where len_bytes is what the
 * engine's own header says, not value->len, so a stale length can under-read
 * but cannot overflow your buffer -- and returns the number written. It writes
 * NO terminator: an OmniEngine string may contain a NUL, and its length is
 * known.
 *
 * Returns 0 without reading or writing anything when value->tag is not
 * OMNI_STRING, when any pointer is NULL, when value->payload.ptr is not an
 * address this engine can account for (another engine's, a redefined host
 * binding's, or one you invented), when cap is 0, or when called from inside a
 * callback on this engine -- a size_t has nowhere to put status 6.
 *
 * `value` is taken by pointer so that a 16-byte struct never has to cross a
 * C-extension calling convention in three different toolkits; you already have
 * it in memory, because that is where you read `len` from. */
OMNI_API size_t OMNI_CALL omni_str_copy(const OmniEngine *engine,
                                        const OmniValue *value,
                                        char *buf,
                                        size_t cap);

/* Bind `name` to a string owned by the host.
 *
 * The engine copies `bytes`, so `bytes` may be freed as soon as this returns,
 * while the binding survives every later omni_exec(). Redefining `name`
 * replaces the binding and releases the previous bytes: a value you kept from
 * before then copies out as zero rather than reading freed memory.
 *
 * Every buffer carries its own length, like the rest of this ABI -- `name` is
 * not required to be NUL-terminated.
 * Returns OMNI_OK, or OMNI_ERR_ARG for an empty or over-long name, non-UTF-8
 * in either buffer, or a payload over OMNI_MAX_STRING_LEN. */
OMNI_API uint32_t OMNI_CALL omni_define_string(OmniEngine *engine,
                                               const char *name,
                                               size_t name_len,
                                               const char *bytes,
                                               size_t bytes_len);

/* Register a function a script can call as name(...).
 *
 * The engine stores `callback` and `user_data` and never dereferences either --
 * user_data travels back to the callback unchanged, so hang whatever you need off
 * it. Redefining `name` replaces both halves together and releases neither, which
 * is why a host unregisters before it frees what user_data pointed at.
 *
 * `arity` is fixed. A script that passes a different count is an
 * OMNI_ERR_RUNTIME naming the function and both counts, produced by the engine
 * without entering host code.
 *
 * Every buffer carries its own length, as elsewhere in this ABI: `name` need not
 * be NUL-terminated.
 * Returns OMNI_OK, OMNI_ERR_ARG for a NULL engine or callback, an empty or
 * over-long name, non-UTF-8 in `name`, or an `arity` above OMNI_MAX_ARGS, and
 * OMNI_ERR_REENTRANT when called from inside one of this engine's callbacks. */
OMNI_API uint32_t OMNI_CALL omni_register_host_fn(OmniEngine *engine,
                                                  const char *name,
                                                  size_t name_len,
                                                  uint32_t arity,
                                                  omni_host_fn callback,
                                                  void *user_data);

/* Drop the host function called `name`; returns 1 if there was one, 0 if there
 * was not (including for a NULL engine, a bad name, or a re-entrant call).
 *
 * This is the call that makes it safe to release the callback's own storage
 * afterwards: a koffi slot, a ctypes trampoline, a C closure. The engine forgets
 * the function at the same moment, so a later call to that name is an ordinary
 * "unknown function" error rather than a call through a dangling pointer. */
OMNI_API size_t OMNI_CALL omni_unregister_host_fn(OmniEngine *engine,
                                                  const char *name,
                                                  size_t name_len);

/* How many host functions this engine currently has. Returns 0 for a NULL
 * engine. A teardown check that the count is back to its baseline is the
 * cheapest proof that a host released every callback it registered. */
OMNI_API size_t OMNI_CALL omni_host_fn_count(const OmniEngine *engine);

/* Load a native plugin and register every function its manifest declares.
 *
 * `path` is `path_len` bytes of UTF-8 (not NUL-terminated) naming a shared
 * library (.dll / .so) built against the plugin ABI: it must export
 * `omni_plugin_manifest` whose bytes pass the strict verification above. On
 * success the plugin's functions join the host-function table and are callable
 * from programs until the engine is freed; the handle keeps the library mapped
 * for that whole span, so do not unload the same module out from under it.
 *
 * Returns OMNI_OK, OMNI_ERR_ARG for a NULL engine or path, an empty or over-long
 * path, non-UTF-8 in `path`, or a library that fails verification -- wrong
 * magic, wrong manifest size, plugin-ABI or engine-ABI mismatch, a bad plugin
 * or function name, more than OMNI_MAX_PLUGIN_FUNCTIONS functions, a NULL
 * function pointer, an arity above OMNI_MAX_ARGS, or a duplicate name. A
 * refused plugin changes nothing: one that fails partway through registration
 * rolls back the functions it already registered. OMNI_ERR_REENTRANT applies
 * as it does to every engine call. */
OMNI_API uint32_t OMNI_CALL omni_engine_load_plugin(OmniEngine *engine,
                                                    const char *path,
                                                    size_t path_len);

/* How many plugins this engine has loaded. Returns 0 for a NULL engine. The
 * zero-count check after teardown proves no foreign code is still mapped. */
OMNI_API size_t OMNI_CALL omni_plugin_count(const OmniEngine *engine);

/* Allow fs.read_text to read files under path on this engine.
 *
 * The root is canonicalized once, here, so every later containment check
 * compares a canonical path to a canonical root; granting an existing root
 * again is a no-op. Returns OMNI_OK, or OMNI_ERR_ARG for a NULL engine or
 * path, an empty or over-OMNI_MAX_PATH path, non-UTF-8 bytes, or a path
 * that cannot be canonicalized -- the engine records only roots it could
 * resolve, which is the fail-closed half of the sandbox. OMNI_ERR_REENTRANT
 * applies as it does to every engine call. The path is length-prefixed, so
 * it may contain any UTF-8 the host platform allows in a path; no NUL
 * terminator is required or consulted. */
OMNI_API uint32_t OMNI_CALL omni_engine_allow_fs_read(OmniEngine *engine,
                                                      const char *path,
                                                      size_t path_len);

/* Allow fs.write_text to write files under path on this engine.
 *
 * The same contract as omni_engine_allow_fs_read, for the write root set:
 * canonicalized once, deduplicated, refused when unresolvable. */
OMNI_API uint32_t OMNI_CALL omni_engine_allow_fs_write(OmniEngine *engine,
                                                       const char *path,
                                                       size_t path_len);

/* Set the directory `import` resolves modules under, replacing any previous
 * module root. Returns OMNI_OK, or OMNI_ERR_ARG as
 * omni_engine_allow_fs_read does, and OMNI_ERR_REENTRANT from inside a
 * callback on the same handle. */
OMNI_API uint32_t OMNI_CALL omni_engine_set_module_root(OmniEngine *engine,
                                                        const char *path,
                                                        size_t path_len);

/* Fill *out with the engine's counters and OMNI_OK, or return OMNI_ERR_ARG if
 * engine or out is NULL.
 *
 * out->size is always rewritten by the callee: compare it with
 * sizeof(OmniStats) before believing the rest. */
OMNI_API uint32_t OMNI_CALL omni_debug_stats(OmniEngine *engine,
                                             OmniStats *out);

/* Copy the most recent engine error into buf, writing at most cap - 1 bytes
 * and always NUL-terminating. Returns the bytes written, excluding the
 * terminator, so a return of cap - 1 means the text was truncated.
 * The text is the rendered error -- "line 3: division by zero" -- so a host
 * can show the user where the program failed, not only what went wrong.
 * Returns 0 when there is no pending error (including after a success, which
 * clears it), when buf is NULL or cap is 0, and when called from inside a
 * callback on this engine. */
OMNI_API size_t OMNI_CALL omni_last_error(OmniEngine *engine,
                                          char *buf,
                                          size_t cap);

/* Turn per-opcode self-timing on (enabled != 0) or off for this engine.
 *
 * The opcode_calls counts and elapsed_ns in omni_profile() are recorded on
 * every execution; only the opcode_ns split costs a clock read per dispatch,
 * which is why it is opt-in. Returns OMNI_OK, or OMNI_ERR_ARG for a NULL
 * engine and OMNI_ERR_REENTRANT from inside a callback on this handle. */
OMNI_API uint32_t OMNI_CALL omni_engine_set_profiling(OmniEngine *engine,
                                                      uint32_t enabled);

/* Fill *out with the most recent execution's profile and OMNI_OK, or return
 * OMNI_ERR_ARG if engine or out is NULL.
 *
 * out->size is always rewritten by the callee: compare it with
 * sizeof(OmniProfile) before believing the rest, as with omni_debug_stats().
 * From inside a callback on this handle it returns OMNI_ERR_REENTRANT. */
OMNI_API uint32_t OMNI_CALL omni_profile(OmniEngine *engine,
                                         OmniProfile *out);

/* Copy the mnemonic of opcode `index` into buf, writing at most cap - 1 bytes
 * and always NUL-terminating. Returns the bytes written, excluding the
 * terminator; an index of opcode_count or above writes an empty string and
 * returns 0, as do a NULL buf and a cap of 0. */
OMNI_API size_t OMNI_CALL omni_opcode_name(uint32_t index,
                                           char *buf,
                                           size_t cap);

/* Test hook: deliberately panics inside the engine so a host can verify the
 * boundary returns OMNI_ERR_PANIC instead of crashing the process.
 * It is not part of the API and is named to make that obvious.
 * From inside a callback it returns OMNI_ERR_REENTRANT instead -- the panic
 * would have to unwind through the foreign frames that are calling it, which is
 * exactly what rule 9 exists to prevent. */
OMNI_API uint32_t OMNI_CALL omni_debug_panic(OmniEngine *engine);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* OMNI_H */
