'use strict';

/**
 * Batch execution tests: one crossing, N programs.
 *
 * What a loop of `run` cannot give is N string results alive at the same time,
 * and what it costs is N crossings. Both halves are tested here, plus the
 * boundary rules: a batch is independent executions rather than one long
 * program, a failing item is data rather than a stopped loop, and the memory
 * stays flat over thousands of calls.
 */

const test = require('node:test');
const assert = require('node:assert/strict');

const omni = require('../index.js');
const bindings = require('../lib/bindings.js');

const { Engine, STATUS, TAG } = omni;

test('runMany returns one result per source, in order', () => {
  const engine = new Engine();
  try {
    const results = engine.runMany(['6 * 7', '"hello, " + "batch"', '1 == 1', '']);
    assert.deepEqual(
      results.map((r) => r.tag),
      ['number', 'string', 'bool', 'undef']
    );
    assert.equal(results[0].value, 42);
    assert.equal(results[1].value, 'hello, batch');
    assert.equal(results[2].value, true);
    assert.equal(results[3].value, null);
  } finally {
    engine.close();
  }
});

test('every string of a batch is copied out', () => {
  const engine = new Engine();
  try {
    // Three live results at once is the thing a loop of run() cannot give. Read
    // them in order and then re-derive each one, so a batch that let the engine
    // rewind between items would show up as text that changed under the read.
    const programs = ['"a" + "1"', '"b" + "2"', '"c" + "3"'];
    const results = engine.runMany(programs);
    assert.deepEqual(
      results.map((r) => r.text.toString()),
      ['a1', 'b2', 'c3']
    );
    for (const [source, expected] of programs.map((s, i) => [s, results[i]]).reverse()) {
      assert.equal(engine.run(source).text.toString(), expected.text.toString());
    }
    // And the batch's own Results are JavaScript now, not borrowed memory.
    assert.deepEqual(
      results.map((r) => r.text.toString()),
      ['a1', 'b2', 'c3']
    );
  } finally {
    engine.close();
  }
});

test('a failing item raises naming its index', () => {
  const engine = new Engine();
  try {
    assert.throws(
      () => engine.runMany(['1 + 1', '1 / 0', '2 + 2']),
      (err) => {
        assert.equal(err.kind, 'runtime');
        assert.match(err.message, /item 1 of 3/);
        assert.match(err.message, /division by zero/);
        return true;
      }
    );
    assert.equal(engine.lastStatus, STATUS.ERR_RUNTIME);
  } finally {
    engine.close();
  }
});

test('tryRunMany keeps running after a failure', () => {
  const engine = new Engine();
  try {
    const items = engine.tryRunMany(['1 +', '2 + 3', '1 / 0', '20 / 4']);
    assert.deepEqual(items.map((i) => i.status), [1, 0, 2, 0]);
    assert.deepEqual(items.map((i) => i.statusName), ['compile', 'ok', 'runtime', 'ok']);
    assert.equal(items[0].result, null);
    assert.equal(items[1].result.value, 5);
    assert.equal(items[3].result.value, 5, 'the item after a failure ran');
    assert.match(items[0].message, /unexpected end of input/);
    assert.match(items[2].message, /division by zero/);
    assert.equal(items[1].message, '');
  } finally {
    engine.close();
  }
});

test("an item cannot read the previous item's binding", () => {
  const engine = new Engine();
  try {
    const items = engine.tryRunMany(['let secret = 1; secret', 'secret']);
    assert.equal(items[0].result.value, 1);
    assert.equal(items[1].status, STATUS.ERR_RUNTIME);
    // The message is item 1's own, which is why omni_batch_error exists: after a
    // batch, lastError() only ever holds the final item's text.
    assert.match(items[1].message, /secret/);
  } finally {
    engine.close();
  }
});

test('a host global survives every item of a batch', () => {
  const engine = new Engine();
  try {
    engine.defineString('greeting', 'hello');
    const results = engine.runMany(['greeting + " 1"', 'greeting + " 2"', 'greeting']);
    assert.deepEqual(
      results.map((r) => r.value),
      ['hello 1', 'hello 2', 'hello']
    );
  } finally {
    engine.close();
  }
});

test('a host function is callable from a batch item', () => {
  const engine = new Engine();
  try {
    engine.registerFunction('square', (x) => x * x, { arity: 1 });
    engine.registerFunction('tag', (x) => `squared: ${Math.round(x * x)}`, { arity: 1 });
    const results = engine.runMany(['square(6)', 'square(7) + 1', 'tag(3) + "!"']);
    assert.equal(results[0].value, 36);
    assert.equal(results[1].value, 50);
    assert.equal(results[2].value, 'squared: 9!');
  } finally {
    engine.close();
  }
});

