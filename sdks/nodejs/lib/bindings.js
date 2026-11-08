'use strict';

/**
 * koffi declarations for OmniEngine's C ABI.
 *
 * The prototypes below are transcribed from crates/omni-ffi/include/omni.h and
 * are the reason that header exists: koffi compiles the signature string, so a
 * mismatch between the SDK and the library shows up as garbage numbers or a
 * crash instead of a type error. Three guards make the risk visible at load time:
 *
 *   - the ABI level is read from the library and compared with what this file
 *     declares, and a mismatch refuses to load,
 *   - the struct and union sizes and every field offset are asserted against the
 *     ABI, and
 *   - every pointer parameter is declared explicitly, so a handle is never
 *     rounded through a JavaScript number.
 *
 * Handles come back from koffi as BigInt. That is deliberate and load-bearing:
 * BigInt keeps all 64 bits. Converting one with Number() would truncate any
 * address above 2^53, which works in a demo and corrupts memory in production.
 * The one address this SDK could read -- a string result's `payload.ptr` -- is
 * never converted at all: `omni_str_copy` takes the value struct, not the
 * pointer, so the bytes cross into a Buffer and the host never holds an address.
 *
 * Type names must be declared before they appear in a signature string. Passing
 * an undeclared name (the opaque `OmniEngine *`, for instance) is not a type
 * error koffi throws -- it aborts the process. Hence `void *` for the handle,
 * which is also exactly what an opaque pointer is to C.
 */

const path = require('node:path');
const { resolveLibrary, OmniLibraryError } = require('./discover');

// ---------------------------------------------------------------------------
// Constants: crates/omni-ffi/src/lib.rs, include/omni.h, omni/_ffi.py
// ---------------------------------------------------------------------------

/** Status codes returned by every ABI call. */
const STATUS = Object.freeze({
  OK: 0,
  ERR_COMPILE: 1,
  ERR_RUNTIME: 2,
  ERR_PANIC: 3,
  ERR_ARG: 4,
  ERR_HOST: 5,
  ERR_REENTRANT: 6,
});

const STATUS_NAMES = Object.freeze({
  0: 'ok',
  1: 'compile',
  2: 'runtime',
  3: 'panic',
  4: 'argument',
  5: 'host',
  6: 'reentrant',
});

/** Value tags on OmniValue. */
const TAG = Object.freeze({
  NUMBER: 0,
  NIL: 1,
  BOOL: 2,
  UNDEF: 3,
  STRING: 4,
});

const TAG_NAMES = Object.freeze({
  0: 'number',
  1: 'nil',
  2: 'bool',
  3: 'undef',
  4: 'string',
});

/**
 * Wire format of the value struct this file reads.
 *
 * Version 1 had no `len` field and no string arm, so a v1 library read through
 * these declarations yields `len` as padding -- wrong numbers, not an error.
 * `loadBindings` compares the library's own answer with this constant.
 *
 * Version 3 changes no layout: it adds the host callback exports and statuses.
 * The bump is a guard rail -- a v2 library does not have `omni_register_host_fn`,
 * so reading a script that calls a host function fails as a runtime error, not
 * as a missing symbol.
 *
 * Version 4 changes no layout either. It is `omni_exec_many` and
 * `omni_batch_error`, which a v3 SDK cannot declare, and the fact that
 * `OmniStats.execs` now counts batch items as well as single calls -- a v3 host
 * reading it as "number of omni_exec calls" would draw a wrong conclusion from a
 * correct number.
 */
const ABI_VERSION = 4;

/** The ABI rejects any length above these, before reading a single byte. */
const MAX_SOURCE_LEN = 2 ** 28;
const MAX_STRING_LEN = 16 * 1024 * 1024;
const MAX_IDENTIFIER_LEN = 32;

/** How many arguments a host function may take, and how deep calls may nest. */
const MAX_ARGS = 16;
const MAX_CALL_DEPTH = 64;

