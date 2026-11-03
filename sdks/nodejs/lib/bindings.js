'use strict';

/**
 * koffi declarations for OmniEngine's C ABI.
 *
 * The prototypes below are transcribed from crates/omni-ffi/include/omni.h and
 * are the reason that header exists: koffi compiles the signature string, so a
 * mismatch between the SDK and the library shows up as garbage numbers or a
 * crash instead of a type error. Two guards make the risk visible at load time:
 *
 *   - the struct size and field offsets are asserted against the ABI, and
 *   - every pointer parameter is declared explicitly, so a handle is never
 *     rounded through a JavaScript number.
 *
 * Handles come back from koffi as BigInt. That is deliberate and load-bearing:
 * BigInt keeps all 64 bits. Converting one with Number() would truncate any
 * address above 2^53, which works in a demo and corrupts memory in production.
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
});

const STATUS_NAMES = Object.freeze({
  0: 'ok',
  1: 'compile',
  2: 'runtime',
  3: 'panic',
  4: 'argument',
});

/** Value tags on OmniValue. */
const TAG = Object.freeze({
  NUMBER: 0,
  NIL: 1,
  BOOL: 2,
  UNDEF: 3,
});

const TAG_NAMES = Object.freeze({
  0: 'number',
  1: 'nil',
  2: 'bool',
  3: 'undef',
});

/** The ABI rejects any length above this, before reading a single byte. */
const MAX_SOURCE_LEN = 2 ** 28;

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
 * Load the library and attach every signature to it.
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
  const OmniValue = koffi.struct('OmniValue', {
    tag: 'uint32_t',
    number: 'double',
  });

  // Fail here rather than returning wrong numbers later if the two sides drift.
  if (koffi.sizeof && koffi.sizeof(OmniValue) !== 16) {
    throw new OmniError(
      STATUS.ERR_ARG,
      `koffi sees OmniValue as ${koffi.sizeof(OmniValue)} bytes, expected 16; the SDK and the library disagree about the ABI`
    );
  }
  if (koffi.offsetof && koffi.offsetof(OmniValue, 'number') !== 8) {
    throw new OmniError(
      STATUS.ERR_ARG,
      `koffi sees OmniValue.number at offset ${koffi.offsetof(OmniValue, 'number')}, expected 8`
    );
  }

  // `const void *` for source, not `const char *`: the ABI takes an explicit
  // length and reads UTF-8 bytes, and a Buffer is exactly those. Declaring a
  // string type would add a NUL-termination assumption the ABI does not have.
  return {
    koffi,
    lib,
    path: resolvedPath,
    OmniValue,
    statusName,
    tagName,
    STATUS,
    TAG,
    MAX_SOURCE_LEN,
    version: lib.func('const char *omni_version()'),
    engineNew: lib.func('void *omni_engine_new()'),
    engineFree: lib.func('void omni_engine_free(void *engine)'),
    exec: lib.func('uint32_t omni_exec(void *engine, const void *src, size_t len, OmniValue *out)'),
    execNumber: lib.func('uint32_t omni_exec_number(void *engine, const void *src, size_t len, double *out)'),
    lastError: lib.func('size_t omni_last_error(void *engine, void *buf, size_t cap)'),
    debugPanic: lib.func('uint32_t omni_debug_panic(void *engine)'),
  };
}

module.exports = {
  loadBindings,
  declare,
  OmniError,
  OmniLibraryError,
  STATUS,
  STATUS_NAMES,
  TAG,
  TAG_NAMES,
  statusName,
  tagName,
  MAX_SOURCE_LEN,
};
