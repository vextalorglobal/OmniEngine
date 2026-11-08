'use strict';

/**
 * Phase 9 diagnostics tests: per-opcode heatmaps over the Node boundary.
 *
 * The profiler answers questions a timer alone cannot: which opcodes a program
 * dispatched, how often, and where its nanoseconds went. These tests pin the
 * SDK's half of that contract -- the struct layout, the name lookup, the
 * always-on counters versus the opt-in timing split, and the fact that a
 * profile always describes the last evaluation.
 */

const test = require('node:test');
const assert = require('node:assert/strict');

const omni = require('../index.js');
const bindings = require('../lib/bindings.js');

const { Engine } = omni;

const MNEMONICS = [
  'PUSH_NUMBER',
  'PUSH_STR',
  'PUSH_FOLDED',
  'PUSH_NIL',
  'PUSH_UNDEF',
  'LOAD',
  'STORE',
  'POP',
  'ADD',
  'SUB',
  'MUL',
  'DIV',
  'MOD',
  'EQ',
  'NE',
  'NEG',
  'JUMP',
  'JUMP_IF_FALSE',
  'CALL',
  'RET',
  'IMPORT',
];

test('a fresh engine profiles as all zeros', () => {
  const engine = new Engine();
  try {
    const profile = engine.profile();
    assert.equal(profile.totalInstructions, 0);
    assert.equal(profile.elapsedNs, 0);
    assert.equal(profile.peakBytes, 0);
    // Every opcode this build knows is present, even the unused ones, so a
    // heatmap sums the same way on every build.
    assert.deepEqual(Object.keys(profile.opcodeCalls).sort(), [...MNEMONICS].sort());
    assert.deepEqual(Object.keys(profile.opcodeNs).sort(), [...MNEMONICS].sort());
    assert.equal(Object.values(profile.opcodeCalls).reduce((a, b) => a + b, 0), 0);
    assert.equal(Object.values(profile.opcodeNs).reduce((a, b) => a + b, 0), 0);
  } finally {
    engine.close();
  }
});

test('the heatmap sums to the instruction count', () => {
  const engine = new Engine();
  try {
    const result = engine.run('let x = 2 + 3\nx * 4');
    assert.equal(result.value, 20);
    const profile = engine.profile();
    assert.equal(
      Object.values(profile.opcodeCalls).reduce((a, b) => a + b, 0),
      profile.totalInstructions
    );
    assert.ok(profile.totalInstructions > 0);
    assert.equal(profile.opcodeCalls.ADD, 0); // folded at compile time
    assert.equal(profile.opcodeCalls.PUSH_FOLDED, 1);
    assert.equal(profile.opcodeCalls.STORE, 1);
    assert.equal(profile.opcodeCalls.LOAD, 1);
    assert.equal(profile.opcodeCalls.MUL, 1);
  } finally {
    engine.close();
  }
});

test('per-opcode timing stays zero until enabled', () => {
  const engine = new Engine();
  try {
    engine.run('1 + 1');
    let profile = engine.profile();
    assert.equal(Object.values(profile.opcodeNs).reduce((a, b) => a + b, 0), 0);
    assert.ok(profile.elapsedNs > 0, 'the scalar timer is always on');

    engine.setProfiling(true);
    engine.run('let x = 1\nx' + ' + 1'.repeat(2000));
    profile = engine.profile();
    const timed = Object.values(profile.opcodeNs).reduce((a, b) => a + b, 0);
    assert.ok(timed > 0);
    assert.ok(timed <= profile.elapsedNs);

    engine.setProfiling(false);
    engine.run('let x = 1\nx' + ' + 1'.repeat(2000));
    assert.equal(Object.values(engine.profile().opcodeNs).reduce((a, b) => a + b, 0), 0);
  } finally {
    engine.close();
  }
});

test('peakBytes bounds a string-heavy program', () => {
  const engine = new Engine();
  try {
    engine.run('1 + 2');
    assert.equal(engine.profile().peakBytes, 0);
    const text = '"' + 'x'.repeat(64000) + '"';
    const result = engine.run(text);
    assert.equal(result.value.length, 64000);
    const profile = engine.profile();
    assert.ok(profile.peakBytes >= 64000);
    assert.ok(profile.peakBytes < 2 * 64000);
  } finally {
    engine.close();
  }
});

test('the profile is per execution', () => {
  const engine = new Engine();
  try {
    engine.run('"' + 'x'.repeat(64000) + '"');
    engine.run('7');
    const profile = engine.profile();
    assert.equal(profile.totalInstructions, 2); // PUSH_NUMBER + RET
    assert.equal(profile.peakBytes, 0);
  } finally {
    engine.close();
  }
});

test('a failed execution is profiled like a successful one', () => {
  const engine = new Engine();
  try {
    engine.setProfiling(true);
    assert.throws(() => engine.run('1 / 0'), (err) => {
      assert.equal(err.kind, 'runtime');
      assert.match(err.message, /division by zero/);
      return true;
    });
    const profile = engine.profile();
    assert.equal(profile.opcodeCalls.DIV, 1);
    assert.equal(
      Object.values(profile.opcodeCalls).reduce((a, b) => a + b, 0),
      profile.totalInstructions
    );
    assert.ok(Object.values(profile.opcodeNs).reduce((a, b) => a + b, 0) > 0);
  } finally {
    engine.close();
  }
});

test('a batch profiles its last item', () => {
  const engine = new Engine();
  try {
    const results = engine.runMany(['2 + 21', '2 * 21']);
    assert.deepEqual(results.map((r) => r.value), [23, 42]);
    const profile = engine.profile();
    assert.equal(profile.totalInstructions, 2); // PUSH_FOLDED + RET
    assert.equal(profile.opcodeCalls.PUSH_FOLDED, 1);
  } finally {
    engine.close();
  }
});

test('the profile struct matches the C header', () => {
  const b = bindings.loadBindings();
  assert.equal(b.koffi.sizeof(b.OmniProfile), 1064);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'size'), 0);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'opcode_count'), 4);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'total_instructions'), 8);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'elapsed_ns'), 16);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'peak_bytes'), 24);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'reserved'), 32);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'opcode_calls'), 40);
  assert.equal(b.koffi.offsetof(b.OmniProfile, 'opcode_ns'), 552);
  assert.equal(omni.MAX_OPCODES, 64);
});