test('an empty batch costs no execution and expires nothing', () => {
  const engine = new Engine();
  try {
    const held = engine.run('"still current"');
    const before = engine.stats();
    assert.deepEqual(engine.tryRunMany([]), []);
    assert.deepEqual(engine.runMany([]), []);
    assert.deepEqual(engine.runMany(new Set()), []);
    const after = engine.stats();
    assert.equal(after.execs, before.execs, 'a no-op crossed nothing');
    assert.equal(held.value, 'still current');
  } finally {
    engine.close();
  }
});

test('execs counts items, not crossings', () => {
  const engine = new Engine();
  try {
    const before = engine.stats().execs;
    engine.runMany(['1', '2', '3', '4', '5']);
    assert.equal(engine.stats().execs, before + 5);
  } finally {
    engine.close();
  }
});

test('a batch larger than the ABI allows is refused locally', () => {
  const engine = new Engine();
  const original = bindings.MAX_BATCH;
  try {
    bindings.MAX_BATCH = 8;
    assert.throws(
      () => engine.runMany(Array.from({ length: 9 }, () => '1')),
      (err) => {
        assert.equal(err.kind, 'argument');
        assert.match(err.message, /at most 8/);
        return true;
      }
    );
    assert.equal(engine.stats().execs, 0, 'nothing ran');
  } finally {
    bindings.MAX_BATCH = original;
  }
  assert.equal(bindings.MAX_BATCH, 4096);
});

test('an over-long source refuses the whole batch', () => {
  const engine = new Engine();
  const original = bindings.MAX_SOURCE_LEN;
  try {
    // Checked per item, before the crossing, so item 0 never runs either.
    bindings.MAX_SOURCE_LEN = 16;
    assert.throws(
      () => engine.runMany(['1 + 1', '1' + ' '.repeat(17)]),
      (err) => err.kind === 'argument'
    );
    assert.equal(engine.stats().execs, 0);
  } finally {
    bindings.MAX_SOURCE_LEN = original;
  }
});

test('a batch from inside a host function is refused as re-entrant', () => {
  const engine = new Engine();
  try {
    engine.registerFunction(
      'probe',
      () => {
        engine.runMany(['1 + 1']);
        return 0;
      },
      { arity: 0 }
    );
    // The refusal travels back as a host failure, which is the ABI's way of
    // saying a callback may not alias the engine that is calling it.
    assert.throws(
      () => engine.run('probe()'),
      (err) => {
        assert.equal(err.kind, 'host');
        assert.match(err.message, /already inside a host function/);
        return true;
      }
    );
    // The engine is still usable afterwards, and a single call still works.
    assert.equal(engine.evaluate('1 + 1'), 2);
  } finally {
    engine.close();
  }
});

test('omni_batch_error answers only the current batch', () => {
  const b = bindings.loadBindings();
  const handle = b.engineNew();
  const sources = ['1 + 1', 'let = 2', '1 / 0', '2 + 2'];
  const out = b.koffi.alloc(b.OmniValue, sources.length);
  const statuses = new Uint32Array(sources.length);
  const buf = Buffer.alloc(4096);
  const read = (index) => {
    const n = Number(b.batchError(handle, index, buf, buf.length));
    return n > 0 ? buf.subarray(0, n).toString('utf8') : '';
  };
  try {
    assert.equal(rawBatch(b, handle, sources, out, statuses), STATUS.ERR_COMPILE);
    assert.deepEqual(Array.from(statuses), [0, 1, 2, 0]);
    assert.equal(read(0), '');
    assert.match(read(1), /expected a name/);
    assert.match(read(2), /division by zero/);
    assert.equal(read(3), '');
    assert.equal(read(4), '', 'an index past the batch');
    // A single evaluation ends the batch's currency: there is no item 1 of a
    // call that had no items.
    const one = Buffer.from('1', 'utf8');
    b.exec(handle, one, one.length, out);
    assert.equal(read(1), '');
  } finally {
    b.koffi.free(out);
    b.engineFree(handle);
  }
});

