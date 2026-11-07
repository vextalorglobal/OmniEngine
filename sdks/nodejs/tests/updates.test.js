'use strict';

/**
 * Release checks: one manifest in, one verdict out, no download.
 *
 * Every test injects the document or points at a `file://` mirror, so the suite
 * never touches the network. The comparison itself runs in the loaded library,
 * which is why the tests prefer a freshly built artifact: a checkout's bundled
 * `native/` copy can predate the update export, and the point here is the ABI,
 * not the age of a local file.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

const omni = require('../index.js');
const { loadBindings } = require('../lib/bindings.js');

/** A built engine from this checkout, when there is one, wins over the bundle. */
function builtLibrary() {
  const name = omni.libraryFileName();
  for (const profile of ['debug', 'release']) {
    const candidate = path.join(__dirname, '..', '..', '..', 'target', profile, name);
    if (fs.existsSync(candidate)) return candidate;
  }
  return undefined;
}

const libraryPath = builtLibrary();
const bindings = loadBindings(libraryPath);
const skip =
  typeof bindings.checkUpdate === 'function'
    ? false
    : 'the loaded engine predates omni_check_update; cargo build -p omni-ffi';
const opts = skip ? { skip } : {};

function manifest(version, extra = {}) {
  const key = omni.platformKey();
  const assets = key
    ? {
        [key]: {
          url: `https://example.invalid/omni-${key}-${version}.zip`,
          sha256: 'a'.repeat(64),
        },
      }
    : {};
  return JSON.stringify({
    version,
    abi: omni.ABI_VERSION,
    notes: `release ${version}`,
    assets,
    ...extra,
  });
}

function tempFile(contents) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'omni-update-'));
  const file = path.join(dir, 'omni-update.json');
  fs.writeFileSync(file, contents);
  return { dir, file };
}

test('a build that matches the release is up to date', opts, async () => {
  const result = await omni.checkUpdates({ manifest: manifest(omni.VERSION), libraryPath });
  assert.equal(result.status, 'up-to-date');
  assert.equal(result.current, omni.VERSION);
  assert.equal(result.latest, omni.VERSION);
  assert.equal(result.ok, true);
  assert.equal(result.available, false);
  assert.equal(result.error, null);
  assert.equal(result.url, null); // an injected document names no source
  assert.equal(String(result), `omni ${omni.VERSION} is up to date`);
});

test('a newer release is available, with notes and this platform asset', opts, async () => {
  const result = await omni.checkUpdates({ manifest: manifest('0.2.0'), libraryPath });
  assert.equal(result.status, 'available');
  assert.equal(result.available, true);
  assert.equal(result.ok, true);
  assert.equal(result.latest, '0.2.0');
  assert.equal(result.notes, 'release 0.2.0');
  assert.equal(result.abi, omni.ABI_VERSION);
  assert.match(String(result), /0\.2\.0 is available/);

  const key = omni.platformKey();
  if (key) {
    assert.deepEqual(result.asset, {
      url: `https://example.invalid/omni-${key}-0.2.0.zip`,
      sha256: 'a'.repeat(64),
    });
  } else {
    assert.equal(result.asset, null);
  }
});

test('an older release is not an upgrade', opts, async () => {
  const result = await omni.checkUpdates({ manifest: manifest('0.0.9'), libraryPath });
  assert.equal(result.status, 'up-to-date');
  assert.equal(result.available, false);
});

test('a release for another ABI is a mismatch, not an upgrade', opts, async () => {
  const result = await omni.checkUpdates({
    manifest: manifest('0.2.0', { abi: omni.ABI_VERSION + 1 }),
    libraryPath,
  });
  assert.equal(result.status, 'abi-mismatch');
  assert.equal(result.ok, true);
  assert.equal(result.available, false);
  assert.match(String(result), new RegExp(`ABI ${omni.ABI_VERSION}`));
});

test('an unreadable manifest is a verdict, not a crash', opts, async () => {
  const result = await omni.checkUpdates({ manifest: '{"version":', libraryPath });
  assert.equal(result.status, 'invalid');
  assert.equal(result.ok, false);
  assert.equal(result.latest, '');
  assert.equal(result.error, null);
});

test('an oversized manifest is refused before it is parsed', opts, async () => {
  const oversized = 'x'.repeat(omni.MAX_MANIFEST_LEN + 1);
  const result = await omni.checkUpdates({ manifest: oversized, libraryPath });
  assert.equal(result.status, 'bad-argument');
  assert.equal(result.ok, false);
});

