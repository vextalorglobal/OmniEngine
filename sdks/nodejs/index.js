'use strict';

/**
 * OmniEngine from Node.js.
 *
 *     const { Engine } = require('omniengine');
 *
 *     const engine = new Engine();
 *     engine.evaluate('let answer = 40 + 2');            // 42
 *     engine.run('20 / 3').value;                        // 6.666666666666667
 *     engine.run('"omni" + " " + "engine"').value;       // 'omni engine'
 *     engine.close();
 *
 * Or let the handle clean itself up (Node 20.11+, 22, and later):
 *
 *     await using engine = new Engine();
 *
 * Everything this package hands back is JavaScript data. A string result is
 * copied by `omni_str_copy` into a Buffer that belongs to the caller, so no
 * object you can hold refers to the egress block the engine rewinds at the
 * start of the next evaluation:
 *
 *     const kept = engine.run('"kept"').value;
 *     engine.run('"overwritten"');      // `kept` is still 'kept'
 *
 * A host can also give the engine a string of its own, which lives as long as
 * the binding rather than as long as one execution:
 *
 *     engine.defineString('greeting', 'hello');
 *     engine.run('greeting + ", then some"').value;
 *     engine.stats().egressBytesReserved;   // what the buffers cost
 *
 * Build the engine once and this package finds it:
 *
 *     cargo build -p omni-ffi
 *
 * An engine handle belongs to one thread. Node is single-threaded per isolate,
 * so that is usually free -- but a Worker thread needs its own Engine, and a
 * handle must never be shared between workers.
 */

const bindingsModule = require('./lib/bindings');
const { loadBindings, OmniError, OmniLibraryError, STATUS, TAG, statusName, tagName } = bindingsModule;
const { libraryFileName, resolveLibrary } = require('./lib/discover');

/** Version of this SDK, kept equal to the workspace version of OmniEngine. */
const VERSION = '0.1.0';

/** Default size for the buffer that error text is copied into. */
const ERROR_BUFFER_SIZE = 4096;

/**
 * @typedef {object} Result
 * @property {string} tag     "number" | "bool" | "string" | "nil" | "undef"
 * @property {number} number  the raw double; meaningful for number and bool
 * @property {Buffer} text    the exact bytes of a string result, else empty
 * @property {*} value        the same thing as a JavaScript value
 */

function toResult(raw, text) {
  const tag = tagName(raw.tag);
  // The eight payload bytes of a string are an address. Reporting them as a
  // double would put meaningless noise in every log that prints a Result, so
  // the wrapper reads that arm only for the tags that mean a number.
  const number = tag === 'number' || tag === 'bool' ? raw.payload.number : 0;
  let value;
  if (tag === 'number') value = number;
  else if (tag === 'bool') value = number !== 0;
  else if (tag === 'string') value = text.toString('utf8');
  else value = null; // nil and undef carry no payload
  return { tag, number, text, value };
}

const EMPTY = Buffer.alloc(0);

class Engine {
  /**
   * Create an engine.
   *
   * @param {{ libraryPath?: string }} [options] point at an explicit artifact
   * @throws {OmniLibraryError} if the library is missing, is not this ABI level,
   *   or koffi is not installed
   */
  constructor(options = {}) {
    this._b = loadBindings(options.libraryPath);
    this._handle = this._b.engineNew();
    if (!this._handle) {
      throw new OmniLibraryError('omni_engine_new returned a null handle');
    }

    // Scratch buffers, allocated once per engine and reused. koffi.alloc returns
    // memory the caller owns, so close() must free it.
    this._valueOut = this._b.koffi.alloc(this._b.OmniValue, 1);
    this._numberOut = this._b.koffi.alloc('double', 1);
    this._statsOut = this._b.koffi.alloc(this._b.OmniStats, 1);
    this._errorBuf = Buffer.alloc(ERROR_BUFFER_SIZE);
    this._lastStatus = STATUS.OK;
  }

  /** Absolute path of the library this engine is talking to. */
  get libraryPath() {
    return this._b.path;
  }

  /** Version reported by the engine library, not by this SDK. */
  get version() {
    return this._require().version();
  }

  /** Wire format the loaded library reports. `loadBindings` refused otherwise. */
  get abiVersion() {
    return this._require().abiVersion();
  }

  /** Status code of the most recent call, for code that prefers data to exceptions. */
  get lastStatus() {
    return this._lastStatus;
  }

  /** Message from the most recent failure, or `''`. A success clears it. */
  lastError() {
    return this._error(this._require());
  }

  /**
   * Evaluate `source`, returning a Result.
   *
   * A string result arrives as `text` (the exact bytes, NULs included) and
   * `value` (the same bytes decoded as UTF-8).
   *
   * @param {string} source
   * @returns {Result}
   * @throws {OmniError} when the program or the call fails
   */
  run(source) {
    const b = this._require();
    const bytes = encode(source);
    const status = b.exec(this._handle, bytes, bytes.length, this._valueOut);
    this._lastStatus = status;
    if (status !== STATUS.OK) throw new OmniError(status, this._error(b, status));
    const raw = b.koffi.decode(this._valueOut, b.OmniValue);
    return toResult(raw, this._copyString(raw));
  }