test('the raw layer hands back arrays the caller sized', () => {
  const b = bindings.loadBindings();
  const handle = b.engineNew();
  const sources = ['1 + 1', 'no_such_function()'];
  const out = b.koffi.alloc(b.OmniValue, sources.length);
  const statuses = new Uint32Array(sources.length);
  try {
    const status = rawBatch(b, handle, sources, out, statuses);
    assert.equal(status, STATUS.ERR_RUNTIME);
    assert.deepEqual(Array.from(statuses), [0, STATUS.ERR_RUNTIME]);
    const values = b.koffi.decode(out, b.koffi.array(b.OmniValue, sources.length));
    assert.equal(values[0].tag, TAG.NUMBER);
    assert.equal(values[0].payload.number, 2);
    assert.equal(values[1].tag, TAG.UNDEF, 'a failed item is a value, not a hole');
  } finally {
    b.koffi.free(out);
    b.engineFree(handle);
  }
});

test('a batch takes Buffers and iterables, not only arrays of strings', () => {
  const engine = new Engine();
  try {
    const results = engine.runMany(new Set(['1 + 1', Buffer.from('"from a buffer"', 'utf8')]));
    assert.equal(results[0].value, 2);
    assert.equal(results[1].value, 'from a buffer');
    assert.deepEqual(engine.runMany(null), []);
    assert.deepEqual(engine.runMany(undefined), []);
  } finally {
    engine.close();
  }
});

test('a batch that outgrows its scratch keeps its results', () => {
  const engine = new Engine();
  try {
    engine.runMany(['1']);
    assert.equal(engine._batchCapacity, 8);
    const many = Array.from({ length: 20 }, (_unused, i) => `${i} + 1`);
    const results = engine.runMany(many);
    assert.equal(engine._batchCapacity, 32, 'the arrays doubled rather than resized per call');
    assert.deepEqual(results.map((r) => r.value), many.map((_s, i) => i + 1));
    // Shrinking back reuses the same memory and reads only the range it filled.
    const small = engine.runMany(['"again"']);
    assert.equal(small[0].value, 'again');
    assert.equal(engine._batchCapacity, 32);
  } finally {
    engine.close();
  }
});

test('repeated batches reserve a flat amount of memory', () => {
  const engine = new Engine();
  try {
    const programs = Array(8).fill('"a" + "b" + "c"');
    engine.runMany(programs);
    const before = engine.stats();
    for (let i = 0; i < 2000; i++) {
      assert.equal(engine.runMany(programs).length, 8);
    }
    const after = engine.stats();
    assert.equal(after.execs, before.execs + 16000);
    assert.equal(after.egressBytesReserved, before.egressBytesReserved);
    assert.equal(after.arenaBytesReserved, before.arenaBytesReserved);
    assert.equal(after.egressHighWater, before.egressHighWater);
    // The SDK's own scratch is reused, not reallocated per call.
    assert.equal(engine._batchCapacity, 8);
  } finally {
    engine.close();
  }
});

test('a batch of long strings stops growing', () => {
  const engine = new Engine();
  try {
    // Eight 40 KiB results need more than one arena chunk, which is the case a
    // single call never reaches and the reason the batch shares one rewind.
    const programs = Array(8).fill('"' + 'x'.repeat(40 * 1024) + '"');
    engine.runMany(programs);
    const before = engine.stats();
    assert.ok(before.egressBytesReserved >= 8 * 40 * 1024);
    for (let i = 0; i < 50; i++) engine.runMany(programs);
    const after = engine.stats();
    assert.equal(after.egressBytesReserved, before.egressBytesReserved, 'the egress block rewinds');
    assert.equal(after.arenaBytesReserved, before.arenaBytesReserved, 'the cursor walks mapped chunks');
  } finally {
    engine.close();
  }
});

/**
 * Hand the ABI the four arrays the caller owns, and call it from a frame that
 * keeps the encoded sources alive for the duration.
 *
 * Written out here on purpose: these tests exist to prove that the raw
 * signature works against caller-owned memory, so they cannot borrow the
 * wrapper they are meant to check.
 */
function rawBatch(b, handle, sources, out, statuses) {
  const keep = sources.map((s) => Buffer.from(s, 'utf8'));
  const addrs = new BigUint64Array(keep.length);
  const lens = new BigUint64Array(keep.length);
  for (let i = 0; i < keep.length; i++) {
    addrs[i] = BigInt(b.koffi.address(keep[i]));
    lens[i] = BigInt(keep[i].length);
  }
  statuses.fill(0);
  return b.execMany(handle, addrs, lens, keep.length, out, statuses);
}
