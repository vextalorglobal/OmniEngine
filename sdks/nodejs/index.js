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
 * Many programs at once, if they are independent, belong in one crossing:
 *
 *     const results = engine.runMany(['1 + 1', '"two"', 'square(3)']);
 *     results[2].value;                          // 9
 *
 * `runMany` pays the FFI round trip and the engine's per-execution reclamation
 * once for the whole list instead of once per program, and it keeps every string
 * result readable until the next call -- see `tryRunMany` for the version that
 * reports failures as data.
 *
 * Ask whether a newer engine is published -- a check, never a download:
 *
 *     const check = await require('omniengine').checkUpdates();
 *     if (check.available) console.log(check.latest, 'is out');
 *
 * The comparison runs inside the loaded library (`omni_check_update`), so the
 * version and ABI it uses are the ones the engine actually speaks. A failed
 * check is data (`status: 'error'`), not an exception, because an update notice
 * must never break the program that asked for it.
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
const fs = require('node:fs/promises');
const { fileURLToPath } = require('node:url');

/** Version of this SDK, kept equal to the workspace version of OmniEngine. */
const VERSION = '0.1.0';

/** Default size for the buffer that error text is copied into. */
const ERROR_BUFFER_SIZE = 4096;

/** Room for one opcode mnemonic; the longest is well under 32 bytes. */
const OPCODE_NAME_BUFFER_SIZE = 64;

/** Where a release check looks when the caller names nothing else. */
const DEFAULT_UPDATE_URL =
  'https://github.com/fahim5536/omniengine/releases/latest/download/omni-update.json';

/** Names another manifest source: a URL or a `file://` path. */
const UPDATE_URL_ENV = 'OMNI_UPDATE_URL';

/** Set to `1` to permit plain HTTP manifest URLs. */
const ALLOW_INSECURE_ENV = 'OMNI_ALLOW_INSECURE';

/** Set to `1` to make every Engine start a background check. */
const UPDATE_CHECK_ENV = 'OMNI_UPDATE_CHECK';