  /**
   * Evaluate `source` requiring a number.
   *
   * @param {string} source
   * @returns {number}
   * @throws {OmniError}
   */
  evaluate(source) {
    const b = this._require();
    const bytes = encode(source);
    const status = b.execNumber(this._handle, bytes, bytes.length, this._numberOut);
    this._lastStatus = status;
    if (status !== STATUS.OK) throw new OmniError(status, this._error(b, status));
    return this._b.koffi.decode(this._numberOut, 'double');
  }

  /**
   * Evaluate `source` requiring a string.
   *
   * The mirror of `evaluate`, for the caller who would rather get an error than
   * an `unknown` to type-check.
   *
   * @param {string} source
   * @returns {string}
   * @throws {OmniError} when the program produced anything but a string
   */
  string(source) {
    const result = this.run(source);
    if (result.tag !== 'string') {
      throw new OmniError(STATUS.ERR_RUNTIME, `the program did not evaluate to a string (it produced a ${result.tag})`);
    }
    return result.value;
  }

  /**
   * Evaluate without throwing: `{ status, statusName, result, message }`.
   * For REPLs and editors, where a syntax error is a normal event.
   *
   * @param {string} source
   */
  tryRun(source) {
    const b = this._require();
    const bytes = encode(source);
    const status = b.exec(this._handle, bytes, bytes.length, this._valueOut);
    this._lastStatus = status;
    if (status !== STATUS.OK) {
      return { status, statusName: statusName(status), result: null, message: this._error(b, status) };
    }
    const raw = b.koffi.decode(this._valueOut, b.OmniValue);
    return { status, statusName: 'ok', result: toResult(raw, this._copyString(raw)), message: '' };
  }

  /**
   * Bind `name` to a string the host owns.
   *
   * The engine copies `value` into storage that lives as long as the binding,
   * so the program can read it on any later call -- unlike a result string,
   * which is only valid until the next evaluation. Redefining `name` replaces
   * the binding, and a Result kept from before then copies out as zero bytes
   * rather than as freed memory.
   *
   * `name` is at most `MAX_IDENTIFIER_LEN` UTF-8 bytes; `value` is at most
   * `MAX_STRING_LEN`. Both limits are read from the bindings module at call
   * time, so a test can lower them without allocating 16 MiB.
   *
   * @param {string} name
   * @param {string|Buffer} value
   * @throws {OmniError} when the name or payload is rejected
   */
  defineString(name, value) {
    const b = this._require();
    const nameBytes = encodeBytes(name, bindingsModule.MAX_IDENTIFIER_LEN, 'name');
    const payload = Buffer.isBuffer(value) ? value : Buffer.from(String(value), 'utf8');
    if (payload.length > bindingsModule.MAX_STRING_LEN) {
      throw new OmniError(
        STATUS.ERR_ARG,
        `payload is ${payload.length} bytes; the ABI accepts at most ${bindingsModule.MAX_STRING_LEN}`
      );
    }
    const status = b.defineString(this._handle, nameBytes, nameBytes.length, payload, payload.length);
    this._lastStatus = status;
    if (status !== STATUS.OK) {
      throw new OmniError(status, this._error(b, status) || `the ABI rejected the binding ${JSON.stringify(String(name))}`);
    }
  }

  /**
   * The engine's memory counters: executions, strings built, bytes held.
   *
   * Read them after a benchmark to tell a high-water mark from a leak. The
   * execution arena is bulk-freed per call and the egress block rewinds, so
   * `*BytesReserved` only ever grows to the largest single request.
   *
   * @returns {{execs: number, strings: number, arenaBytesInUse: number,
   *   arenaBytesReserved: number, arenaChunks: number, egressBytesInUse: number,
   *   egressBytesReserved: number, egressHighWater: number}}
   */
  stats() {
    const b = this._require();
    const status = b.debugStats(this._handle, this._statsOut);
    this._lastStatus = status;
    if (status !== STATUS.OK) throw new OmniError(status, this._error(b, status));
    const raw = b.koffi.decode(this._statsOut, b.OmniStats);
    // The callee writes its own struct size first. A mismatch means the header
    // this file was transcribed from and the library disagree about the field
    // order, so every number below is untrustworthy.
    if (raw.size !== b.koffi.sizeof(b.OmniStats)) {
      throw new OmniError(
        STATUS.ERR_ARG,
        `the library says OmniStats is ${raw.size} bytes; this SDK reads ${b.koffi.sizeof(b.OmniStats)}`
      );
    }
    // uint64 counters arrive as BigInt, which cannot be JSON.stringified or
    // compared with a plain number. No counter approaches 2^53 in a process that
    // has not allocated petabytes, so the conversion is lossless in practice.
    return {
      execs: Number(raw.execs),
      strings: Number(raw.strings),
      arenaBytesInUse: Number(raw.arena_bytes_in_use),
      arenaBytesReserved: Number(raw.arena_bytes_reserved),
      arenaChunks: Number(raw.arena_chunks),
      egressBytesInUse: Number(raw.egress_bytes_in_use),
      egressBytesReserved: Number(raw.egress_bytes_reserved),
      egressHighWater: Number(raw.egress_high_water),
    };
  }

