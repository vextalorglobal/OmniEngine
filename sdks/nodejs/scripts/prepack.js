'use strict';

/**
 * Pre-publish guard for the npm package.
 *
 * Runs automatically from `prepack` (and therefore `npm pack` / `npm publish`).
 * It answers one question: is this tarball safe to hand to strangers?
 *
 *   1. The three version numbers in the repository agree, and the release tag
 *      (GITHUB_REF_NAME, or --tag vX.Y.Z) matches them.
 *   2. Every ABI constant -- C header, Python SDK, Node SDK -- is the same.
 *   3. With --require-native (or OMNI_REQUIRE_NATIVE=1), all five platform
 *      artifacts are present and non-empty. Without it, missing ones are a
 *      warning so a checkout can still be packed for local testing.
 *
 * It reads files only. It never builds, downloads, or publishes anything.
 */

const fs = require('node:fs');
const path = require('node:path');

const PACKAGE_ROOT = path.resolve(__dirname, '..');
const REPO_ROOT = path.resolve(PACKAGE_ROOT, '..', '..');

const PLATFORMS = [
  ['win32-x64', 'omni.dll'],
  ['linux-x64', 'libomni.so'],
  ['linux-arm64', 'libomni.so'],
  ['darwin-x64', 'libomni.dylib'],
  ['darwin-arm64', 'libomni.dylib'],
];

const args = process.argv.slice(2);
const requireNative =
  args.includes('--require-native') || process.env.OMNI_REQUIRE_NATIVE === '1';
const tagArg = (() => {
  const at = args.indexOf('--tag');
  if (at !== -1 && at + 1 < args.length) return args[at + 1];
  return process.env.GITHUB_REF_NAME || process.env.RELEASE_TAG || '';
})();

const failures = [];

function fail(message) {
  failures.push(message);
}

function warn(message) {
  console.warn(`omniengine: warning: ${message}`);
}

function read(file) {
  return fs.readFileSync(file, 'utf8');
}

function match(text, pattern, what) {
  const found = text.match(pattern);
  if (!found) throw new Error(`could not read ${what}`);
  return found[1];
}

function versions() {
  const cargo = read(path.join(REPO_ROOT, 'Cargo.toml'));
  const pyproject = read(path.join(REPO_ROOT, 'sdks', 'python', 'pyproject.toml'));
  const pkg = JSON.parse(read(path.join(PACKAGE_ROOT, 'package.json')));
  return {
    cargo: match(cargo, /^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m, 'workspace version'),
    python: match(pyproject, /^\[project\][\s\S]*?^version\s*=\s*"([^"]+)"/m, 'pyproject version'),
    node: pkg.version,
  };
}

function abiLevels() {
  const header = read(path.join(REPO_ROOT, 'crates', 'omni-ffi', 'include', 'omni.h'));
  const python = read(path.join(REPO_ROOT, 'sdks', 'python', 'omni', '_ffi.py'));
  const node = read(path.join(PACKAGE_ROOT, 'lib', 'bindings.js'));
  return {
    header: match(header, /^#define\s+OMNI_ABI_VERSION\s+(\d+)/m, 'OMNI_ABI_VERSION in omni.h'),
    python: match(python, /^OMNI_ABI_VERSION\s*=\s*(\d+)/m, 'OMNI_ABI_VERSION in _ffi.py'),
    node: match(node, /^const ABI_VERSION\s*=\s*(\d+);/m, 'ABI_VERSION in bindings.js'),
  };
}

function checkVersions() {
  if (!fs.existsSync(path.join(REPO_ROOT, 'Cargo.toml'))) {
    warn('not inside the repository; version cross-check skipped');
    return;
  }
  const v = versions();
  if (v.cargo !== v.python || v.cargo !== v.node) {
    fail(
      `version mismatch: Cargo.toml ${v.cargo}, pyproject.toml ${v.python}, ` +
        `package.json ${v.node}. One release, one version.`
    );
  }
  if (tagArg) {
    const tag = tagArg.replace(/^v/, '');
    if (tag !== v.cargo) {
      fail(`release tag ${tagArg} names ${tag}, but the packages say ${v.cargo}`);
    }
  }
  console.log(`omniengine: version ${v.node}${tagArg ? ` (tag ${tagArg})` : ''}`);
}

function checkAbi() {
  if (!fs.existsSync(path.join(REPO_ROOT, 'crates', 'omni-ffi', 'include', 'omni.h'))) {
    warn('ABI cross-check needs the repository; skipped');
    return;
  }
  const abi = abiLevels();
  const levels = new Set([abi.header, abi.python, abi.node]);
  if (levels.size !== 1) {
    fail(
      `ABI mismatch: omni.h ${abi.header}, Python ${abi.python}, Node ${abi.node}. ` +
        'Every surface must speak the same wire format.'
    );
  }
  console.log(`omniengine: ABI ${abi.node}`);
}

function checkNative() {
  let present = 0;
  const missing = [];
  for (const [platform, name] of PLATFORMS) {
    const file = path.join(PACKAGE_ROOT, 'native', platform, name);
    if (fs.existsSync(file) && fs.statSync(file).size > 0) {
      present += 1;
    } else {
      missing.push(`${platform}/${name}`);
    }
  }
  if (missing.length === 0) {
    console.log(`omniengine: native artifacts ${present}/${PLATFORMS.length}`);
    return;
  }
  const message = `missing native artifacts: ${missing.join(', ')}`;
  if (requireNative) {
    fail(message);
  } else {
    warn(`${message} (allowed without --require-native)`);
  }
}

try {
  checkVersions();
  checkAbi();
  checkNative();
} catch (err) {
  fail(err.message);
}

if (failures.length > 0) {
  for (const message of failures) console.error(`omniengine: ${message}`);
  process.exitCode = 1;
}
