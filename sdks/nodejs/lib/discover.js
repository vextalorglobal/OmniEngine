'use strict';

/**
 * Finds the OmniEngine shared library.
 *
 * Order, most explicit first:
 *   1. OMNI_ENGINE_LIBRARY  -- a full path to the artifact
 *   2. OMNI_ENGINE_HOME     -- a project root to search inside
 *   3. native/<platform>-<arch>/ beside this package -- bundled by a release
 *   4. the workspace this SDK was checked out with, target/debug then release
 *   5. the bare platform name, which lets the OS loader use PATH
 *
 * Nothing here downloads or builds a binary. From a checkout the engine is the
 * cargo artifact; a release tarball carries one built from the tagged commit by
 * CI, per platform, so the bundled file cannot be older than the package that
 * names it.
 */

const fs = require('node:fs');
const path = require('node:path');

/** File name a host loads on this platform. */
function libraryFileName() {
  if (process.platform === 'win32') return 'omni.dll';
  if (process.platform === 'darwin') return 'libomni.dylib';
  return 'libomni.so';
}

/** Directories a cargo build could plausibly have written the artifact into. */
const SEARCH_SUBDIRS = ['', 'target/debug', 'target/release', 'lib', 'bin'];

/**
 * `base` and each of its parents, as places a cargo `target/` could live.
 *
 * An installed package sits in node_modules, so a fixed number of `..` never
 * reaches the buyer's project. Walking up does, and it costs nothing.
 */
function upwardTargets(base, name, levels = 6) {
  const found = [];
  let current = path.resolve(base);
  for (let i = 0; i <= levels; i++) {
    // Check `current` before moving up, so the directory you passed in counts.
    for (const profile of ['debug', 'release']) {
      found.push(path.join(current, 'target', profile, name));
    }
    const parent = path.dirname(current);
    if (parent === current) break; // filesystem root
    current = parent;
  }
  return found;
}

/**
 * Every candidate path, in priority order.
 *
 * @param {{ env?: NodeJS.ProcessEnv, basedir?: string, cwd?: string }} [options]
 * @returns {string[]}
 */
function candidatePaths(options = {}) {
  const env = options.env || process.env;
  const name = libraryFileName();
  const packageDir = options.basedir || __dirname;
  const found = [];

  if (env.OMNI_ENGINE_LIBRARY) found.push(env.OMNI_ENGINE_LIBRARY);

  if (env.OMNI_ENGINE_HOME) {
    for (const sub of SEARCH_SUBDIRS) {
      found.push(sub ? path.join(env.OMNI_ENGINE_HOME, ...sub.split('/'), name) : path.join(env.OMNI_ENGINE_HOME, name));
    }
  }

  // A release package bundles one directory per platform and architecture;
  // `native/win32-x64/omni.dll` is the file a Windows x64 install loads.
  const packageRoot = path.resolve(packageDir, '..');
  found.push(path.join(packageRoot, 'native', `${process.platform}-${process.arch}`, name));

  // A package that bundles the native artifact puts it next to index.js.
  found.push(path.join(packageRoot, name));
  found.push(path.join(packageDir, name));

  // A checkout or a linked install resolves outwards from the package itself;
  // an installed one resolves outwards from wherever the command was run.
  found.push(...upwardTargets(packageDir, name));
  found.push(...upwardTargets(options.cwd || process.cwd(), name));

  const seen = new Set();
  return found.filter((p) => {
    const key = path.normalize(p).toLowerCase();
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

/**
 * The first candidate that exists on disk.
 *
 * @param {{ env?: NodeJS.ProcessEnv, basedir?: string }} [options]
 * @returns {string} an absolute path
 * @throws {OmniLibraryError} when nothing was found
 */
function resolveLibrary(options = {}) {
  const tried = candidatePaths(options);
  for (const candidate of tried) {
    if (candidate && fs.existsSync(candidate) && fs.statSync(candidate).isFile()) {
      return path.resolve(candidate);
    }
  }

  // Fall back to letting the loader search PATH / LD_LIBRARY_PATH, which is how
  // an installed system package is picked up.
  const env = options.env || process.env;
  const bare = libraryFileName();
  const fromPath = searchPathFor(bare, env);
  if (fromPath) return fromPath;

  throw new OmniLibraryError(
    `could not find ${bare}.\n` +
      'A release install carries the engine in native/<platform>-<arch>/; if\n' +
      'that file is missing, reinstall the package for this platform.\n' +
      'From a checkout, build it first:\n' +
      '    cargo build -p omni-ffi\n' +
      'The SDK then finds it in any target/debug or target/release directory\n' +
      'above this package or above the current working directory.\n' +
      'For a library elsewhere, set OMNI_ENGINE_LIBRARY to its full path, or\n' +
      'OMNI_ENGINE_HOME to the project root.\n' +
      'Locations tried:\n' +
      tried.map((p) => `  ${p}`).join('\n')
  );
}

/** Ask PATH, the way the OS loader would. */
function searchPathFor(name, env = process.env) {
  const dirs = (env.PATH || env.Path || '').split(path.delimiter);
  for (const dir of dirs) {
    if (!dir) continue;
    const full = path.join(dir, name);
    try {
      if (fs.statSync(full).isFile()) return full;
    } catch {
      /* not here, keep looking */
    }
  }
  return null;
}

/** Raised when no engine library exists. Carries the fix in its message. */
class OmniLibraryError extends Error {
  constructor(message) {
    super(message);
    this.name = 'OmniLibraryError';
  }
}

module.exports = { libraryFileName, candidatePaths, resolveLibrary, OmniLibraryError };
