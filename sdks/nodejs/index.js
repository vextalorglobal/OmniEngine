'use strict';

/**
 * OmniEngine from Node.js.
 *
 *     const { Engine } = require('omniengine');
 *
 *     const engine = new Engine();
 *     engine.evaluate('let answer = 40 + 2');   // 42
 *     engine.run('20 / 3').value;               // 6.666666666666667
 *     engine.close();
 *
 * Or let the handle clean itself up (Node 20.11+, 22, and later):
 *
 *     await using engine = new Engine();
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
 * @property {string} tag   "number" | "bool" | "nil" | "undef"
 * @property {number} number the raw double; meaningful for number and bool
 * @property {*} value       the same thing as a JavaScript value
 */

function toResult(raw) {
  const tag = tagName(raw.tag);
  let value;
  if (tag === 'number') value = raw.number;
  else if (tag === 'bool') value = raw.number !== 0;
  else value = null; // nil and undef carry no payload
  return { tag, number: raw.number, value };
}

class Engine {
  /**
   * Create an engine.
   *
   * @param {{ libraryPath?: string }} [options] point at an explicit artifact
   * @throws {OmniLibraryError} if the library is missing or koffi is not installed
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
    return toResult(this._b.koffi.decode(this._valueOut, this._b.OmniValue));
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
    return { status, statusName: 'ok', result: toResult(b.koffi.decode(this._valueOut, b.OmniValue)), message: '' };
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
    for (const ptr of [this._valueOut, this._numberOut]) {
      if (ptr) {
        b.koffi.free(ptr);
      }
    }
    this._valueOut = null;
    this._numberOut = null;
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
  const bytes = Buffer.isBuffer(source) ? source : Buffer.from(String(source), 'utf8');
  // Read the bound from the module rather than a captured constant, so a test
  // can lower it and exercise the guard without allocating 256 MiB.
  const limit = bindingsModule.MAX_SOURCE_LEN;
  if (bytes.length > limit) {
    throw new OmniError(STATUS.ERR_ARG, `source is ${bytes.length} bytes; the ABI accepts at most ${limit}`);
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
  get MAX_SOURCE_LEN() {
    return bindingsModule.MAX_SOURCE_LEN;
  },
  VERSION,
};
