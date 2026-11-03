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
  assert.equal(b.koffi.sizeof(b.OmniValue), 16);
  assert.equal(b.koffi.offsetof(b.OmniValue, 'number'), 8);
  assert.equal(b.koffi.offsetof(b.OmniValue, 'tag'), 0);
});

test('status and tag names', () => {
  assert.equal(omni.statusName(0), 'ok');
  assert.equal(omni.statusName(4), 'argument');
  assert.equal(omni.statusName(99), 'status-99');
  assert.equal(omni.tagName(0), 'number');
  assert.equal(omni.tagName(3), 'undef');
  assert.equal(omni.TAG.NUMBER, 0);
});
