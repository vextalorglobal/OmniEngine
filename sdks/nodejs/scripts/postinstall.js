'use strict';

/**
 * Verify the engine that came in this tarball, once, at install time.
 *
 * A release package carries one directory per platform under `native/`. This
 * script loads the one for the current machine and lets the binding check the
 * ABI level and the struct layout, so a mismatched or truncated artifact fails
 * where it was installed rather than on the first evaluation.
 *
 * It deliberately does not download, build, or repair anything:
 *
 *   OMNI_SKIP_POSTINSTALL=1     do nothing (offline or air-gapped installs)
 *   OMNI_STRICT_POSTINSTALL=1   missing native artifact is an error
 *
 * With no bundle for this platform the install still succeeds: the runtime
 * falls back to a `target/` build or `OMNI_ENGINE_LIBRARY`, which is the only
 * case a checkout install is in.
 */

const fs = require('node:fs');
const path = require('node:path');
const { libraryFileName } = require('../lib/discover');

function note(message) {
  console.log(`omniengine: ${message}`);
}

function main() {
  if (process.env.OMNI_SKIP_POSTINSTALL === '1') {
    note('postinstall verification skipped (OMNI_SKIP_POSTINSTALL=1)');
    return 0;
  }

  const platform = `${process.platform}-${process.arch}`;
  const artifact = path.join(__dirname, '..', 'native', platform, libraryFileName());

  if (!fs.existsSync(artifact)) {
    const strict = process.env.OMNI_STRICT_POSTINSTALL === '1';
    const message =
      `no bundled engine for ${platform} (looked for ${artifact}).\n` +
      'Build one with `cargo build -p omni-ffi`, or set OMNI_ENGINE_LIBRARY.';
    if (strict) {
      console.error(`omniengine: ${message}`);
      return 1;
    }
    note(message);
    return 0;
  }

  let koffi;
  try {
    koffi = require('koffi');
  } catch {
    note(`bundled engine present for ${platform}; koffi is not installed, so it was not verified`);
    return 0;
  }

  try {
    const bindings = require('../lib/bindings');
    // loadBindings declares the whole ABI, checks the struct layout, and
    // compares the library's ABI level with the SDK's before returning.
    bindings.loadBindings(artifact);
    note(`bundled engine verified (${platform}, ABI ${bindings.ABI_VERSION})`);
    return 0;
  } catch (err) {
    console.error(`omniengine: the bundled engine for ${platform} failed to load:\n${err.message}`);
    return 1;
  }
}

process.exitCode = main();