/** Items one `omni_exec_many` call may carry. */
const MAX_BATCH = 4096;

/**
 * Length of the fixed per-opcode arrays in `OmniProfile`. Mirrors
 * `OMNI_MAX_OPCODES`: the engine writes only the first `opcode_count` slots,
 * and the rest stay zero.
 */
const MAX_OPCODES = 64;

/** What a host callback returns: success, or failure with a message in `out`. */
const HOST_OK = 0;
const HOST_ERR = 1;

/**
 * `omni_check_update`'s answers, an additive export inside ABI 4.
 *
 * The structs and every other signature are unchanged, so a library built before
 * this export still loads; only the release check is unavailable.
 */
const UPDATE = Object.freeze({
  UP_TO_DATE: 0,
  AVAILABLE: 1,
  ABI_MISMATCH: 2,
  INVALID: 3,
  BAD_ARG: -1,
});

const UPDATE_STATUS_NAMES = Object.freeze({
  0: 'up-to-date',
  1: 'available',
  2: 'abi-mismatch',
  3: 'invalid',
  '-1': 'bad-argument',
});

/** Largest manifest `omni_check_update` will read, mirroring the core limit. */
const MAX_MANIFEST_LEN = 1 << 20;

function statusName(status) {
  return STATUS_NAMES[status] || `status-${status}`;
}

function tagName(tag) {
  return TAG_NAMES[tag] || `tag-${tag}`;
}

/** A program OmniEngine rejected, or a call the host made incorrectly. */
class OmniError extends Error {
  /**
   * @param {number} status raw u32 from the ABI
   * @param {string} message engine text, already copied out of the engine
   */
  constructor(status, message) {
    // `message` stays exactly what the engine said, matching the Python SDK's
    // OmniError.message. Error.message cannot hold both the raw text and a
    // prefix, so the prefix lives in toString() -- overwriting it here would
    // quietly duplicate the kind in every stack trace.
    super(message || '');
    this.name = 'OmniError';
    this.status = status;
    this.kind = statusName(status);
  }

  toString() {
    return this.message ? `${this.kind}: ${this.message}` : this.kind;
  }
}

// ---------------------------------------------------------------------------
// Loading and declaring
// ---------------------------------------------------------------------------

const cache = new Map();

/**
 * Load the library, attach every signature to it, and verify its ABI level.
 *
 * @param {string} [libraryPath] absolute path; resolved from the environment by default
 * @returns {Bindings}
 */
function loadBindings(libraryPath) {
  const resolved = libraryPath ? path.resolve(libraryPath) : resolveLibrary();
  const hit = cache.get(resolved);
  if (hit) return hit;

  let koffi;
  try {
    koffi = require('koffi');
  } catch (err) {
    throw new OmniLibraryError(
      `the Node SDK needs koffi to call into OmniEngine, and requiring it failed: ${err.message}\n` +
        'Install it with:  npm install koffi\n' +
        'It ships prebuilt binaries, so no compiler is needed.'
    );
  }

  const bindings = declare(koffi, koffi.load(resolved), resolved);
  cache.set(resolved, bindings);
  return bindings;
}

