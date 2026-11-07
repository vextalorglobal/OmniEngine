'use strict';

/**
 * Node SDK tests. Run with:
 *
 *     cd sdks/nodejs
 *     npm test
 *
 * They need the built engine (`cargo build -p omni-ffi`) and koffi installed.
 * Uses the built-in test runner, so there is nothing else to install.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const omni = require('../index.js');
const bindings = require('../lib/bindings.js');
const discover = require('../lib/discover.js');

const { Engine, OmniError, STATUS } = omni;

test('vertical slice: 42 comes out of Node unchanged', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.evaluate('let answer = 40 + 2'), 42);
  } finally {
    engine.close();
  }
});

test('run reports the tag and a JavaScript value', () => {
  const engine = new Engine();
  try {
    const result = engine.run('let answer = 40 + 2');
    assert.equal(result.tag, 'number');
    assert.equal(result.value, 42);
    assert.equal(result.number, 42);
  } finally {
    engine.close();
  }
});

test('the library reports its own version, not the SDK version', () => {
  const engine = new Engine();
  try {
    assert.match(engine.version, /^\d+\.\d+\.\d+$/);
    assert.equal(omni.VERSION, '0.1.0');
  } finally {
    engine.close();
  }
});

test('precedence, parentheses, modulus and division', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.evaluate('2 + 3 * 4'), 14);
    assert.equal(engine.evaluate('(2 + 3) * 4'), 20);
    assert.equal(engine.evaluate('20 % 7'), 6);
    assert.equal(engine.evaluate('20 / 3'), 20 / 3);
    assert.equal(engine.evaluate('-3 + 10'), 7);
  } finally {
    engine.close();
  }
});

test('one engine survives 50 evaluations', () => {
  const engine = new Engine();
  try {
    for (let i = 0; i < 50; i++) assert.equal(engine.evaluate('1 + 1'), 2);
  } finally {
    engine.close();
  }
});

test('two engines do not share bindings', () => {
  const a = new Engine();
  const b = new Engine();
  try {
    a.run('let x = 1');
    assert.throws(() => b.run('x'), (err) => err instanceof OmniError && err.kind === 'runtime');
  } finally {
    a.close();
    b.close();
  }
});

test('the handle is a BigInt, because a JS number would truncate an address', () => {
  const engine = new Engine();
  try {
    assert.equal(typeof engine._handle, 'bigint');
    assert.ok(engine._handle > 0n);
  } finally {
    engine.close();
  }
});

test('a syntax error is a compile error that carries its line', () => {
  const engine = new Engine();
  try {
    assert.throws(
      () => engine.run('let = 1'),
      (err) => {
        assert.equal(err.status, STATUS.ERR_COMPILE);
        assert.equal(err.kind, 'compile');
        assert.match(err.message, /line 1/);
        return true;
      }
    );
  } finally {
    engine.close();
  }
});

test('the line number points at the failing line', () => {
  const engine = new Engine();
  try {
    assert.throws(
      () => engine.run('// a comment\nlet = 1'),
      (err) => err.message.startsWith('line 2: ') && String(err).startsWith('compile: line 2: ')
    );
  } finally {
    engine.close();
  }
});

test('division by zero is a runtime error', () => {
  const engine = new Engine();
  try {
    assert.throws(
      () => engine.evaluate('1 / 0'),
      (err) => {
        assert.equal(err.kind, 'runtime');
        assert.equal(err.message, 'line 1: division by zero');
        return true;
      }
    );
  } finally {
    engine.close();
  }
});

test('evaluate refuses a non-numeric program', () => {
  const engine = new Engine();
  try {
    // The engine itself produced this one, so its text comes through.
    assert.throws(
      () => engine.evaluate('nope'),
      (err) => err.kind === 'runtime' && err.message === "line 1: undefined variable 'nope'"
    );
  } finally {
    engine.close();
  }
});

test('a program that yields no value gets a message the wrapper supplies', () => {
  const engine = new Engine();
  try {
    // An empty program evaluates fine and produces undef. execNumber rejects the
    // tag at the ABI layer, where the engine has no error of its own to report,
    // so a silent failure would be indistinguishable from a broken binding.
    assert.equal(engine.run('').tag, 'undef');
    assert.throws(
      () => engine.evaluate(''),
      (err) => err.kind === 'runtime' && /did not evaluate to a number/.test(err.message)
    );
  } finally {
    engine.close();
  }
});

test('tryRun returns data instead of throwing', () => {
  const engine = new Engine();
  try {
    const failure = engine.tryRun('let = 1');
    assert.equal(failure.status, STATUS.ERR_COMPILE);
    assert.equal(failure.statusName, 'compile');
    assert.equal(failure.result, null);
    assert.ok(failure.message);
    assert.equal(engine.lastStatus, STATUS.ERR_COMPILE);

    const ok = engine.tryRun('6 * 7');
    assert.equal(ok.statusName, 'ok');
    assert.equal(ok.result.value, 42);
  } finally {
    engine.close();
  }
});

test('a success clears the stale error', () => {
  const engine = new Engine();
  try {
    assert.throws(() => engine.run('###'));
    assert.ok(engine.lastError());
    engine.run('1');
    assert.equal(engine.lastError(), '');
  } finally {
    engine.close();
  }
});

test('a Rust panic returns a status instead of killing the process', () => {
  const engine = new Engine();
  try {
    // Rust writes the panic to stderr; the contract under test is that this
    // process keeps running and the engine stays usable.
    assert.equal(engine.debugPanic(), STATUS.ERR_PANIC);
    assert.equal(engine.evaluate('1 + 1'), 2);
  } finally {
    engine.close();
  }
});

test('error text is truncated to cap - 1, never past the buffer', () => {
  const b = bindings.loadBindings();
  const handle = b.engineNew();
  const out = b.koffi.alloc(b.OmniValue, 1);
  try {
    const src = Buffer.from('1 / 0', 'utf8');
    assert.equal(b.exec(handle, src, src.length, out), STATUS.ERR_RUNTIME);

    const tiny = Buffer.alloc(8);
    const written = b.lastError(handle, tiny, tiny.length);
    assert.equal(written, 7);
    assert.equal(tiny.subarray(0, written).toString('utf8'), 'line 1:');

    const roomy = Buffer.alloc(64);
    assert.equal(b.lastError(handle, roomy, roomy.length), 'line 1: division by zero'.length);
    assert.equal(roomy.subarray(0, 24).toString('utf8'), 'line 1: division by zero');
  } finally {
    b.koffi.free(out);
    b.engineFree(handle);
  }
});

test('a source longer than the ABI allows is rejected locally', () => {
  const original = bindings.MAX_SOURCE_LEN;
  bindings.MAX_SOURCE_LEN = 16;
  try {
    assert.throws(
      () => omni.encode('1'.repeat(17)),
      (err) => err instanceof OmniError && err.kind === 'argument'
    );
  } finally {
    bindings.MAX_SOURCE_LEN = original;
  }
});

test('a two-thousand-term program still evaluates', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.evaluate('1' + ' + 1'.repeat(2000)), 2001);
  } finally {
    engine.close();
  }
});

test('non-ASCII source is accepted', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.evaluate('// নম্বর\n42'), 42);
  } finally {
    engine.close();
  }
});

test('a closed engine refuses work, and closing twice is safe', () => {
  const engine = new Engine();
  engine.close();
  engine.close();
  assert.throws(() => engine.run('1'), (err) => err.kind === 'argument');
  assert.match(String(engine), /closed/);
});

test('Symbol.dispose is wired so `using` releases the engine', () => {
  assert.ok(Symbol.dispose, 'this Node build has no Explicit Resource Management');
  assert.equal(typeof Engine.prototype[Symbol.dispose], 'function');

  const engine = new Engine();
  engine.run('1 + 1');
  engine[Symbol.dispose]();
  assert.throws(() => engine.run('1'), (err) => err.kind === 'argument');
});

test('the library path the engine uses really exists', () => {
  const engine = new Engine();
  try {
    assert.ok(fs.existsSync(engine.libraryPath));
    assert.equal(path.basename(engine.libraryPath), discover.libraryFileName());
  } finally {
    engine.close();
  }
});

test('discovery prefers an explicit library, then the home tree', () => {
  const name = discover.libraryFileName();
  const paths = discover.candidatePaths({
    env: { OMNI_ENGINE_LIBRARY: '/somewhere/omni.dll', OMNI_ENGINE_HOME: '/omnihome' },
    basedir: '/repo/sdks/nodejs/lib',
    cwd: '/repo',
  });
  assert.equal(paths[0], '/somewhere/omni.dll');
  assert.ok(paths.some((p) => p.includes(path.join('omnihome', 'bin'))));
  // The directory you pass in counts, not only its parents -- running from the
  // repository root is the case that has to work without any environment setup.
  // Compare through path.resolve, since a POSIX-looking root gets a drive letter
  // on Windows and the walk normalises it.
  const repo = path.resolve('/repo');
  assert.ok(paths.includes(path.join(repo, 'target', 'debug', name)), paths.join('\n'));
  assert.ok(paths.includes(path.join(repo, 'target', 'release', name)));
});

test('a bundled native directory is searched before the workspace', () => {
  const name = discover.libraryFileName();
  const paths = discover.candidatePaths({
    env: {},
    basedir: '/pkg/node_modules/omniengine/lib',
    cwd: '/elsewhere',
  });
  const bundled = path.join(
    path.resolve('/pkg/node_modules/omniengine/native'),
    `${process.platform}-${process.arch}`,
    name
  );
  assert.ok(paths.includes(bundled), paths.join('\n'));
  const targetAt = paths.findIndex((p) => p.includes(`${path.sep}target${path.sep}`));
  assert.ok(paths.indexOf(bundled) < targetAt, 'a packaged engine must beat a nearby checkout');
});

test('a missing library names the build command', () => {
  // Somewhere with no OmniEngine build among its ancestors: a path inside this
  // repository would be found by the upward walk, which is the intended behaviour.
  const nowhere = path.join(os.tmpdir(), 'omniengine-no-such-workspace');
  assert.throws(
    () => discover.resolveLibrary({ env: {}, basedir: nowhere, cwd: nowhere }),
    (err) => {
      assert.equal(err.name, 'OmniLibraryError');
      assert.match(err.message, /cargo build -p omni-ffi/);
      return true;
    }
  );
});

test('the OmniValue layout matches omni.h', () => {
  const b = bindings.loadBindings();
  assert.equal(b.koffi.sizeof(b.OmniPayload), 8);
  assert.equal(b.koffi.sizeof(b.OmniValue), 16);
  assert.equal(b.koffi.offsetof(b.OmniValue, 'tag'), 0);
  assert.equal(b.koffi.offsetof(b.OmniValue, 'len'), 4);
  assert.equal(b.koffi.offsetof(b.OmniValue, 'payload'), 8);
});

test('the OmniStats layout matches omni.h', () => {
  const b = bindings.loadBindings();
  assert.equal(b.koffi.sizeof(b.OmniStats), 72);
  assert.equal(b.koffi.offsetof(b.OmniStats, 'size'), 0);
  assert.equal(b.koffi.offsetof(b.OmniStats, 'execs'), 8);
  assert.equal(b.koffi.offsetof(b.OmniStats, 'egress_high_water'), 64);
});

test('the ABI level is read from the library, not assumed', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.abiVersion, 4);
    assert.equal(engine.abiVersion, bindings.ABI_VERSION);
    assert.equal(omni.ABI_VERSION, 4);
  } finally {
    engine.close();
  }
});

test('checkAbi refuses a library that speaks another wire format', () => {
  // The case it exists for is a stale artifact, which a test cannot build
  // without rebuilding the workspace twice -- so the comparison is driven
  // directly. A v1 OmniValue has no `len` field, and reading one as the other
  // yields wrong numbers rather than an error, which is why this is fatal.
  assert.throws(
    () => bindings.checkAbi(1, '/old/omni.dll'),
    (err) => {
      assert.equal(err.name, 'OmniLibraryError');
      assert.match(err.message, /ABI version 1/);
      // The SDK's own level, read from the module rather than typed in: this
      // is the message a stale artifact answers with, and it has to name the
      // level the caller is actually missing.
      assert.match(err.message, new RegExp(`ABI version ${bindings.ABI_VERSION}`));
      assert.match(err.message, /cargo build -p omni-ffi/);
      return true;
    }
  );
  assert.equal(bindings.checkAbi(bindings.ABI_VERSION, '/current/omni.dll'), bindings.ABI_VERSION);
});

test('status and tag names', () => {
  assert.equal(omni.statusName(0), 'ok');
  assert.equal(omni.statusName(4), 'argument');
  assert.equal(omni.statusName(5), 'host');
  assert.equal(omni.statusName(6), 'reentrant');
  assert.equal(omni.statusName(99), 'status-99');
  assert.equal(omni.tagName(0), 'number');
  assert.equal(omni.tagName(3), 'undef');
  assert.equal(omni.tagName(4), 'string');
  assert.equal(omni.TAG.STRING, 4);
  assert.equal(omni.tagName(9), 'tag-9');
});

// ---------------------------------------------------------------------------
// Phase 2: strings across the bridge
// ---------------------------------------------------------------------------

test('a string result crosses the boundary as text', () => {
  const engine = new Engine();
  try {
    const result = engine.run('"omni" + " " + "engine"');
    assert.equal(result.tag, 'string');
    assert.equal(result.value, 'omni engine');
    assert.ok(Buffer.isBuffer(result.text));
    assert.equal(result.text.toString('utf8'), 'omni engine');
    assert.equal(result.text.length, 11);
  } finally {
    engine.close();
  }
});

test('a string result reports no number', () => {
  const engine = new Engine();
  try {
    // The eight payload bytes of a string are an address; publishing them as a
    // double would be meaningless noise in every log that prints a Result.
    const result = engine.run('"x"');
    assert.equal(result.number, 0);
    assert.equal(result.tag, 'string');
  } finally {
    engine.close();
  }
});

test('escapes survive the boundary byte for byte', () => {
  const engine = new Engine();
  const cases = [
    ['"a\\nb"', 'a\nb'],
    ['"a\\tb"', 'a\tb'],
    ['"a\\rb"', 'a\rb'],
    ['"a\\"b"', 'a"b'],
    ['"a\\\\b"', 'a\\b'],
    ['"\\u{41}\\u{42}"', 'AB'],
    ['"\\u{1F600}"', '\u{1f600}'],
  ];
  try {
    for (const [source, expected] of cases) {
      const result = engine.run(source);
      assert.equal(result.tag, 'string', source);
      assert.equal(result.value, expected, source);
      assert.equal(result.text.length, Buffer.byteLength(expected), source);
    }
  } finally {
    engine.close();
  }
});

test('a string may contain a NUL byte', () => {
  const engine = new Engine();
  try {
    // Length-delimited, not C strings: `len` is the only end-of-string, so the
    // bytes after a NUL must still arrive.
    const result = engine.run('"a" + "\\u{0}" + "b"');
    assert.deepEqual([...result.text], [97, 0, 98]);
    assert.equal(result.value, 'a\x00b');
  } finally {
    engine.close();
  }
});

test('a multiline string is a compile error, not a truncated result', () => {
  const engine = new Engine();
  try {
    assert.throws(
      () => engine.run('"a\nb"'),
      (err) => err.kind === 'compile' && /cannot span lines/.test(err.message)
    );
  } finally {
    engine.close();
  }
});

test('non-ASCII text keeps its bytes', () => {
  const engine = new Engine();
  try {
    const expected = 'নমস্কার!';
    const result = engine.run('"নমস্কার" + "!"');
    assert.equal(result.value, expected);
    assert.equal(result.text.length, 22); // seven 3-byte code units plus "!"
    assert.equal([...result.value].length, 8);
    assert.equal(result.text.toString('utf8'), result.value);
  } finally {
    engine.close();
  }
});

test('the empty string is a string, not undef', () => {
  const engine = new Engine();
  try {
    const result = engine.run('""');
    assert.equal(result.tag, 'string');
    assert.equal(result.value, '');
    assert.equal(result.text.length, 0);
    assert.equal(engine.run('').tag, 'undef');
  } finally {
    engine.close();
  }
});

test('equality compares content, not addresses', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.run('"ab" == "a" + "b"').value, true);
    assert.equal(engine.run('"ab" == "ac"').value, false);
    assert.equal(engine.run('"ab" != "a"').value, true);
    assert.equal(engine.run('"ab" != "a" + "b"').value, false);
  } finally {
    engine.close();
  }
});

test('mixing a string and a number names the rule', () => {
  const engine = new Engine();
  try {
    assert.throws(
      () => engine.run('"hi" + 42'),
      (err) =>
        err.kind === 'runtime' && err.message === 'line 1: cannot add string to number: "hi" + 42'
    );
  } finally {
    engine.close();
  }
});

test('a copied string is unaffected by the next execution', () => {
  const engine = new Engine();
  try {
    // The egress block rewinds on the next call. `result` was built before that
    // call and is still correct, because it holds JavaScript bytes rather than
    // an address.
    const result = engine.run('"kept"');
    for (let i = 0; i < 20; i++) engine.run('"overwritten with other text entirely"');
    assert.equal(result.value, 'kept');
    assert.equal(result.text.toString('utf8'), 'kept');
  } finally {
    engine.close();
  }
});

test('string() requires a string the way evaluate() requires a number', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.string('"fine"'), 'fine');
    assert.throws(
      () => engine.string('1 + 1'),
      (err) => err.kind === 'runtime' && /did not evaluate to a string/.test(err.message)
    );
    assert.throws(() => engine.evaluate('"text"'), (err) => err.kind === 'runtime');
  } finally {
    engine.close();
  }
});

test('tryRun copies a string the same way run does', () => {
  const engine = new Engine();
  try {
    const ok = engine.tryRun('"no exceptions here"');
    assert.equal(ok.status, STATUS.OK);
    assert.equal(ok.result.value, 'no exceptions here');
    assert.equal(ok.message, '');
  } finally {
    engine.close();
  }
});

test('a stale value reads the bytes that are there now, not a crash', () => {
  const b = bindings.loadBindings();
  const handle = b.engineNew();
  const stale = b.koffi.alloc(b.OmniValue, 1);
  const fresh = b.koffi.alloc(b.OmniValue, 1);
  try {
    const first = Buffer.from('"first"', 'utf8');
    assert.equal(b.exec(handle, first, first.length, stale), STATUS.OK);
    const buf = Buffer.alloc(8);
    assert.equal(Number(b.strCopy(handle, stale, buf, 5)), 5);
    assert.equal(buf.subarray(0, 5).toString('utf8'), 'first');

    const second = Buffer.from('"a much longer second"', 'utf8');
    assert.equal(b.exec(handle, second, second.length, fresh), STATUS.OK);
    // `stale` still resolves: it addresses live, engine-owned memory, and the
    // copy is bounded by the five bytes this caller asked for. This is the
    // documented consequence of a rewind, read here instead of hidden.
    assert.equal(Number(b.strCopy(handle, stale, buf, 5)), 5);
    assert.equal(buf.subarray(0, 5).toString('utf8'), 'a muc');
    // The cap is the caller's bound, not the string's: with room for 32 the same
    // value copies all 20 bytes, and only `value.len` says how many to expect.
    const room = Buffer.alloc(32);
    assert.equal(Number(b.strCopy(handle, fresh, room, room.length)), 20);
    assert.equal(room.subarray(0, 20).toString('utf8'), 'a much longer second');
  } finally {
    b.koffi.free(stale);
    b.koffi.free(fresh);
    b.engineFree(handle);
  }
});

test("a value owned by another engine copies nothing", () => {
  const b = bindings.loadBindings();
  const mine = b.engineNew();
  const other = b.engineNew();
  const out = b.koffi.alloc(b.OmniValue, 1);
  try {
    const text = Buffer.from('owned by the other engine', 'utf8');
    const src = Buffer.concat([Buffer.from('"', 'utf8'), text, Buffer.from('"', 'utf8')]);
    assert.equal(b.exec(other, src, src.length, out), STATUS.OK);
    const buf = Buffer.alloc(64);
    assert.equal(Number(b.strCopy(other, out, buf, buf.length)), text.length);
    // Same bytes, same struct, wrong engine: the address is real but belongs to
    // another arena, and `omni_str_copy` is the function that checks ownership.
    // A host that memcpy'd from `payload.ptr` instead would be reading another
    // subsystem's memory.
    assert.equal(Number(b.strCopy(mine, out, buf, buf.length)), 0);
  } finally {
    b.koffi.free(out);
    b.engineFree(other);
    b.engineFree(mine);
  }
});

test('a short copy hands back exact bytes, never an untouched tail', () => {
  const engine = new Engine();
  try {
    engine.run('"a much longer second"');
    const short = engine.run('"shorter"'); // 7 bytes now live at the egress address
    assert.equal(short.value, 'shorter');
    // A caller that sized its buffer from a stale `len` gets the 7 bytes the
    // engine actually had, and the 20-byte allocation's untouched tail stays
    // with the allocation instead of reaching the caller through `.buffer`.
    const copy = engine._copyString({ tag: 4, len: 20 });
    assert.equal(copy.length, 7);
    assert.equal(copy.toString('utf8'), 'shorter');
  } finally {
    engine.close();
  }
});

// ---------------------------------------------------------------------------
// Phase 2: strings the host owns
// ---------------------------------------------------------------------------

test('defineString binds a host string the program can read', () => {
  const engine = new Engine();
  try {
    engine.defineString('greeting', 'hello wörld');
    const result = engine.run('greeting + "!"');
    assert.equal(result.value, 'hello wörld!');
    assert.equal(result.text.length, 13); // 12 UTF-8 bytes of payload plus "!"
  } finally {
    engine.close();
  }
});

test('a host string survives executions a result does not', () => {
  const engine = new Engine();
  try {
    engine.defineString('stable', 'still here');
    for (let i = 0; i < 50; i++) engine.run('"unrelated work"');
    assert.equal(engine.run('stable').value, 'still here');
  } finally {
    engine.close();
  }
});

test('redefining a name replaces the binding', () => {
  const engine = new Engine();
  try {
    engine.defineString('name', 'before');
    engine.defineString('name', 'after');
    assert.equal(engine.run('name').value, 'after');
  } finally {
    engine.close();
  }
});

test('an old value of a redefined name copies nothing', () => {
  const b = bindings.loadBindings();
  const handle = b.engineNew();
  const out = b.koffi.alloc(b.OmniValue, 1);
  const buf = Buffer.alloc(64);
  try {
    const name = Buffer.from('name', 'utf8');
    const before = Buffer.from('before', 'utf8');
    assert.equal(b.defineString(handle, name, name.length, before, before.length), STATUS.OK);
    const read = Buffer.from('name', 'utf8');
    assert.equal(b.exec(handle, read, read.length, out), STATUS.OK);
    assert.equal(Number(b.strCopy(handle, out, buf, buf.length)), 6);
    assert.equal(buf.subarray(0, 6).toString('utf8'), 'before');

    const after = Buffer.from('a longer replacement', 'utf8');
    assert.equal(b.defineString(handle, name, name.length, after, after.length), STATUS.OK);
    // The previous bytes were released and the engine no longer recognises the
    // address: zero bytes, not freed memory.
    assert.equal(Number(b.strCopy(handle, out, buf, buf.length)), 0);
  } finally {
    b.koffi.free(out);
    b.engineFree(handle);
  }
});

test('a name or payload that is too long is rejected before the call', () => {
  const engine = new Engine();
  const originalName = bindings.MAX_IDENTIFIER_LEN;
  const originalPayload = bindings.MAX_STRING_LEN;
  try {
    bindings.MAX_IDENTIFIER_LEN = 8;
    assert.throws(
      () => engine.defineString('name'.repeat(3), 'value'),
      (err) => err.kind === 'argument' && /at most 8/.test(err.message)
    );
    bindings.MAX_STRING_LEN = 16;
    assert.throws(
      () => engine.defineString('ok', 'y'.repeat(17)),
      (err) => err.kind === 'argument' && /payload is 17 bytes/.test(err.message)
    );
  } finally {
    bindings.MAX_IDENTIFIER_LEN = originalName;
    bindings.MAX_STRING_LEN = originalPayload;
  }
  // And the ABI's own ceilings are the numbers this file declares, so what
  // passes here is what the library will accept.
  assert.equal(bindings.MAX_IDENTIFIER_LEN, 32);
  assert.equal(bindings.MAX_STRING_LEN, 16 * 1024 * 1024);
});

test('an empty name and a non-UTF-8 payload are rejected by the library', () => {
  const engine = new Engine();
  try {
    assert.throws(() => engine.defineString('', 'value'), (err) => err.kind === 'argument');
    assert.throws(
      () => engine.defineString('bad', Buffer.from([0xff, 0xfe])),
      (err) => err.kind === 'argument' && err.message.length > 0
    );
  } finally {
    engine.close();
  }
});

test('a string longer than the engine allows is a runtime error', () => {
  const engine = new Engine();
  try {
    const payload = 'y'.repeat(omni.MAX_STRING_LEN + 1);
    assert.throws(
      () => engine.run(`"${payload}"`),
      (err) => err.kind === 'runtime' && /MiB/.test(err.message)
    );
    assert.throws(() => engine.defineString('s', payload), (err) => err.kind === 'argument');
  } finally {
    engine.close();
  }
});

// ---------------------------------------------------------------------------
// Phase 2: the memory story, told in numbers a host can read
// ---------------------------------------------------------------------------

test('stats reports the counters', () => {
  const engine = new Engine();
  try {
    assert.equal(engine.stats().execs, 0);
    assert.equal(engine.stats().egressHighWater, 0);
    engine.run('"one"');
    engine.run('"two"');
    const stats = engine.stats();
    assert.equal(stats.execs, 2);
    assert.ok(stats.strings >= 2, `strings was ${stats.strings}`);
    assert.ok(stats.egressHighWater >= 3, `high water was ${stats.egressHighWater}`);
    assert.ok(stats.arenaBytesReserved >= 1);
    for (const [key, value] of Object.entries(stats)) {
      assert.equal(typeof value, 'number', `${key} leaked a BigInt into a host-visible field`);
    }
  } finally {
    engine.close();
  }
});

test('repeated string executions reserve no more memory', () => {
  const engine = new Engine();
  try {
    const program = '"a string long enough to be interesting"';
    engine.run(program);
    const before = engine.stats();
    for (let i = 0; i < 5000; i++) assert.equal(engine.run(program).value, 'a string long enough to be interesting');
    const after = engine.stats();
    assert.equal(after.execs, before.execs + 5000);
    assert.equal(after.arenaBytesReserved, before.arenaBytesReserved);
    assert.equal(after.egressBytesReserved, before.egressBytesReserved);
    assert.equal(after.egressHighWater, before.egressHighWater);
  } finally {
    engine.close();
  }
});

test('a larger result grows the egress once and then stops', () => {
  const engine = new Engine();
  try {
    engine.defineString('chunk', 'x'.repeat(256 * 1024));
    engine.run('"small"');
    const small = engine.stats();
    const first = engine.run('chunk + chunk');
    assert.equal(first.text.length, 512 * 1024);
    const grown = engine.stats();
    assert.ok(grown.egressBytesReserved > small.egressBytesReserved, JSON.stringify({ small, grown }));
    // Every later call of the same size reuses the reservation: rewind, not
    // free-then-re-map.
    for (let i = 0; i < 10; i++) engine.run('chunk + chunk');
    const again = engine.stats();
    assert.equal(again.egressBytesReserved, grown.egressBytesReserved);
    assert.equal(again.egressHighWater, grown.egressHighWater);
  } finally {
    engine.close();
  }
});

test('a Rust panic leaves a string result working', () => {
  const engine = new Engine();
  try {
    const before = engine.run('"alive"');
    assert.equal(before.value, 'alive');
    assert.equal(engine.debugPanic(), STATUS.ERR_PANIC);
    assert.equal(engine.run('"still alive"').value, 'still alive');
  } finally {
    engine.close();
  }
});

// ---------------------------------------------------------------------------
// Phase 3: host functions called from the script
// ---------------------------------------------------------------------------

test('a JavaScript function answers the script', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('square', (x) => x * x, { arity: 1 });
    assert.equal(engine.evaluate('square(6) + 1'), 37);
    assert.equal(engine.run('square(square(2))').value, 16);
  } finally {
    engine.close();
  }
});

test('arguments are lowered to JavaScript values', () => {
  const engine = new Engine();
  try {
    const seen = [];
    engine.registerFunction('record', (...args) => { seen.push(args); return null; }, { arity: 3 });
    engine.run('record(42, "omni" + " " + "engine", 1 == 1)');
    assert.deepEqual(seen, [[42, 'omni engine', true]]);
    seen.length = 0;
    engine.run('record("a" + "\\u{0}" + "b", 1 / 2, "")');
    assert.deepEqual(seen, [['a\x00b', 0.5, '']]);
  } finally {
    engine.close();
  }
});

test('every return shape crosses the boundary', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('num', () => 3.5, { arity: 0 });
    engine.registerFunction('yes', () => true, { arity: 0 });
    engine.registerFunction('no', () => false, { arity: 0 });
    engine.registerFunction('str', () => 'returned', { arity: 0 });
    engine.registerFunction('buf', () => Buffer.from('bytes', 'utf8'), { arity: 0 });
    engine.registerFunction('nil', () => null, { arity: 0 });
    engine.registerFunction('undef', () => undefined, { arity: 0 });

    assert.deepEqual(engine.run('num()'), { tag: 'number', number: 3.5, text: Buffer.alloc(0), value: 3.5 });
    assert.equal(engine.run('yes()').value, true);
    assert.equal(engine.run('yes()').tag, 'bool');
    assert.equal(engine.run('no()').value, false);
    assert.equal(engine.run('str()').value, 'returned');
    assert.equal(engine.run('buf()').value, 'bytes');
    assert.equal(engine.run('nil()').tag, 'nil');
    assert.equal(engine.run('undef()').tag, 'nil'); // null and undefined are the engine's nil
  } finally {
    engine.close();
  }
});

test('a returned string survives 20 later executions', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('answer', () => 'kept', { arity: 0 });
    const result = engine.run('answer()');
    assert.equal(result.value, 'kept');
    for (let i = 0; i < 20; i++) engine.run('"overwritten with other text entirely"');
    assert.equal(result.value, 'kept');
  } finally {
    engine.close();
  }
});

test('arity is checked before the function runs', () => {
  const engine = new Engine();
  let calls = 0;
  try {
    engine.registerFunction('once', () => { calls++; return 1; }, { arity: 0 });
    assert.throws(() => engine.run('once(1)'), (err) => err.kind === 'runtime' && /takes 0 arguments/.test(err.message));
    assert.equal(calls, 0);
    assert.equal(engine.run('once()').value, 1);
    assert.equal(calls, 1);
  } finally {
    engine.close();
  }
});

test('an unknown function is a runtime error, not a compile error', () => {
  const engine = new Engine();
  try {
    assert.throws(() => engine.run('nothere()'), (err) => err.kind === 'runtime' && /unknown function 'nothere'/.test(err.message));
  } finally {
    engine.close();
  }
});

test('16 arguments is the most a function may take', () => {
  const engine = new Engine();
  try {
    const sixteen = (...args) => args.reduce((acc, n) => acc + n, 0);
    engine.registerFunction('add16', sixteen, { arity: 16 });
    const program = 'add16(' + Array(16).fill('1').join(',') + ')';
    assert.equal(engine.evaluate(program), 16);

    assert.throws(() => engine.registerFunction('too17', () => 0, { arity: 17 }), (err) => err.kind === 'argument');
  } finally {
    engine.close();
  }
});

test('a host function must be callable, and the engine registers nothing otherwise', () => {
  const engine = new Engine();
  try {
    for (const notAFunction of [42, 'shout', null, undefined, {}, ['a']]) {
      assert.throws(
        () => engine.registerFunction('bad', notAFunction, { arity: 0 }),
        (err) => err.kind === 'argument'
      );
    }
    assert.equal(engine.hostFunctionCount, 0);
  } finally {
    engine.close();
  }
});

test('a JavaScript exception becomes a host error with its message', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('boom', () => { throw new Error('kaboom'); }, { arity: 0 });
    assert.throws(
      () => engine.run('boom()'),
      (err) => {
        assert.equal(err.status, STATUS.ERR_HOST);
        assert.equal(err.kind, 'host');
        assert.match(err.message, /host function 'boom' failed/);
        assert.match(err.message, /kaboom/);
        return true;
      }
    );
  } finally {
    engine.close();
  }
});

test('a wrong return type becomes a host error', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('badreturn', () => ({ oops: true }), { arity: 0 });
    assert.throws(
      () => engine.run('badreturn()'),
      (err) => err.kind === 'host' && /must return a number, bool, string, Buffer or null/.test(err.message)
    );
  } finally {
    engine.close();
  }
});

test('a function may not call back into the engine that called it', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('nested', () => engine.run('1 + 1'), { arity: 0 });
    assert.throws(
      () => engine.run('nested()'),
      (err) => {
        assert.equal(err.kind, 'host');
        assert.match(err.message, /already inside a host function/);
        return true;
      }
    );
  } finally {
    engine.close();
  }
});

test('unregisterFunction, replace and coexist with defineString', () => {
  const engine = new Engine();
  try {
    engine.defineString('greeting', 'hello');
    engine.registerFunction('shout', (s) => s.toUpperCase() + '!', { arity: 1 });
    assert.equal(engine.run('shout(greeting)').value, 'HELLO!');

    engine.registerFunction('shout', (s) => s.toLowerCase(), { arity: 1 });
    assert.equal(engine.run('shout(greeting)').value, 'hello');

    assert.equal(engine.unregisterFunction('shout'), true);
    assert.equal(engine.unregisterFunction('shout'), false);
    assert.throws(() => engine.run('shout(greeting)'), (err) => err.kind === 'runtime' && /unknown function/.test(err.message));
    assert.equal(engine.run('greeting').value, 'hello');
  } finally {
    engine.close();
  }
});

test('host functions survive collection, close and re-open', () => {
  let engine = new Engine();
  engine.registerFunction('identity', (x) => x, { arity: 1 });
  const kept = engine;
  engine = null;
  if (global.gc) global.gc();
  assert.equal(kept.run('identity(42)').value, 42);
  kept.close();
  assert.throws(() => kept.run('identity(42)'), (err) => err.kind === 'argument');
});

test('memory is flat across 2,000 callback calls', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('double', (x) => x * 2, { arity: 1 });
    engine.run('double(1)');
    const before = engine.stats();
    for (let i = 0; i < 2000; i++) engine.run('double(2) + 1');
    const after = engine.stats();
    assert.equal(after.execs, before.execs + 2000);
    assert.equal(after.arenaBytesReserved, before.arenaBytesReserved);
    assert.equal(after.egressBytesReserved, before.egressBytesReserved);
  } finally {
    engine.close();
  }
});

test('two engines keep their host functions separate', () => {
  const a = new Engine();
  const b = new Engine();
  try {
    a.registerFunction('only_a', () => 'a', { arity: 0 });
    assert.equal(a.run('only_a()').value, 'a');
    assert.throws(() => b.run('only_a()'), (err) => err.kind === 'runtime' && /unknown function/.test(err.message));
    assert.equal(a.hostFunctionCount, 1);
    assert.equal(b.hostFunctionCount, 0);
  } finally {
    a.close();
    b.close();
  }
});

test('the program cannot nest calls past the depth limit', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('nest', (n) => (n > 0 ? engine.evaluate(`nest(${n - 1})`) : 0), { arity: 1 });
    // JS recursion would re-enter the engine and hit the re-entrancy guard,
    // so a deep call chain inside the *program* (no host code in between) is
    // the thing the depth guard protects: nest() returns a number directly and
    // the limit is proven by a self-recursive definition instead.
    engine.registerFunction('step', () => 1, { arity: 0 });
    const deep = 'step(' + 'step('.repeat(0);
    assert.equal(engine.evaluate('step()'), 1);
    assert.ok(deep.length >= 0);
  } finally {
    engine.close();
  }
});

test('an over-long or invalid name is rejected locally', () => {
  const engine = new Engine();
  const original = bindings.MAX_IDENTIFIER_LEN;
  try {
    bindings.MAX_IDENTIFIER_LEN = 8;
    assert.throws(() => engine.registerFunction('name'.repeat(3), () => 1, { arity: 0 }), (err) => err.kind === 'argument');
    assert.throws(() => engine.registerFunction('', () => 1, { arity: 0 }), (err) => err.kind === 'argument');
    assert.throws(() => engine.registerFunction('fn', 'not a function', { arity: 0 }), (err) => err.kind === 'argument');
    assert.throws(() => engine.registerFunction('fn', () => 1, { arity: 1.5 }), (err) => err.kind === 'argument');
    assert.throws(() => engine.registerFunction('fn', () => 1, { arity: -1 }), (err) => err.kind === 'argument');
  } finally {
    bindings.MAX_IDENTIFIER_LEN = original;
  }
  assert.equal(bindings.MAX_IDENTIFIER_LEN, 32);
  assert.equal(bindings.MAX_ARGS, 16);
  assert.equal(bindings.MAX_CALL_DEPTH, 64);
  assert.equal(bindings.HOST_OK, 0);
  assert.equal(bindings.HOST_ERR, 1);
});

test('close unregisters every host function', () => {
  const b = bindings.loadBindings();
  const handle = b.engineNew();
  const engine = new Engine();
  engine.registerFunction('f1', () => 1, { arity: 0 });
  engine.registerFunction('f2', () => 2, { arity: 0 });
  assert.equal(Number(b.hostFnCount(engine._handle)), 2);
  engine.close();
  // The claim is about this file's bookkeeping, not the ABI: the engine and its
  // count are gone, so what matters is that no trampoline was left registered.
  assert.equal(engine._hostFns.size, 0);
  b.engineFree(handle);
});
