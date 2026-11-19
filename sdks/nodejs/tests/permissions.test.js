'use strict';

/**
 * Permission tests for the Node SDK: the ABI 5 grant surface.
 *
 * Run with:
 *
 *     cd sdks/nodejs
 *     npm test
 *
 * They need the built engine (`cargo build -p omni-ffi`). The rules under
 * test are the sandbox contract: no grant means every filesystem touch fails
 * closed with a positioned error, a grant opens exactly the path it names,
 * sibling engines never inherit anything, and parallel workers carry the
 * parent's roots.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { Engine, OmniError, STATUS } = require('../index.js');

/** A path as an Omni string literal, backslashes escaped for the lexer. */
function q(p) {
  return `"${String(p).replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

function tempRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'omni-node-perm-'));
}

function engine() {
  const e = new Engine();
  return { e, close: () => e.close() };
}

test('reads fail closed until the root is granted', () => {
  const { e, close } = engine();
  const root = tempRoot();
  try {
    const target = path.join(root, 'a.txt');
    fs.writeFileSync(target, 'from a');

    assert.throws(
      () => e.run(`fs.read_text(${q(target)})`),
      (err) =>
        err instanceof OmniError &&
        err.kind === 'runtime' &&
        err.message.includes(
          'fs.read_text is disabled: this engine has no filesystem read permission'
        )
    );

    e.allowFsRead(root);
    assert.equal(e.run(`fs.read_text(${q(target)})`).value, 'from a');
  } finally {
    close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('a grant opens one root and names the request when denied', () => {
  const { e, close } = engine();
  const root = tempRoot();
  const outsideDir = tempRoot();
  try {
    const inside = path.join(root, 'in.txt');
    const outside = path.join(outsideDir, 'out.txt');
    fs.writeFileSync(inside, 'inside');
    fs.writeFileSync(outside, 'outside');

    e.allowFsRead(root);
    assert.equal(e.run(`fs.read_text(${q(inside)})`).value, 'inside');

    assert.throws(
      () => e.run(`fs.read_text(${q(outside)})`),
      (err) =>
        err instanceof OmniError &&
        err.kind === 'runtime' &&
        err.message.includes(`path '${outside}' is outside the allowed read roots`)
    );
  } finally {
    close();
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(outsideDir, { recursive: true, force: true });
  }
});

test('writes fail closed until the root is granted', () => {
  const { e, close } = engine();
  const root = tempRoot();
  try {
    const target = path.join(root, 'a.txt');

    assert.throws(
      () => e.run(`fs.write_text(${q(target)}, "x")`),
      (err) =>
        err instanceof OmniError &&
        err.kind === 'runtime' &&
        err.message.includes(
          'fs.write_text is disabled: this engine has no filesystem write permission'
        )
    );
    assert.equal(fs.existsSync(target), false, 'a denied write must not touch the disk');

    e.allowFsWrite(root);
    e.run(`fs.write_text(${q(target)}, "written")`);
    assert.equal(fs.readFileSync(target, 'utf8'), 'written');
  } finally {
    close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('the module root gates imports until granted', () => {
  const { e, close } = engine();
  const root = tempRoot();
  try {
    fs.writeFileSync(path.join(root, 'helper.omni'), 'let answer = 40 + 2');

    assert.throws(
      () => e.run('import "helper"\nhelper.answer'),
      (err) =>
        err instanceof OmniError &&
        err.kind === 'runtime' &&
        err.message.includes('import is disabled: this engine has no module root')
    );

    e.setModuleRoot(root);
    assert.equal(e.run('import "helper"\nhelper.answer').value, 42);
  } finally {
    close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('grants never leak between engines', () => {
  const a = engine();
  const b = engine();
  const root = tempRoot();
  try {
    const target = path.join(root, 'n.txt');
    fs.writeFileSync(target, '7');

    a.e.allowFsRead(root);
    assert.equal(a.e.run(`fs.read_text(${q(target)})`).value, '7');

    assert.throws(
      () => b.e.run(`fs.read_text(${q(target)})`),
      (err) =>
        err instanceof OmniError &&
        err.message.includes(
          'fs.read_text is disabled: this engine has no filesystem read permission'
        )
    );
  } finally {
    a.close();
    b.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('parallel workers inherit the parent\'s grants', () => {
  const { e, close } = engine();
  const root = tempRoot();
  try {
    const target = path.join(root, 'n.txt');
    fs.writeFileSync(target, '7');
    const program = `fs.read_text(${q(target)})`;

    e.allowFsRead(root);
    const results = e.runParallel([program, program], 2);
    assert.deepEqual(results.map((r) => r.value), ['7', '7']);
  } finally {
    close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('host misuse is a deterministic error, not a crash', () => {
  const { e, close } = engine();
  const root = tempRoot();
  try {
    for (const grant of [e.allowFsRead.bind(e), e.allowFsWrite.bind(e), e.setModuleRoot.bind(e)]) {
      assert.throws(() => grant(''), (err) => err instanceof OmniError && err.kind === 'argument');
      assert.throws(
        () => grant(path.join(root, 'no-such-directory')),
        (err) => err instanceof OmniError && err.kind === 'argument'
      );
      assert.throws(
        () => grant('a'.repeat(4096 + 1)),
        (err) => err instanceof OmniError && err.kind === 'argument'
      );
    }

    // A string with a NUL would be silently truncated by a C-string layer, so
    // the wrapper refuses it before any bytes cross.
    assert.throws(() => e.allowFsRead('ok\0hidden'), TypeError);

    // Refused grants change nothing: the engine still evaluates.
    assert.equal(e.evaluate('1 + 1'), 2);
  } finally {
    close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('a Buffer path is accepted', () => {
  const { e, close } = engine();
  const root = tempRoot();
  try {
    const target = path.join(root, 'p.txt');
    fs.writeFileSync(target, 'ok');
    e.allowFsRead(Buffer.from(root, 'utf8'));
    assert.equal(e.run(`fs.read_text(${q(target)})`).value, 'ok');
  } finally {
    close();
    fs.rmSync(root, { recursive: true, force: true });
  }
});