  /**
   * Deliberately panic inside the engine and report the status.
   * Proves from a host that a Rust panic cannot unwind into the process.
   * Not part of the language; named after the ABI's test hook.
   *
   * @returns {number} OMNI_ERR_PANIC when the boundary is working
   */
  debugPanic() {
    return this._require().debugPanic(this._handle);
  }

  /** Release the engine and its buffers. Safe to call more than once. */
  close() {
    const b = this._b;
    if (this._handle) b.engineFree(this._handle);
    this._handle = null;
    for (const ptr of [this._valueOut, this._numberOut, this._statsOut]) {
      if (ptr) {
        b.koffi.free(ptr);
      }
    }
    this._valueOut = null;
    this._numberOut = null;
    this._statsOut = null;
  }

  /**
   * Copy a string result out of the engine, through the ABI.
   *
   * `omni_str_copy` takes the address of the value struct -- not the pointer
   * inside it -- so this file never converts an engine address into a
   * JavaScript value, and a wrapper cannot turn the documented staleness rule
   * into a segfault.
   */
  _copyString(raw) {
    if (raw.tag !== TAG.STRING) return EMPTY;
    const needed = Number(raw.len);
    if (needed === 0) return EMPTY;
    // The engine writes at most `needed` bytes, so an unsafe allocation is safe
    // here: every byte below the returned count was written by the copy. A short
    // read is possible only when `len` was stale, and then the tail of the
    // allocation was never touched -- so the tail goes with the allocation
    // instead of into a Buffer a caller might read through `.buffer`.
    const buf = Buffer.allocUnsafe(needed);
    const written = Number(this._b.strCopy(this._handle, this._valueOut, buf, needed));
    if (written === needed) return buf;
    return Buffer.from(buf.subarray(0, written));
  }

  _error(b, status) {
    const written = b.lastError(this._handle, this._errorBuf, this._errorBuf.length);
    const text = written > 0 ? this._errorBuf.subarray(0, written).toString('utf8') : '';
    if (text) return text;
    // execNumber can report a type failure the engine itself never produced,
    // so the wrapper supplies the message instead of returning silence.
    if (status === STATUS.ERR_RUNTIME) return 'the program did not evaluate to a number';
    return '';
  }

  _require() {
    if (this._handle === null) throw new OmniError(STATUS.ERR_ARG, 'this Engine is closed');
    return this._b;
  }

  toString() {
    return `[omni.Engine ${this._handle ? `v${this._b.version()}` : 'closed'} ${this._b.path}]`;
  }
}

// `using` support, for runtimes that have Explicit Resource Management.
if (Symbol.dispose) {
  Object.defineProperty(Engine.prototype, Symbol.dispose, {
    value: function dispose() {
      this.close();
    },
    writable: true,
    configurable: true,
  });
}

/** @param {string|Buffer} source */
function encode(source) {
  return encodeBytes(source, bindingsModule.MAX_SOURCE_LEN, 'source');
}

/**
 * Encode one host buffer and bound it the way the ABI does.
 *
 * @param {string|Buffer} value
 * @param {number} limit read from the bindings module by the caller, at call time
 * @param {string} label the word to use in the rejection
 */
function encodeBytes(value, limit, label) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(String(value), 'utf8');
  // Read the bound from the module rather than a captured constant, so a test
  // can lower it and exercise the guard without allocating 256 MiB.
  if (bytes.length > limit) {
    throw new OmniError(STATUS.ERR_ARG, `${label} is ${bytes.length} bytes; the ABI accepts at most ${limit}`);
  }
  return bytes;
}

module.exports = {
  Engine,
  OmniError,
  OmniLibraryError,
  STATUS,
  TAG,
  statusName,
  tagName,
  libraryFileName,
  resolveLibrary,
  encode,
  encodeBytes,
  get MAX_SOURCE_LEN() {
    return bindingsModule.MAX_SOURCE_LEN;
  },
  get MAX_STRING_LEN() {
    return bindingsModule.MAX_STRING_LEN;
  },
  get MAX_IDENTIFIER_LEN() {
    return bindingsModule.MAX_IDENTIFIER_LEN;
  },
  get ABI_VERSION() {
    return bindingsModule.ABI_VERSION;
  },
  VERSION,
};