test('a file:// manifest is read without a network stack', opts, async () => {
  const { dir, file } = tempFile(manifest('0.2.0'));
  try {
    const result = await omni.checkUpdates({ url: pathToFileURL(file).href, libraryPath });
    assert.equal(result.status, 'available');
    assert.equal(result.url, pathToFileURL(file).href);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('plain HTTP is refused rather than followed', opts, async () => {
  const saved = process.env.OMNI_ALLOW_INSECURE;
  delete process.env.OMNI_ALLOW_INSECURE;
  try {
    const result = await omni.checkUpdates({ url: 'http://example.invalid/omni-update.json', libraryPath });
    assert.equal(result.status, 'error');
    assert.match(result.error, /refusing plain HTTP/);
    assert.match(result.error, /OMNI_ALLOW_INSECURE/);
  } finally {
    if (saved !== undefined) process.env.OMNI_ALLOW_INSECURE = saved;
  }
});

test('a transport failure is data by default and an exception when asked', opts, async () => {
  const offline = () => {
    throw new Error('offline');
  };
  const quiet = await omni.checkUpdates({ url: 'https://example.invalid/x.json', fetch: offline, libraryPath });
  assert.equal(quiet.status, 'error');
  assert.match(quiet.error, /Error: offline/);
  assert.equal(quiet.ok, false);

  await assert.rejects(
    omni.checkUpdates({ url: 'https://example.invalid/x.json', fetch: offline, libraryPath, raiseOnError: true }),
    /offline/
  );
});

test('an injected reader receives the URL and the timeout', opts, async () => {
  const seen = [];
  const reader = (url, timeoutMs) => {
    seen.push([url, timeoutMs]);
    return manifest('0.2.0');
  };
  const result = await omni.checkUpdates({ url: 'https://mirror.invalid/omni.json', timeoutMs: 1234, fetch: reader, libraryPath });
  assert.deepEqual(seen, [['https://mirror.invalid/omni.json', 1234]]);
  assert.equal(result.status, 'available');
  assert.equal(result.url, 'https://mirror.invalid/omni.json');
});

test('OMNI_UPDATE_URL names the source when the caller does not', opts, async () => {
  const saved = process.env.OMNI_UPDATE_URL;
  const seen = [];
  process.env.OMNI_UPDATE_URL = 'https://mirror.invalid/env.json';
  try {
    const result = await omni.checkUpdates({
      fetch: (url) => {
        seen.push(url);
        return manifest('0.2.0');
      },
      libraryPath,
    });
    assert.deepEqual(seen, ['https://mirror.invalid/env.json']);
    assert.equal(result.url, 'https://mirror.invalid/env.json');
  } finally {
    if (saved === undefined) delete process.env.OMNI_UPDATE_URL;
    else process.env.OMNI_UPDATE_URL = saved;
  }
});

test('an Engine starts a check only when asked', opts, async () => {
  const { dir, file } = tempFile(manifest('0.2.0'));
  const saved = process.env.OMNI_UPDATE_URL;
  process.env.OMNI_UPDATE_URL = pathToFileURL(file).href;
  try {
    const plain = new omni.Engine({ libraryPath });
    const asked = new omni.Engine({ libraryPath, updateCheck: true });
    try {
      assert.equal(plain.updateCheck, null);
      assert.ok(asked.updateCheck instanceof Promise);
      const result = await asked.updateCheck;
      assert.equal(result.status, 'available');
      assert.equal(asked.run('6 * 7').value, 42); // the check never blocks the engine
    } finally {
      plain.close();
      asked.close();
    }
  } finally {
    if (saved === undefined) delete process.env.OMNI_UPDATE_URL;
    else process.env.OMNI_UPDATE_URL = saved;
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('OMNI_UPDATE_CHECK=1 enables the check without an option', opts, async () => {
  const { dir, file } = tempFile(manifest(omni.VERSION));
  const savedUrl = process.env.OMNI_UPDATE_URL;
  const savedCheck = process.env.OMNI_UPDATE_CHECK;
  process.env.OMNI_UPDATE_URL = pathToFileURL(file).href;
  process.env.OMNI_UPDATE_CHECK = '1';
  try {
    const engine = new omni.Engine({ libraryPath });
    try {
      assert.ok(engine.updateCheck instanceof Promise);
      const result = await engine.updateCheck;
      assert.equal(result.status, 'up-to-date');
    } finally {
      engine.close();
    }
  } finally {
    if (savedUrl === undefined) delete process.env.OMNI_UPDATE_URL;
    else process.env.OMNI_UPDATE_URL = savedUrl;
    if (savedCheck === undefined) delete process.env.OMNI_UPDATE_CHECK;
    else process.env.OMNI_UPDATE_CHECK = savedCheck;
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
