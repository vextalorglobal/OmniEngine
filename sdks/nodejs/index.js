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
 * And a JavaScript function becomes a name the script can call:
 *
 *     engine.registerFunction('square', (x) => x * x, { arity: 1 });
 *     engine.run('square(6) + 1').value;        // 37
 *     engine.registerFunction('shout', (s) => s.toUpperCase() + '!');
 *     engine.run('shout("hi")').value;          // 'HI!'
 *
 * Arguments arrive as numbers, booleans, strings, or null, and the return value
 * travels back the same way. An exception in your function is not a crash: it
 * becomes an `OmniError` whose kind is `'host'`, carrying your message to the
 * line of the call that caused it.
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
    // Registered callbacks and their koffi handles, keyed by name. The handle is
    // the memory koffi allocated for the C-to-JS trampoline; it stays live only
    // while this Map holds it, and `koffi.unregister` releases it. A callback
    // that is not registered has no handle, so the engine never calls dead code.
    this._hostFns = new Map();
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
   * Bind `name` to a JavaScript function the program can call.
   *
   * The script calls it by name -- `square(6)` -- with exactly `arity` arguments.
   * Arguments arrive as numbers, booleans, strings, or null, and whatever the
   * function returns travels back as the same kind of value. A string the
   * function returns is copied by the engine before the call returns, so a
   * Buffer the host built for the answer needs to live only until the callback
   * hands it over.
   *
   * Registering `name` again replaces the previous binding. When the engine
   * itself is calling your function, a second call into the engine on this same
   * handle from that function is refused with a host error rather than
   * re-entering a borrowed state.
   *
   * @param {string} name        at most `MAX_IDENTIFIER_LEN` UTF-8 bytes
   * @param {(...args) => any} fn the JavaScript function to call
   * @param {{ arity?: number }} [options] argument count; defaults to `fn.length`
   * @throws {OmniError} when the name, the arity, or the library rejects it
   */
  registerFunction(name, fn, options = {}) {
    const b = this._require();
    if (typeof fn !== 'function') {
      throw new OmniError(STATUS.ERR_ARG, `a host function must be callable, got ${typeof fn}`);
    }
    const arity = options.arity === undefined ? fn.length : options.arity;
    if (!Number.isInteger(arity) || arity < 0 || arity > bindingsModule.MAX_ARGS) {
      throw new OmniError(
        STATUS.ERR_ARG,
        `arity is ${arity}; a host function takes an integer between 0 and ${bindingsModule.MAX_ARGS}`
      );
    }
    const nameBytes = encodeBytes(name, bindingsModule.MAX_IDENTIFIER_LEN, 'name');

    const trampoline = (userData, args, argc, out) => this._callHost(name, fn, userData, args, argc, out);
    const handle = b.koffi.register(trampoline, b.koffi.pointer(b.OmniHostFn));
    const status = b.registerHostFn(this._handle, nameBytes, nameBytes.length, arity, handle, null);
    this._lastStatus = status;
    if (status !== STATUS.OK) {
      b.koffi.unregister(handle);
      throw new OmniError(status, this._error(b, status) || `the ABI rejected the host function ${JSON.stringify(String(name))}`);
    }

    // Replace any previous binding under this name, releasing its trampoline.
    const previous = this._hostFns.get(String(name));
    if (previous) b.koffi.unregister(previous.handle);
    this._hostFns.set(String(name), { arity, fn, handle, nameBytes });
  }

  /**
   * Remove the host function bound to `name`.
   *
   * Returns true when a binding was removed. The koffi trampoline is released
   * immediately, so a script that calls the name afterwards gets an ordinary
   * runtime error, not a call into a function that has been unregistered.
   *
   * @param {string} name
   * @returns {boolean}
   */
  unregisterFunction(name) {
    const b = this._b;
    const key = String(name);
    const entry = this._hostFns.get(key);
    if (!entry) return false;
    if (this._handle === null) {
      // The engine is closed, so the registry is gone with it; release the
      // trampoline and report that nothing was removed from a live engine.
      b.koffi.unregister(entry.handle);
      this._hostFns.delete(key);
      return false;
    }
    const removed = Number(b.unregisterHostFn(this._handle, entry.nameBytes, entry.nameBytes.length)) === 1;
    if (removed) {
      b.koffi.unregister(entry.handle);
      this._hostFns.delete(key);
    }
    return removed;
  }

  /** Registrations this engine currently holds.
   *
   * Read from the library, not from this file's own map, so the number is the
   * engine's answer and a registration this SDK thought it had but lost is a
   * difference you can see rather than one you have to discover at the next call.
   *
   * @returns {number}
   */
  get hostFunctionCount() {
    return Number(this._require().hostFnCount(this._handle));
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
    // Every registered trampoline belongs to this handle. Once the engine is
    // gone, a callback the runtime still holds would be a live JS closure with
    // dead C memory behind it, so unregister them all before letting go.
    for (const entry of this._hostFns.values()) b.koffi.unregister(entry.handle);
    this._hostFns.clear();
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
   * The trampoline koffi calls on the engine's behalf.
   *
   * The arguments arrive as raw memory owned by the engine and are only good
   * for the duration of this call: `args` is a borrowed array of value structs,
   * and a string argument's `payload.ptr` points at the text itself, not at the
   * object header a *result* pointer points at. So this reads them with a direct
   * copy rather than through `omni_str_copy`, which would re-enter the engine
   * that is already inside this callback.
   *
   * Any JavaScript exception becomes a host error carried back as data. A throw
   * that crossed into Rust would unwind through foreign frames -- undefined
   * behaviour the ABI exists to prevent.
   */
  _callHost(name, fn, _userData, args, argc, out) {
    const b = this._b;
    const values = [];
    try {
      // koffi.array rejects a length of zero, so a call with no arguments reads
      // the empty list without asking the pointer what lives at it.
      const decoded = argc === 0 ? [] : b.koffi.decode(args, b.koffi.array(b.OmniValue, argc));
      for (let i = 0; i < argc; i++) values.push(liftArgument(b, decoded[i]));
      return writeValue(b, out, fn(...values));
    } catch (err) {
      return writeError(b, out, err);
    }
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
    // so the wrapper supplies the message instead of returning silence. The
    // re-entrant status is the other case: the engine refused before it could
    // write, and a host that only sees a bare 6 learns nothing about why.
    if (status === STATUS.ERR_RUNTIME) return 'the program did not evaluate to a number';
    if (status === STATUS.ERR_REENTRANT) {
      return 'this engine is already inside a host function; a callback may not call back into the engine that called it';
    }
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
 * Lower one borrowed argument struct into a JavaScript value.
 *
 * This is the only place in the SDK that reads `payload.ptr` as a memory
 * address rather than handing the struct to `omni_str_copy`, and it is only
 * correct here: a callback argument's pointer addresses the first byte of the
 * text, not the string object's header, and the bytes it points at cannot move
 * while the engine is inside the callback.
 *
 * @param {Bindings} b
 * @param {{ tag: number, len: number, payload: { number: number, ptr: bigint } }} raw
 */
function liftArgument(b, raw) {
  const tag = tagName(raw.tag);
  if (tag === 'number') return raw.payload.number;
  if (tag === 'bool') return raw.payload.number !== 0;
  if (tag === 'string') {
    const length = Number(raw.len);
    if (!raw.payload.ptr || length === 0) return '';
    // `uint8_t` rather than `char`: koffi decodes a char array as a C string and
    // stops at the first NUL, which would silently truncate a value whose bytes
    // are otherwise complete.
    const bytes = b.koffi.decode(raw.payload.ptr, b.koffi.array('uint8_t', length));
    return Buffer.from(bytes).toString('utf8');
  }
  return null; // nil and undef carry no payload
}

/**
 * Encode one JavaScript value as the result of a host call.
 *
 * The engine copies out of `out` before this call unwinds, so the Buffer that
 * carries a returned string only needs to live until the callback returns --
 * and returning OMNI_HOST_ERR is how a failure reports itself, with the message
 * in the same `out` slot.
 */
function writeValue(b, out, result) {
  if (result === null || result === undefined) {
    b.koffi.encode(out, b.OmniValue, { tag: TAG.NIL, len: 0, payload: { number: 0 } });
    return bindingsModule.HOST_OK;
  }
  if (typeof result === 'boolean') {
    b.koffi.encode(out, b.OmniValue, { tag: TAG.BOOL, len: 0, payload: { number: result ? 1 : 0 } });
    return bindingsModule.HOST_OK;
  }
  if (typeof result === 'number') {
    if (Number.isNaN(result) || !Number.isFinite(result)) {
      return writeFailure(b, out, 'a host function cannot return NaN or Infinity');
    }
    b.koffi.encode(out, b.OmniValue, { tag: TAG.NUMBER, len: 0, payload: { number: result } });
    return bindingsModule.HOST_OK;
  }
  if (Buffer.isBuffer(result) || typeof result === 'string') {
    const buf = Buffer.isBuffer(result) ? result : Buffer.from(String(result), 'utf8');
    if (buf.length > bindingsModule.MAX_STRING_LEN) {
      return writeFailure(b, out, `a host function returned ${buf.length} bytes; the ABI accepts at most ${bindingsModule.MAX_STRING_LEN}`);
    }
    b.koffi.encode(out, b.OmniValue, { tag: TAG.STRING, len: buf.length, payload: { ptr: buf } });
    return bindingsModule.HOST_OK;
  }
  // Any other type is a mistake in the host's own function, so the engine gets
  // a failure with the type named rather than a silent zero or a half value.
  return writeFailure(b, out, `a host function must return a number, bool, string, Buffer or null, not ${typeof result}`);
}

/** Write a failure message in the result slot, with its length recorded. */
function writeFailure(b, out, message) {
  const buf = Buffer.from(message, 'utf8');
  const capped = buf.length > 4096 ? buf.subarray(0, 4096) : buf;
  b.koffi.encode(out, b.OmniValue, { tag: TAG.STRING, len: capped.length, payload: { ptr: capped } });
  return bindingsModule.HOST_ERR;
}

/**
 * Carrying the message is what turns a JavaScript throw into engine data.
 *
 * koffi re-surfaces an exception that escapes a callback as a throw at the
 * native call site, which is exactly the unwinding across foreign frames this
 * ABI forbids. The message travels in the same slot a result string would, so
 * the engine reads it with the same rules.
 */
function writeError(b, out, err) {
  const message = err instanceof Error ? `${err.name}: ${err.message}` : String(err);
  return writeFailure(b, out, message);
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
  get MAX_ARGS() {
    return bindingsModule.MAX_ARGS;
  },
  get MAX_CALL_DEPTH() {
    return bindingsModule.MAX_CALL_DEPTH;
  },
  get HOST_OK() {
    return bindingsModule.HOST_OK;
  },
  get HOST_ERR() {
    return bindingsModule.HOST_ERR;
  },
  get ABI_VERSION() {
    return bindingsModule.ABI_VERSION;
  },
  VERSION,
};
