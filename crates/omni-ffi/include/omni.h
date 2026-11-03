/*
 * omni.h -- the C ABI of OmniEngine.
 *
 * Hand-written and authoritative. It mirrors
 * crates/omni-ffi/src/lib.rs, and the Rust side has compile-time asserts
 * pinning OmniValue's size and field offsets so the two cannot drift silently.
 *
 * crates/omni-ffi/cbindgen.toml can regenerate this surface with:
 *
 *     cbindgen --config crates/omni-ffi/cbindgen.toml crates/omni-ffi \
 *              --output crates/omni-ffi/include/omni.h
 *
 * That command overwrites this file, so review the diff before committing it.
 *
 * ## What a caller must know
 *
 * 1. Results come back by value. No OmniEngine-internal pointer is ever
 *    returned, because the engine frees its whole arena at the start of each
 *    omni_exec(). Anything you get from omni_exec() is yours to keep.
 *
 * 2. Error text is copied into storage you own. Call omni_last_error() with a
 *    buffer; it truncates rather than overflowing, and always NUL-terminates.
 *
 * 3. `src` is not required to be NUL-terminated -- `len` decides what is read.
 *    It must be valid UTF-8. Malformed UTF-8 returns OMNI_ERR_ARG without
 *    touching the engine.
 *
 * 4. A Rust panic is converted to OMNI_ERR_PANIC at this boundary and never
 *    unwinds into your process. After OMNI_ERR_PANIC the engine is still
 *    usable, but treat the handle as suspect and recreate it if you can.
 *
 * 5. One engine handle is one thread. Do not share a handle across threads
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

#include <stddef.h>  /* size_t */
#include <stdint.h>  /* uint32_t */

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

/* ---- status codes: the u32 returned by every call ---- */

#define OMNI_OK            0u /* the call succeeded                       */
#define OMNI_ERR_COMPILE   1u /* source did not lex or parse              */
#define OMNI_ERR_RUNTIME   2u /* parsed, then failed while evaluating     */
#define OMNI_ERR_PANIC     3u /* a Rust panic was caught at the boundary  */
#define OMNI_ERR_ARG       4u /* null pointer, bad length, or bad UTF-8   */

/* ---- value tags: OmniValue::tag ---- */

#define OMNI_NUMBER        0u /* `number` holds the value                 */
#define OMNI_NIL           1u /* no payload                               */
#define OMNI_BOOL          2u /* `number` is 0.0 or 1.0                   */
#define OMNI_UNDEF         3u /* no payload; an empty program             */

/* A single 16-byte result passed by value.
 *
 * Not a union: no string values exist yet, so a pointer arm would be a
 * variant nothing populates that every host must still read unsafely.
 * It arrives with strings in Phase 2. */
typedef struct OmniValue {
  uint32_t tag;    /* one of the OMNI_* tags above */
  double number;   /* payload; 0.0 for tags that carry none */
} OmniValue;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(OmniValue) == 16, "OmniValue must stay 16 bytes");
_Static_assert(offsetof(OmniValue, number) == 8, "OmniValue::number must stay at offset 8");
#elif defined(__cplusplus) && (__cplusplus >= 201103L)
static_assert(sizeof(OmniValue) == 16, "OmniValue must stay 16 bytes");
static_assert(offsetof(OmniValue, number) == 8, "OmniValue::number must stay at offset 8");
#endif

/* Opaque. Never dereference; the layout is a Rust private detail. */
typedef struct OmniEngine OmniEngine;

/* Version string, static and NUL-terminated. Never free it. */
OMNI_API const char *OMNI_CALL omni_version(void);

/* Create an engine. Returns NULL if allocation itself failed. */
OMNI_API OmniEngine *OMNI_CALL omni_engine_new(void);

/* Destroy an engine. NULL is accepted and does nothing. Do not use the
 * handle again, and do not free it twice. */
OMNI_API void OMNI_CALL omni_engine_free(OmniEngine *engine);

/* Evaluate `src` (exactly `len` bytes, UTF-8, need not be NUL-terminated)
 * and write the result to *out.
 * Returns OMNI_OK on success. out is untouched when the status is not
 * OMNI_OK.
 * A `len` of 0 is a valid empty program and yields OMNI_UNDEF; `src` may then
 * be NULL, which is what hosts such as Node do with an empty buffer. */
OMNI_API uint32_t OMNI_CALL omni_exec(OmniEngine *engine,
                                      const char *src,
                                      size_t len,
                                      OmniValue *out);

/* Evaluate and require a numeric result.
 * Returns OMNI_ERR_RUNTIME if the program produced a non-number. */
OMNI_API uint32_t OMNI_CALL omni_exec_number(OmniEngine *engine,
                                             const char *src,
                                             size_t len,
                                             double *out);

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
