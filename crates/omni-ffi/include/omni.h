/*
 * omni.h -- the C ABI of OmniEngine, version 2.
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
 *    start of the next omni_exec() on the same engine, so the bytes stay valid
 *    memory for the life of the handle and the documented contract is:
 *
 *        valid until your next omni_exec() on this engine.
 *
 *    Reading it after that is a wrong read, never a crash: you get whatever the
 *    later execution wrote at those addresses. To keep a string, copy it with
 *    omni_str_copy() into storage you own.
 *
 *    omni_str_copy() checks the address against the engine you pass. A pointer
 *    from another engine, a host binding that has since been redefined, or a
 *    number you invented all copy zero bytes instead of faulting. That is why
 *    you call it instead of memcpy'ing from payload.ptr.
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
 * 7. One engine handle is one thread. Do not share a handle across threads
 *    without your own lock.
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

/* Wire format, not the crate version. See caller rule 5. */
#define OMNI_ABI_VERSION 2u

/* ---- status codes: the u32 returned by every call ---- */

#define OMNI_OK            0u /* the call succeeded                       */
#define OMNI_ERR_COMPILE   1u /* source did not lex or parse              */
#define OMNI_ERR_RUNTIME   2u /* parsed, then failed while evaluating     */
#define OMNI_ERR_PANIC     3u /* a Rust panic was caught at the boundary  */
#define OMNI_ERR_ARG       4u /* null pointer, bad length, or bad UTF-8   */

/* There is deliberately no OMNI_ERR_MEMORY. A program that builds too long a
 * string is the program's fault (OMNI_ERR_RUNTIME); a host that passes too long
 * an argument is a bad call (OMNI_ERR_ARG); a real allocation failure aborts. A
 * fifth code would let callers branch on a condition that cannot tell them
 * anything the two existing codes do not. */

/* ---- value tags: OmniValue::tag ---- */

#define OMNI_NUMBER        0u /* payload.number holds the value           */
#define OMNI_NIL           1u /* no payload                               */
#define OMNI_BOOL          2u /* payload.number is 0.0 or 1.0             */
#define OMNI_UNDEF         3u /* no payload; an empty program             */
#define OMNI_STRING        4u /* payload.ptr, len bytes, engine-owned     */

/* ---- limits: mirrors of omni_core::{object::MAX_STRING_LEN, engine::MAX_IDENTIFIER_LEN} ----
 *
 * Restated here because C cannot read a Rust constant; the engine enforces them
 * and refuses a longer payload (OMNI_ERR_ARG from a host, OMNI_ERR_RUNTIME from
 * a program), so a wrong number in this header costs a rejected call rather
 * than memory corruption. */

#define OMNI_MAX_STRING_LEN     (16u * 1024u * 1024u)
#define OMNI_MAX_IDENTIFIER_LEN 32u

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
 * to stop, rather than reading a counter from the wrong offset forever. */
typedef struct OmniStats {
  uint32_t size;                /* sizeof(OmniStats), written by the callee */
  uint64_t execs;               /* omni_exec() calls started                */
  uint64_t strings;             /* string blocks built, cumulative          */
  uint64_t arena_bytes_in_use;
  uint64_t arena_bytes_reserved;
  uint64_t arena_chunks;
  uint64_t egress_bytes_in_use;
  uint64_t egress_bytes_reserved;
  uint64_t egress_high_water;   /* largest single execution's egress demand */
} OmniStats;

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
OMNI_STATIC_ASSERT(offsetof(OmniStats, execs) == 8, "OmniStats::execs must stay at offset 8");
OMNI_STATIC_ASSERT(offsetof(OmniStats, egress_high_water) == 64, "OmniStats tail must stay at 64");

/* Opaque. Never dereference; the layout is a Rust private detail. */
typedef struct OmniEngine OmniEngine;

/* ABI level of the loaded library. Compare with OMNI_ABI_VERSION before
 * trusting any other symbol in this header. */
OMNI_API uint32_t OMNI_CALL omni_abi_version(void);

/* Version string, static and NUL-terminated. Never free it. */
OMNI_API const char *OMNI_CALL omni_version(void);

/* Create an engine. Returns NULL if allocation itself failed. */
OMNI_API OmniEngine *OMNI_CALL omni_engine_new(void);

/* Destroy an engine. NULL is accepted and does nothing. Do not use the
 * handle again, and do not free it twice -- including for any OMNI_STRING
 * result still pointing into it. */
OMNI_API void OMNI_CALL omni_engine_free(OmniEngine *engine);

/* Evaluate `src` (exactly `len` bytes, UTF-8, need not be NUL-terminated)
 * and write the result to *out.
 * Returns OMNI_OK on success. out is untouched when the status is not
 * OMNI_OK.
 * A `len` of 0 is a valid empty program and yields OMNI_UNDEF; `src` may then
 * be NULL, which is what hosts such as Node do with an empty buffer.
 * When out->tag is OMNI_STRING, out->payload.ptr follows caller rule 1. */
OMNI_API uint32_t OMNI_CALL omni_exec(OmniEngine *engine,
                                      const char *src,
                                      size_t len,
                                      OmniValue *out);

/* Evaluate and require a numeric result.
 * Returns OMNI_ERR_RUNTIME if the program produced a non-number -- including
 * a string, which is what omni_exec() and omni_str_copy() are for. */
OMNI_API uint32_t OMNI_CALL omni_exec_number(OmniEngine *engine,
                                             const char *src,
                                             size_t len,
                                             double *out);

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
 * binding's, or one you invented), or when cap is 0.
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
 * clears it) or when buf is NULL or cap is 0. */
OMNI_API size_t OMNI_CALL omni_last_error(OmniEngine *engine,
                                          char *buf,
                                          size_t cap);

/* Test hook: deliberately panics inside the engine so a host can verify the
 * boundary returns OMNI_ERR_PANIC instead of crashing the process.
 * It is not part of the API and is named to make that obvious. */
OMNI_API uint32_t OMNI_CALL omni_debug_panic(OmniEngine *engine);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* OMNI_H */
