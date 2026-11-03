// The `using` form the README promises, executed as real syntax rather than a
// prototype call. Run by `npm test`, which picks up *.test.mjs too.
import test from 'node:test';
import assert from 'node:assert/strict';

import { Engine } from '../index.js';

test('await using releases the engine at the end of the block', async () => {
  let inner;
  await (async () => {
    await using engine = new Engine();
    assert.equal(engine.evaluate('let answer = 40 + 2'), 42);
    inner = engine;
  })();
  // The block has exited, so the handle is gone even though nothing called close().
  assert.throws(() => inner.run('1'), (err) => err.kind === 'argument');
});