/** Operating system and architecture, as a release manifest names them. */
const PLATFORM_KEYS = Object.freeze({
  'win32-x64': 'win32-x64',
  'linux-x64': 'linux-x64',
  'linux-arm64': 'linux-arm64',
  'darwin-x64': 'darwin-x64',
  'darwin-arm64': 'darwin-arm64',
});

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
   * @param {{ libraryPath?: string, updateCheck?: boolean }} [options] point at
   *   an explicit artifact; `updateCheck: true` starts a release check in the
   *   background (also enabled by `OMNI_UPDATE_CHECK=1`)
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
    this._profileOut = this._b.koffi.alloc(this._b.OmniProfile, 1);
    this._opcodeNameBuf = Buffer.alloc(OPCODE_NAME_BUFFER_SIZE);
    this._errorBuf = Buffer.alloc(ERROR_BUFFER_SIZE);
    // Batch scratch, grown once to the largest batch this engine has been handed
    // and reused after that. koffi passes a TypedArray's own memory for a
    // `void *` parameter, so repeating a batch of the same size costs no
    // allocation and no copy -- the only work per call is filling the arrays.
    this._batchCapacity = 0;
    this._batchAddrs = null;
    this._batchLengths = null;
    this._batchStatuses = null;
    this._batchValues = null;
    this._batchBase = 0n;
    this._batchStride = BigInt(this._b.koffi.sizeof(this._b.OmniValue));
    this._lastStatus = STATUS.OK;
    // Registered callbacks and their koffi handles, keyed by name. The handle is
    // the memory koffi allocated for the C-to-JS trampoline; it stays live only
    // while this Map holds it, and `koffi.unregister` releases it. A callback
    // that is not registered has no handle, so the engine never calls dead code.
    this._hostFns = new Map();

    // A background check is opt-in and starts only after the engine exists, so
    // a host that never asks pays nothing. The promise resolves to an
    // UpdateCheck and never rejects: an unreachable server must not surface as
    // an unhandled rejection in the host's process.
    const wanted = options.updateCheck === undefined ? envWantsCheck() : Boolean(options.updateCheck);
    this.updateCheck = wanted ? checkUpdates({ libraryPath: options.libraryPath }) : null;
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
   * Evaluate `source` and return a native JavaScript value.
   *
   * The one-call form for a REPL or a template engine: a number comes back as
   * `number`, a string as `string`, a boolean as `boolean`, and nil, undefined
   * or an empty program as `null`. Use `run` when the tag itself matters.
   *
   * @param {string} source
   * @returns {number|string|boolean|null}
   * @throws {OmniError} when the program fails
   */
  eval(source) {
    return this.run(source).value;
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
   * Evaluate every source in one crossing and return one Result per source.
   *
   * Each source is an independent program: a `let` in one is not visible to the
   * next, exactly as two `run` calls would behave. Host globals and registered
   * functions survive every item, as they survive every call.
   *
   * Every string result is copied out before this returns, so nothing in the
   * list expires.
   *
   * @param {Iterable<string|Buffer>} sources at most `MAX_BATCH` of them
   * @returns {Result[]}
   * @throws {OmniError} naming the first item that failed. Items after it still
   *   ran -- use `tryRunMany` to see them all.
   */
  runMany(sources) {
    const items = this._batch(sources);
    for (let index = 0; index < items.length; index++) {
      if (items[index].status !== STATUS.OK) {
        throw new OmniError(items[index].status, `item ${index} of ${items.length}: ${items[index].message}`);
      }
    }
    return items.map((item) => item.result);
  }

  /**
   * Evaluate a batch without throwing: one `{ status, statusName, result,
   * message }` per source, in order.
   *
   * The per-item shape of `tryRun`. A failing item carries its own status and
   * the engine's message for *that* item, read through `omni_batch_error` --
   * `lastError()` only ever holds the final item's.
   *
   * An empty iterable returns an empty list and expires nothing: polling an
   * empty queue does not invalidate a Result you are still holding.
   *
   * @param {Iterable<string|Buffer>} sources
   * @throws {OmniError} when the call itself was refused -- too many sources, an
   *   over-long source, a closed engine. A program that fails is data here; a
   *   call that was built wrong is not.
   */
  tryRunMany(sources) {
    return this._batch(sources);
  }

  /**
   * Evaluate every source on a worker group and return one Result per source.
   *
   * The parallel shape of `runMany`: the same per-item isolation, ordering, and
   * result lifetime, with evaluation spread over up to `maxThreads` forked
   * engines. `0` asks the engine for its automatic choice and `1` runs the
   * sequential path.
   *
   * A library built before the parallel export existed falls back to
   * `omni_exec_many` internally, so this method still answers correctly; only
   * the concurrency is missing.
   *
   * Every string result is copied out before this returns, so nothing in the
   * list expires.
   *
   * If any host function is registered, this runs sequentially instead:
   * koffi marshals a JavaScript callback to the main thread, and the main
   * thread is blocked inside the synchronous call, so dispatching a callback
   * from a worker would deadlock. `runMany` is therefore the right shape when
   * a batch calls into JavaScript.
   *
   * @param {Iterable<string|Buffer>} sources at most `MAX_BATCH` of them
   * @param {number} [maxThreads] 0 for automatic, otherwise 1..64
   * @returns {Result[]}
   * @throws {OmniError} naming the first item that failed. Items after it still
   *   ran -- use `tryRunParallel` to see them all.
   */
  runParallel(sources, maxThreads = 0) {
    const items = this._batch(sources, maxThreads);
    for (let index = 0; index < items.length; index++) {
      if (items[index].status !== STATUS.OK) {
        throw new OmniError(items[index].status, `item ${index} of ${items.length}: ${items[index].message}`);
      }
    }
    return items.map((item) => item.result);
  }

  /**
   * Evaluate a batch on a worker group without throwing: one `{ status,
   * statusName, result, message }` per source, in order.
   *
   * The per-item shape of `runParallel`; the same rules as `tryRunMany`.
   *
   * Like `runParallel`, this runs sequentially while any host function is
   * registered, because koffi can only enter JavaScript from the main thread.
   *
   * @param {Iterable<string|Buffer>} sources
   * @param {number} [maxThreads] 0 for automatic, otherwise 1..64
   * @throws {OmniError} when the call itself was refused -- too many sources, an
   *   over-long source, a closed engine, or a bad maxThreads.
   */
  tryRunParallel(sources, maxThreads = 0) {
    return this._batch(sources, maxThreads);
  }

  /**
   * One crossing, then every string copied out before anything else runs.
   *
   * The copy has to happen here and has to happen for every item: the batch
   * shares one egress rewind, so all N results are live at once right now and
   * all N expire on the next execution of either kind. `maxThreads` of null
   * selects the sequential entry point; a number selects the parallel one.
   */
  _batch(sources, maxThreads = null) {
    const b = this._require();
    if (maxThreads !== null) {
      if (!Number.isInteger(maxThreads) || maxThreads < 0 || maxThreads > 0xffffffff) {
        throw new OmniError(STATUS.ERR_ARG, `maxThreads must be a uint32, not ${maxThreads}`);
      }
    }
    const list = sources === null || sources === undefined ? [] : Array.from(sources);
    if (list.length > bindingsModule.MAX_BATCH) {
      throw new OmniError(
        STATUS.ERR_ARG,
        `${list.length} sources; one crossing carries at most ${bindingsModule.MAX_BATCH}`
      );
    }
    if (list.length === 0) return [];

    // The Buffers hold the bytes the engine reads, so they have to outlive the
    // call. The arrays below only carry their addresses.
    const encoded = list.map((source) => encode(source));
    const count = encoded.length;
    if (count > this._batchCapacity) this._growBatch(count);
    for (let i = 0; i < count; i++) {
      this._batchAddrs[i] = BigInt(b.koffi.address(encoded[i]));
      this._batchLengths[i] = BigInt(encoded[i].length);
    }
    // Zero the statuses this call owns. The ABI guarantees nothing about the
    // ones it does not write: an argument error refuses the whole batch with
    // nothing written, and a reused buffer would otherwise report the previous
    // batch's failures as this one's.
    this._batchStatuses.fill(0, 0, count);

    // `_hostFns` non-empty forces the sequential path: koffi queues a callback
    // to the main thread, which is blocked right here inside this synchronous
    // call, so a worker-thread dispatch would deadlock instead of answering.
    const useParallel =
      maxThreads !== null && b.execParallel !== null && this._hostFns.size === 0;
    const callStatus = Number(
      useParallel
        ? b.execParallel(
            this._handle,
            this._batchAddrs,
            this._batchLengths,
            count,
            maxThreads,
            this._batchValues,
            this._batchStatuses
          )
        : b.execMany(this._handle, this._batchAddrs, this._batchLengths, count, this._batchValues, this._batchStatuses)
    );
    this._lastStatus = callStatus;

    if (callStatus !== STATUS.OK && !anyItemFailed(this._batchStatuses, count)) {
      // No item failed, so the call itself was refused -- a bad argument or a
      // re-entrant call. There is no per-item shape for that, and nothing was
      // written, so the results stay undecoded rather than stale.
      throw new OmniError(callStatus, this._error(b, callStatus));
    }

    // One egress rewind for the whole batch: every string is readable right
    // now, and every one of them expires on the next execution. Copying them
    // here is what turns borrowed memory into ordinary JavaScript.
    const values = b.koffi.decode(this._batchValues, b.koffi.array(b.OmniValue, count));
    return values.map((raw, index) => {
      const status = this._batchStatuses[index];
      const failed = status !== STATUS.OK;
      return {
        status,
        statusName: statusName(status),
        result: failed ? null : toResult(raw, this._copyStringAt(raw, index)),
        message: failed ? this._itemError(b, index) : '',
      };
    });
  }

  /**
   * Allocate the batch arrays for at least `count` sources.
   *
   * Sizes double from 8, so a host that grows its batches a little at a time
   * reallocates logarithmically rather than once per call. `count` never
   * exceeds `MAX_BATCH`, so neither can the capacity.
   */
  _growBatch(count) {
    const b = this._b;
    let capacity = 8;
    while (capacity < count) capacity *= 2;
    if (this._batchValues) b.koffi.free(this._batchValues);
    this._batchAddrs = new BigUint64Array(capacity);
    this._batchLengths = new BigUint64Array(capacity);
    this._batchStatuses = new Uint32Array(capacity);
    this._batchValues = b.koffi.alloc(b.OmniValue, capacity);
    this._batchBase = BigInt(b.koffi.address(this._batchValues));
    this._batchCapacity = capacity;
  }

  /**
   * Copy a batch result out by index.
   *
   * `omni_str_copy` wants the address of the value struct, and a batch's structs
   * are one array: this SDK allocated it, so its base plus `index` strides is
   * element `index`. The stride comes from koffi's own layout of the struct,
   * which `declare` has already checked against the ABI.
   */
  _copyStringAt(raw, index) {
    if (raw.tag !== TAG.STRING) return EMPTY;
    const needed = Number(raw.len);
    if (needed === 0) return EMPTY;
    const buf = Buffer.allocUnsafe(needed);
    const written = Number(
      this._b.strCopy(this._handle, this._batchBase + BigInt(index) * this._batchStride, buf, needed)
    );
    if (written === needed) return buf;
    return Buffer.from(buf.subarray(0, written));
  }

  /** The engine's own message for item `index` of the batch just run. */
  _itemError(b, index) {
    const written = Number(b.batchError(this._handle, index, this._errorBuf, this._errorBuf.length));
    return written > 0 ? this._errorBuf.subarray(0, written).toString('utf8') : '';
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
   * `lastExecMicros` is the wall-clock cost of the most recent execution, or 0
   * before the first one. It is a plain number, not a BigInt: it is a `uint32_t`
   * and saturates rather than wrapping.
   *
   * @returns {{execs: number, lastExecMicros: number, strings: number,
   *   arenaBytesInUse: number, arenaBytesReserved: number, arenaChunks: number,
   *   egressBytesInUse: number, egressBytesReserved: number,
   *   egressHighWater: number}}
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
      lastExecMicros: Number(raw.last_exec_micros),
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
   * Turn per-opcode nanosecond timing on or off for later evaluations.
   *
   * The instruction counts and the scalar counters are always recorded; this
   * switch only controls the per-opcode timing split, which costs a timer read
   * per opcode and starts off.
   *
   * @param {boolean} enabled
   * @throws {OmniLibraryError} if the loaded library predates the profiler
   */
  setProfiling(enabled) {
    const b = this._require();
    if (!b.setProfiling) {
      throw new OmniLibraryError(
        'the loaded OmniEngine library predates the profiler (omni_engine_set_profiling). Rebuild it with:\n' +
          '    cargo build -p omni-ffi'
      );
    }
    const status = b.setProfiling(this._handle, enabled ? 1 : 0);
    this._lastStatus = status;
    if (status !== STATUS.OK) throw new OmniError(status, this._error(b, status));
  }

  /**
   * What the most recent evaluation cost, at opcode granularity.
   *
   * `opcodeCalls` maps every opcode mnemonic this build knows to how often the
   * last program dispatched it (zero for the ones it did not use), `opcodeNs`
   * is the same table in nanoseconds -- all zeros unless profiling was enabled
   * first -- and `peakBytes` is the arena high-water mark of that execution.
   *
   * The picture always describes the *last* evaluation: a single `run`, one
   * item of a `runMany` batch, or a failure.
   *
   * @returns {{totalInstructions: number, elapsedNs: number, peakBytes: number,
   *   opcodeCalls: Record<string, number>, opcodeNs: Record<string, number>}}
   * @throws {OmniLibraryError} if the loaded library predates the profiler
   */
  profile() {
    const b = this._require();
    if (!b.profile || !b.opcodeName) {
      throw new OmniLibraryError(
        'the loaded OmniEngine library predates the profiler (omni_profile). Rebuild it with:\n' +
          '    cargo build -p omni-ffi'
      );
    }
    const status = b.profile(this._handle, this._profileOut);
    this._lastStatus = status;
    if (status !== STATUS.OK) throw new OmniError(status, this._error(b, status));
    const raw = b.koffi.decode(this._profileOut, b.OmniProfile);
    if (raw.size !== b.koffi.sizeof(b.OmniProfile)) {
      throw new OmniError(
        STATUS.ERR_ARG,
        `the library says OmniProfile is ${raw.size} bytes; this SDK reads ${b.koffi.sizeof(b.OmniProfile)}`
      );
    }
    const count = Math.min(Number(raw.opcode_count), b.MAX_OPCODES);
    const opcodeCalls = {};
    const opcodeNs = {};
    for (let i = 0; i < count; i++) {
      const written = b.opcodeName(i, this._opcodeNameBuf, this._opcodeNameBuf.length);
      const name = written
        ? this._opcodeNameBuf.toString('utf8', 0, written)
        : `opcode-${i}`;
      opcodeCalls[name] = Number(raw.opcode_calls[i]);
      opcodeNs[name] = Number(raw.opcode_ns[i]);
    }
    return {
      totalInstructions: Number(raw.total_instructions),
      elapsedNs: Number(raw.elapsed_ns),
      peakBytes: Number(raw.peak_bytes),
      opcodeCalls,
      opcodeNs,
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
    for (const ptr of [this._valueOut, this._numberOut, this._statsOut, this._profileOut, this._batchValues]) {
      if (ptr) {
        b.koffi.free(ptr);
      }
    }
    this._valueOut = null;
    this._numberOut = null;
    this._statsOut = null;
    this._profileOut = null;
    this._opcodeNameBuf = null;
    this._batchAddrs = null;
    this._batchLengths = null;
    this._batchStatuses = null;
    this._batchValues = null;
    this._batchBase = 0n;
    this._batchCapacity = 0;
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

/**
 * Did any of the first `count` batch items fail?
 *
 * Reads only the range this call owns, because the array behind it is reused
 * and may still hold an earlier batch's statuses past `count`.
 */
function anyItemFailed(statuses, count) {
  for (let index = 0; index < count; index++) {
    if (statuses[index] !== STATUS.OK) return true;
  }
  return false;
}

/** This host's release key, or `null` when no build is published for it. */
function platformKey() {
  return PLATFORM_KEYS[`${process.platform}-${process.arch}`] || null;
}

function allowInsecure() {
  return ['1', 'true', 'yes'].includes(String(process.env[ALLOW_INSECURE_ENV] || '').toLowerCase());
}

/**
 * Read a manifest over HTTPS, or from `file://` for a local mirror.
 *
 * Plain HTTP is refused unless `OMNI_ALLOW_INSECURE` is set: a check that can be
 * redirected to an unauthenticated channel lets the network choose which release
 * you are offered.
 *
 * @param {string} url
 * @param {number} timeoutMs
 * @returns {Promise<Buffer>}
 */
async function defaultFetch(url, timeoutMs) {
  if (url.startsWith('file://')) {
    const path = fileURLToPath(url);
    const data = await fs.readFile(path);
    return data.length > bindingsModule.MAX_MANIFEST_LEN + 1
      ? data.subarray(0, bindingsModule.MAX_MANIFEST_LEN + 1)
      : data;
  }
  if (!url.startsWith('https://') && !url.startsWith('http://')) {
    throw new Error(`unsupported update URL ${JSON.stringify(url)}; expected https:// or file://`);
  }
  if (url.startsWith('http://') && !allowInsecure()) {
    throw new Error(
      `refusing plain HTTP for ${JSON.stringify(url)}; use https, or set ${ALLOW_INSECURE_ENV}=1 to allow it`
    );
  }
  if (typeof fetch !== 'function') {
    throw new Error('this Node has no global fetch; use Node 18 or later, or pass a fetch function');
  }

  // A cap, not a guess: the body is read at most MAX_MANIFEST_LEN + 1 bytes so a
  // server that keeps sending cannot exhaust host memory before the engine
  // refuses the document for being too large.
  const cap = bindingsModule.MAX_MANIFEST_LEN + 1;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(url, {
      signal: controller.signal,
      headers: { 'user-agent': `omni-node/${VERSION}` },
    });
    if (!response.ok) throw new Error(`the update server answered HTTP ${response.status}`);
    const chunks = [];
    let total = 0;
    if (response.body && typeof response.body.getReader === 'function') {
      const reader = response.body.getReader();
      try {
        while (total < cap) {
          const { done, value } = await reader.read();
          if (done) break;
          const take = value.subarray(0, Math.min(value.length, cap - total));
          chunks.push(Buffer.from(take));
          total += take.length;
          if (take.length < value.length) break;
        }
      } finally {
        await reader.cancel().catch(() => {});
      }
    } else {
      chunks.push(Buffer.from(await response.arrayBuffer()).subarray(0, cap));
      total = chunks[0].length;
    }
    return Buffer.concat(chunks, total);
  } finally {
    clearTimeout(timer);
  }
}

/** Ask the loaded library to compare this build with one manifest. */
function compareWithEngine(b, document) {
  if (typeof b.checkUpdate !== 'function') {
    throw new OmniLibraryError(
      `${b.path} does not export omni_check_update, so it predates the update check. Rebuild it:\n` +
        '    cargo build -p omni-ffi'
    );
  }
  const bytes = Buffer.isBuffer(document) ? document : Buffer.from(String(document), 'utf8');
  const out = Buffer.alloc(64);
  const status = Number(b.checkUpdate(bytes, bytes.length, out, out.length));
  const end = out.indexOf(0);
  const latest = (end === -1 ? out : out.subarray(0, end)).toString('utf8');
  return { status, latest };
}

/**
 * What one release check found.
 *
 * `status` is `'up-to-date'`, `'available'`, `'abi-mismatch'`, `'invalid'` (the
 * manifest is not one this engine reads), `'bad-argument'` (too large), or
 * `'error'` (the check itself failed -- no network, a library too old to
 * compare, a URL refused). `asset` is present only when this platform has a
 * download listed.
 */
class UpdateCheck {
  constructor(fields) {
    this.status = fields.status;
    this.current = fields.current;
    this.latest = fields.latest || '';
    this.abi = fields.abi === undefined ? null : fields.abi;
    this.notes = fields.notes === undefined ? null : fields.notes;
    this.asset = fields.asset === undefined ? null : fields.asset;
    this.url = fields.url === undefined ? null : fields.url;
    this.error = fields.error === undefined ? null : fields.error;
    Object.freeze(this);
  }

  /** A newer build is published for this ABI and platform. */
  get available() {
    return this.status === 'available';
  }

  /** The check ran and the manifest was readable. */
  get ok() {
    return ['up-to-date', 'available', 'abi-mismatch'].includes(this.status);
  }

  toString() {
    if (this.status === 'error') return `update check failed: ${this.error}`;
    if (this.status === 'available') return `omni ${this.latest} is available; this is ${this.current}`;
    if (this.status === 'abi-mismatch') {
      return `release ${this.latest} speaks ABI ${this.abi}; this build speaks ABI ${bindingsModule.ABI_VERSION}`;
    }
    if (this.status === 'up-to-date') return `omni ${this.current} is up to date`;
    return `update status: ${this.status}`;
  }
}

/**
 * Ask whether a newer OmniEngine is published. Never downloads anything.
 *
 * The comparison runs in the engine itself, so the version and ABI it checks
 * are the ones the loaded library actually speaks.
 *
 * @param {object} [options]
 * @param {string|Buffer} [options.manifest] the document itself; no network call
 * @param {string} [options.url] a manifest location; falls back to
 *   `$OMNI_UPDATE_URL`, then the GitHub release
 * @param {number} [options.timeoutMs] how long the request may take
 * @param {(url: string, timeoutMs: number) => any} [options.fetch] a replacement reader
 * @param {boolean} [options.raiseOnError] reject instead of returning `'error'`
 * @param {string} [options.libraryPath] point at an explicit engine artifact
 * @returns {Promise<UpdateCheck>}
 */
async function checkUpdates(options = {}) {
  let source = options.url || process.env[UPDATE_URL_ENV] || DEFAULT_UPDATE_URL;
  try {
    let data;
    if (options.manifest === undefined || options.manifest === null) {
      const reader = options.fetch || defaultFetch;
      data = await reader(source, options.timeoutMs === undefined ? 5000 : options.timeoutMs);
    } else {
      data = options.manifest;
      source = null;
    }
    const text = Buffer.isBuffer(data) ? data.toString('utf8') : String(data);
    const b = loadBindings(options.libraryPath);
    const verdict = compareWithEngine(b, text);

    let notes = null;
    let abi = null;
    let asset = null;
    let document = null;
    try {
      document = JSON.parse(text);
    } catch {
      document = null;
    }
    if (document && typeof document === 'object' && !Array.isArray(document)) {
      if (typeof document.notes === 'string') notes = document.notes;
      if (Number.isInteger(document.abi)) abi = document.abi;
      const key = platformKey();
      const assets = document.assets;
      if (
        key &&
        assets &&
        typeof assets === 'object' &&
        assets[key] &&
        typeof assets[key].url === 'string' &&
        typeof assets[key].sha256 === 'string'
      ) {
        asset = { url: assets[key].url, sha256: assets[key].sha256 };
      }
    }

    return new UpdateCheck({
      status: bindingsModule.UPDATE_STATUS_NAMES[verdict.status] || `status-${verdict.status}`,
      current: VERSION,
      latest: verdict.latest,
      abi,
      notes,
      asset,
      url: source,
    });
  } catch (err) {
    if (options.raiseOnError) throw err;
    return new UpdateCheck({
      status: 'error',
      current: VERSION,
      url: source,
      error: `${err.name || 'Error'}: ${err.message || err}`,
    });
  }
}

/** Whether an Engine should start a check without being asked to. */
function envWantsCheck() {
  return ['1', 'true', 'yes'].includes(String(process.env[UPDATE_CHECK_ENV] || '').toLowerCase());
}

module.exports = {
  Engine,
  UpdateCheck,
  checkUpdates,
  platformKey,
  DEFAULT_UPDATE_URL,
  UPDATE_URL_ENV,
  ALLOW_INSECURE_ENV,
  UPDATE_CHECK_ENV,
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
  get MAX_BATCH() {
    return bindingsModule.MAX_BATCH;
  },
  get MAX_OPCODES() {
    return bindingsModule.MAX_OPCODES;
  },
  get MAX_MANIFEST_LEN() {
    return bindingsModule.MAX_MANIFEST_LEN;
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