/** Register types and prototypes against one loaded library. */
function declare(koffi, lib, resolvedPath) {
  // Declared in dependency order: a signature string may only name types that
  // already exist.
  const OmniPayload = koffi.union('OmniPayload', {
    number: 'double',
    ptr: 'void *',
  });

  const OmniValue = koffi.struct('OmniValue', {
    tag: 'uint32_t',
    len: 'uint32_t',
    payload: 'OmniPayload',
  });

  // `last_exec_micros` occupies the four bytes the C compiler padded after
  // `size`, so the struct stays 72 bytes and every other offset is unchanged.
  const OmniStats = koffi.struct('OmniStats', {
    size: 'uint32_t',
    last_exec_micros: 'uint32_t',
    execs: 'uint64_t',
    strings: 'uint64_t',
    arena_bytes_in_use: 'uint64_t',
    arena_bytes_reserved: 'uint64_t',
    arena_chunks: 'uint64_t',
    egress_bytes_in_use: 'uint64_t',
    egress_bytes_reserved: 'uint64_t',
    egress_high_water: 'uint64_t',
  });

  // The profiler's answer struct: two fixed 64-slot arrays of u64, so 1064
  // bytes exactly. Declared unconditionally -- the struct exists in this SDK
  // even when the loaded library predates the exports that fill it.
  const OmniProfile = koffi.struct('OmniProfile', {
    size: 'uint32_t',
    opcode_count: 'uint32_t',
    total_instructions: 'uint64_t',
    elapsed_ns: 'uint64_t',
    peak_bytes: 'uint64_t',
    reserved: 'uint64_t',
    opcode_calls: koffi.array('uint64_t', MAX_OPCODES),
    opcode_ns: koffi.array('uint64_t', MAX_OPCODES),
  });

  // The callback signature. Declared as a proto so koffi knows how to build the
  // trampoline when `koffi.register` is called on a JavaScript function. It is
  // never named inside a signature string below -- koffi rejects a proto used as
  // a parameter type, so the registry calls take `void *` and are handed the
  // BigInt address that `koffi.register` returns.
  const OmniHostFn = koffi.proto(
    'int32_t OmniHostFn(void *user_data, const OmniValue *args, uint32_t argc, OmniValue *out)'
  );

  // Fail here rather than returning wrong numbers later if the two sides drift.
  // koffi reports sizes and offsets for its own type system, which is the only
  // way to compare a header written in Rust with a struct declared in JS.
  const layout = [
    ['koffi.sizeof(OmniPayload)', koffi.sizeof(OmniPayload), 8],
    ['koffi.sizeof(OmniValue)', koffi.sizeof(OmniValue), 16],
    ['koffi.offsetof(OmniValue, tag)', koffi.offsetof(OmniValue, 'tag'), 0],
    ['koffi.offsetof(OmniValue, len)', koffi.offsetof(OmniValue, 'len'), 4],
    ['koffi.offsetof(OmniValue, payload)', koffi.offsetof(OmniValue, 'payload'), 8],
    ['koffi.sizeof(OmniStats)', koffi.sizeof(OmniStats), 72],
    ['koffi.offsetof(OmniStats, size)', koffi.offsetof(OmniStats, 'size'), 0],
    ['koffi.offsetof(OmniStats, last_exec_micros)', koffi.offsetof(OmniStats, 'last_exec_micros'), 4],
    ['koffi.offsetof(OmniStats, execs)', koffi.offsetof(OmniStats, 'execs'), 8],
    ['koffi.offsetof(OmniStats, egress_high_water)', koffi.offsetof(OmniStats, 'egress_high_water'), 64],
    ['koffi.sizeof(OmniProfile)', koffi.sizeof(OmniProfile), 1064],
    ['koffi.offsetof(OmniProfile, size)', koffi.offsetof(OmniProfile, 'size'), 0],
    ['koffi.offsetof(OmniProfile, opcode_count)', koffi.offsetof(OmniProfile, 'opcode_count'), 4],
    ['koffi.offsetof(OmniProfile, total_instructions)', koffi.offsetof(OmniProfile, 'total_instructions'), 8],
    ['koffi.offsetof(OmniProfile, elapsed_ns)', koffi.offsetof(OmniProfile, 'elapsed_ns'), 16],
    ['koffi.offsetof(OmniProfile, peak_bytes)', koffi.offsetof(OmniProfile, 'peak_bytes'), 24],
    ['koffi.offsetof(OmniProfile, reserved)', koffi.offsetof(OmniProfile, 'reserved'), 32],
    ['koffi.offsetof(OmniProfile, opcode_calls)', koffi.offsetof(OmniProfile, 'opcode_calls'), 40],
    ['koffi.offsetof(OmniProfile, opcode_ns)', koffi.offsetof(OmniProfile, 'opcode_ns'), 552],
  ];
  for (const [name, actual, expected] of layout) {
    if (actual !== expected) {
      throw new OmniError(
        STATUS.ERR_ARG,
        `${name} is ${actual}, expected ${expected}; the SDK and the library disagree about the ABI`
      );
    }
  }

  // Resolved first, and on its own, because this is the one export whose absence
  // has an explanation a host can act on. koffi throws when a function is
  // declared rather than when it is called, so a pre-Phase-2 artifact fails here
  // -- and an error thrown from a `lib.func` inside the object literal below
  // would arrive as koffi's own text, with no hint that the fix is a rebuild.
  let abiVersion;
  try {
    abiVersion = lib.func('uint32_t omni_abi_version()');
  } catch (err) {
    throw new OmniLibraryError(
      `${resolvedPath} does not export omni_abi_version, so it predates the ABI level this SDK needs ` +
        `(${ABI_VERSION}). Rebuild it:\n    cargo build -p omni-ffi\n(${err.message})`
    );
  }

  // Additive in ABI 4, so it is declared only when the library exports it. A
  // library built before the update check still serves every other call, and
  // checkUpdates() turns the missing symbol into one clear error at the moment
  // someone asks for a release, not at load time. koffi throws when a function
  // is declared rather than when it is called, which is why this is a try.
  let checkUpdate = null;
  try {
    checkUpdate = lib.func(
      'int32_t omni_check_update(const void *manifest, size_t len, char *out_version, size_t out_cap)'
    );
  } catch {
    checkUpdate = null;
  }

  // Additive in ABI 4 as well. A library built before concurrent dispatch keeps
  // every sequential call; Engine.runParallel() falls back to `execMany` when
  // this is null rather than failing at the first parallel request.
  let execParallel = null;
  try {
    execParallel = lib.func(
      'uint32_t omni_exec_parallel(void *engine, const void *sources, const void *lengths, size_t count, uint32_t max_threads, void *out, void *statuses)'
    );
  } catch {
    execParallel = null;
  }

  // Additive in ABI 4 too, after the profiler shipped. A library built before
  // Phase 9 keeps every other call; Engine.setProfiling() and Engine.profile()
  // turn the missing symbol into one clear error at the moment profiling is
  // asked for, not at load time.
  let setProfiling = null;
  try {
    setProfiling = lib.func('uint32_t omni_engine_set_profiling(void *engine, uint32_t enabled)');
  } catch {
    setProfiling = null;
  }
  let profile = null;
  try {
    profile = lib.func('uint32_t omni_profile(void *engine, OmniProfile *out)');
  } catch {
    profile = null;
  }
  let opcodeName = null;
  try {
    opcodeName = lib.func('size_t omni_opcode_name(uint32_t index, char *buf, size_t cap)');
  } catch {
    opcodeName = null;
  }

  const bindings = {
    koffi,
    lib,
    path: resolvedPath,
    OmniPayload,
    OmniValue,
    OmniStats,
    OmniProfile,
    OmniHostFn,
    statusName,
    tagName,
    STATUS,
    TAG,
    ABI_VERSION,
    MAX_SOURCE_LEN,
    MAX_STRING_LEN,
    MAX_IDENTIFIER_LEN,
    MAX_ARGS,
    MAX_CALL_DEPTH,
    MAX_BATCH,
    MAX_OPCODES,
    HOST_OK,
    HOST_ERR,
    UPDATE,
    UPDATE_STATUS_NAMES,
    MAX_MANIFEST_LEN,
    checkUpdate,
    abiVersion,
    version: lib.func('const char *omni_version()'),
    engineNew: lib.func('void *omni_engine_new()'),
    engineFree: lib.func('void omni_engine_free(void *engine)'),
    exec: lib.func('uint32_t omni_exec(void *engine, const void *src, size_t len, OmniValue *out)'),
    execNumber: lib.func('uint32_t omni_exec_number(void *engine, const void *src, size_t len, double *out)'),
    // Batch: four caller-owned arrays, so every parameter is `void *` and this
    // SDK does the marshalling. Declaring `OmniValue *` or `uint32_t *` would
    // invite koffi to convert a JavaScript object into a temporary struct and
    // hand the engine *that* address, which is a write that goes nowhere. As
    // `void *`, koffi passes the memory we point it at: a BigUint64Array of
    // source addresses, a BigUint64Array of lengths, a koffi.alloc'd struct
    // array, and a Uint32Array of statuses. `sources` is therefore an array of
    // pointers rather than an array of strings -- the ABI reads `count` of each
    // and validates every source before running any of them.
    execMany: lib.func('uint32_t omni_exec_many(void *engine, const void *sources, const void *lengths, size_t count, void *out, void *statuses)'),
    execParallel,
    setProfiling,
    profile,
    opcodeName,
    // `char *` for the destination: koffi needs to know it is writable, and a
    // Buffer is what the caller owns. The ABI writes no NUL and reads no NUL --
    // `cap` and the return value are the whole contract.
    strCopy: lib.func('size_t omni_str_copy(const void *engine, const OmniValue *value, char *buf, size_t cap)'),
    // Both inputs are `const void *`, not `const char *`: the ABI takes an
    // explicit length for each buffer and may be handed bytes with a NUL in
    // them, which a string type would silently cut short.
    defineString: lib.func('uint32_t omni_define_string(void *engine, const void *name, size_t nameLen, const void *bytes, size_t bytesLen)'),
    registerHostFn: lib.func('uint32_t omni_register_host_fn(void *engine, const void *name, size_t nameLen, uint32_t arity, void *cb, void *user_data)'),
    unregisterHostFn: lib.func('size_t omni_unregister_host_fn(void *engine, const void *name, size_t nameLen)'),
    hostFnCount: lib.func('size_t omni_host_fn_count(const void *engine)'),
    debugStats: lib.func('uint32_t omni_debug_stats(void *engine, OmniStats *out)'),
    lastError: lib.func('size_t omni_last_error(void *engine, void *buf, size_t cap)'),
    // Same contract as omni_last_error, one index over: after a batch,
    // last_error only ever holds the final item's message, so a host that wants
    // the reason item 7 failed asks for it by number.
    batchError: lib.func('size_t omni_batch_error(const void *engine, size_t index, void *buf, size_t cap)'),
    debugPanic: lib.func('uint32_t omni_debug_panic(void *engine)'),
  };

  checkAbi(abiVersion(), resolvedPath);
  return bindings;
}

