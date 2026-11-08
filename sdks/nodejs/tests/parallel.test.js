'use strict';

/**
 * Parallel execution tests: one crossing, N worker engines.
 *
 * The parallel entry point must not change an answer, only where the item ran.
 * These tests therefore compare it against the sequential batch as much as they
 * exercise it, and cover the two things the SDK adds on top of the ABI: refusing
 * a bad `maxThreads` locally, and falling back when the loaded library predates
 * the export.
 */

const test = require('node:test');
const assert = require('node:assert/strict');

const omni = require('../index.js');
const bindings = require('../lib/bindings.js');

const { Engine, STATUS, TAG } = omni;

test('runParallel returns one result per source, in order', () => {
  const engine = new Engine();
  try {
    const results = engine.runParallel(
      ['6 * 7', '"hello, " + "parallel"', '1 == 1', ''],
      4
    );
    assert.deepEqual(
      results.map((r) => r.tag),
      ['number', 'string', 'bool', 'undef']
    );
    assert.equal(results[0].value, 42);
    assert.equal(results[1].value, 'hello, parallel');
    assert.equal(results[2].value, true);
    assert.equal(results[3].value, null);
  } finally {
    engine.close();
  }
});

test('a failing item raises naming its index and its neighbours still run', () => {
  const engine = new Engine();
  try {
    assert.throws(
      () => engine.runParallel(['1 + 1', '1 / 0', '2 + 2'], 4),
      (err) => {
        assert.equal(err.kind, 'runtime');
        assert.match(err.message, /item 1 of 3/);
        assert.match(err.message, /division by zero/);
        return true;
      }
    );
    // The item after the failure ran, so the batch counted three executions even
    // though the call reported item 1.
    assert.equal(engine.stats().execs, 3);
  } finally {
    engine.close();
  }
});

test('tryRunParallel keeps running after a failure', () => {
  const engine = new Engine();
  try {
    const items = engine.tryRunParallel(['1 +', '2 + 3', '1 / 0', '20 / 4'], 4);
    assert.deepEqual(
      items.map((item) => item.status),
      [STATUS.ERR_COMPILE, STATUS.OK, STATUS.ERR_RUNTIME, STATUS.OK]
    );
    assert.equal(items[0].result, null);
    assert.equal(items[1].result.value, 5);
    assert.equal(items[3].result.value, 5, 'the item after a failure ran');
    assert.match(items[0].message, /unexpected end of input/);
    assert.match(items[2].message, /division by zero/);
  } finally {
    engine.close();
  }
});

test('zero and one threads never change an answer', () => {
  const engine = new Engine();
  try {
    const sources = ['1 + 1', '1 / 0', '2 * 2', 'len("abcd")', '40 + 2'];
    const view = (items) =>
      items.map((item) => [item.status, item.result === null ? null : item.result.value]);
    assert.deepEqual(view(engine.tryRunParallel(sources, 0)), view(engine.tryRunParallel(sources, 1)));
    assert.equal(engine.tryRunParallel(sources, 0)[4].result.value, 42);
  } finally {
    engine.close();
  }
});

test('every string of a parallel batch is copied out', () => {
  const engine = new Engine();
  try {
    const results = engine.runParallel(['"a" + "1"', '"b" + "2"', '"c" + "3"', '"d" + "4"'], 4);
    assert.deepEqual(
      results.map((r) => r.text.toString()),
      ['a1', 'b2', 'c3', 'd4']
    );
    // Run something else, then read the Results again: each one owns its bytes,
    // so the shared egress the batch used is irrelevant now.
    engine.run('0');
    assert.deepEqual(
      results
        .slice()
        .reverse()
        .map((r) => r.text.toString()),
      ['d4', 'c3', 'b2', 'a1']
    );
  } finally {
    engine.close();
  }
});

test('a registered host function forces the sequential path', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('square', (x) => x * x, { arity: 1 });
    // Koffi marshals a JS callback to the main thread, which is blocked inside
    // this synchronous call, so the SDK must not dispatch it from a worker. The
    // answers are the same; only the concurrency is given up.
    const results = engine.runParallel(['square(6)', 'square(7) + 1', 'square(21)'], 4);
    assert.deepEqual(
      results.map((r) => r.value),
      [36, 50, 441]
    );
  } finally {
    engine.close();
  }
});

test('a bad maxThreads is refused locally', () => {
  const engine = new Engine();
  try {
    for (const bad of [true, -1, 1.5, '4', 2 ** 32]) {
      assert.throws(
        () => engine.runParallel(['1 + 1'], bad),
        (err) => {
          assert.equal(err.kind, 'argument');
          assert.match(err.message, /maxThreads must be a uint32/);
          return true;
        }
      );
    }
    assert.equal(engine.stats().execs, 0, 'nothing crossed');
  } finally {
    engine.close();
  }
});

test('a library without the parallel symbol falls back to execMany', () => {
  const engine = new Engine();
  const saved = engine._b.execParallel;
  engine._b.execParallel = null;
  try {
    const results = engine.runParallel(['1 + 1', '2 + 2'], 4);
    assert.deepEqual(
      results.map((r) => r.value),
      [2, 4]
    );
  } finally {
    engine._b.execParallel = saved;
    engine.close();
  }
});

test('an empty parallel batch costs no execution and expires nothing', () => {
  const engine = new Engine();
  try {
    const held = engine.run('"still current"');
    const before = engine.stats();
    assert.deepEqual(engine.tryRunParallel([], 4), []);
    assert.deepEqual(engine.runParallel([], 4), []);
    assert.equal(engine.stats().execs, before.execs, 'a no-op crossed nothing');
    assert.equal(held.text.toString(), 'still current');
  } finally {
    engine.close();
  }
});

test('the raw parallel export writes caller-owned arrays in order', () => {
  const b = bindings.loadBindings();
  const handle = b.engineNew();
  const sources = ['1 + 1', '"alpha"', '1 / 0', '20 / 4'];
  const keep = sources.map((s) => Buffer.from(s, 'utf8'));
  const addrs = new BigUint64Array(keep.length);
  const lens = new BigUint64Array(keep.length);
  for (let i = 0; i < keep.length; i++) {
    addrs[i] = BigInt(b.koffi.address(keep[i]));
    lens[i] = BigInt(keep[i].length);
  }
  const out = b.koffi.alloc(b.OmniValue, sources.length);
  const statuses = new Uint32Array(sources.length);
  try {
    const status = b.execParallel(handle, addrs, lens, sources.length, 4, out, statuses);
    assert.equal(status, STATUS.ERR_RUNTIME, 'the first failure, reported once');
    assert.deepEqual(Array.from(statuses), [0, 0, STATUS.ERR_RUNTIME, 0]);
    const values = b.koffi.decode(out, b.koffi.array(b.OmniValue, sources.length));
    assert.equal(values[0].payload.number, 2);
    assert.equal(values[2].tag, TAG.UNDEF, 'a failed item is a value, not a hole');
    assert.equal(values[3].payload.number, 5);
  } finally {
    b.koffi.free(out);
    b.engineFree(handle);
  }
});