/**
 * Refuse a library whose value layout this file cannot read.
 *
 * Split out from `declare` so a test can drive it with any number: the case it
 * exists for -- an artifact built from an older commit -- is not one a test
 * suite can produce without rebuilding the workspace twice.
 */
function checkAbi(declared, source) {
  if (declared === ABI_VERSION) return declared;
  throw new OmniLibraryError(
    `${source} speaks ABI version ${declared}; this SDK is written for ABI version ${ABI_VERSION}. ` +
      'The value layout differs, so reading one as the other produces wrong numbers rather than an ' +
      'error. Rebuild the library, or install the SDK version that matches it:\n    cargo build -p omni-ffi'
  );
}

module.exports = {
  loadBindings,
  declare,
  checkAbi,
  OmniError,
  OmniLibraryError,
  STATUS,
  STATUS_NAMES,
  TAG,
  TAG_NAMES,
  statusName,
  tagName,
  ABI_VERSION,
  MAX_SOURCE_LEN,
  MAX_STRING_LEN,
  MAX_IDENTIFIER_LEN,
  MAX_ARGS,
  MAX_CALL_DEPTH,
  MAX_BATCH,
  MAX_OPCODES,
  HOST_OK,
  HOST_ERR,
  UPDATE,
  UPDATE_STATUS_NAMES,
  MAX_MANIFEST_LEN,
};
