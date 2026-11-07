#!/usr/bin/env node
'use strict';

/**
 * The Node-side benchmark: what one evaluation costs *through the SDK*.
 *
 *     node sdks/nodejs/bench.js [iterations]
 *
 * The Rust benchmark in `crates/omni-core/examples/bench.rs` measures the
 * engine; the Python one measures ctypes. This measures koffi, which is a
 * different bridge with a different cost: it compiles the call once and pins
 * the arguments, so the crossing itself is cheap and the interesting number is
 * what the JavaScript wrapper builds per call -- the source Buffer, the decoded
 * value, the Result, and the text a string result needs.
 *
 * Method, same as the other two:
 *
 *   - zero dependencies (`node:perf_hooks` for GC accounting is builtin);
 *   - timing and accounting run in **separate passes**, because the GC observer
 *     and the traced pass change what they measure;
 *   - the figure reported is the **fastest** of `ROUNDS` rounds, not the mean;
 *   - retention comes from the engine's own counters, which are the only ones
 *     that see inside the DLL, plus the number of garbage collections a loop
 *     provokes.
 *
 * There is deliberately no heap-bytes column. V8 collects in the middle of any
 * loop this long, so a before/after `heapUsed` delta reports when the collector
 * happened to run rather than what the code allocated -- measured here, it came
 * out negative. The leak claim is carried instead by `egress-B` and
 * `egressHighWater`, which are read from the engine and stay flat across 100k
 * executions, and by a `gc/call` of zero.
 *
 * Every number this prints belongs to the machine it ran on.
 */

const { PerformanceObserver } = require('node:perf_hooks');

const omni = require('./index.js');

const { Engine } = omni;

/** Rounds per measurement; the fastest one is reported. See the module doc. */
const ROUNDS = 5;

/** Batch sizes the crossing-amortisation table walks, as the other benches do. */
const BATCH_SIZES = [1, 8, 64, 256];

/** The three workloads again, named as in the Rust and Python benches. */
const SCENARIOS = [
  { name: 'number', source: 'let answer = 40 + 2; answer' },
  { name: 'string', source: '"hello, " + "world" + "!"' },
  { name: 'host-call', source: 'square(6) + 1', hostFn: true },
];

/**
 * @param {string} source
 * @param {boolean} [wantsHostFn]
 */
function prepare(source, wantsHostFn) {
  const engine = new Engine();
  if (wantsHostFn) {
    engine.registerFunction('square', (x) => x * x, { arity: 1 });
  }
  // Warm the arena, the egress block and koffi's own call caches: the first
  // call maps a chunk and compiles the invocation, and neither is per-call.
  for (let i = 0; i < 64; i++) engine.run(source);
  return engine;
}

/**
 * Collections provoked by `iters` calls, and the milliseconds they cost.
 *
 * V8 collects when a heap limit is crossed, so the count is a lower bound on
 * bytes allocated and an upper bound on what the collector costs -- the two
 * halves of the batch claim, which is about the objects a host stops building.
 */
async function collect(work, iters) {
  let count = 0;
  let ms = 0;
  const observer = new PerformanceObserver((list) => {
    for (const entry of list.getEntries()) {
      count += 1;
      ms += entry.duration;
    }
  });
  observer.observe({ type: 'gc', buffered: false });
  const start = process.hrtime.bigint();
  for (let i = 0; i < iters; i++) work();
  const elapsed = Number(process.hrtime.bigint() - start) / iters;
  // GC callbacks are delivered asynchronously; a turn of the event loop is what
  // lets the last entries arrive before the count is read.
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setTimeout(resolve, 20));
  observer.disconnect();
  return { nsPerCall: elapsed, gcPerCall: count / iters, gcMs: ms };
}

/**
 * @param {Engine} engine
 * @param {() => unknown} work one call of the workload, result discarded
 * @param {number} iters
 */
async function measure(engine, work, iters) {
  let best = Infinity;
  for (let round = 0; round < ROUNDS; round++) {
    const start = process.hrtime.bigint();
    for (let i = 0; i < iters; i++) work();
    const ns = Number(process.hrtime.bigint() - start) / iters;
    if (ns < best) best = ns;
  }
  const accounted = await collect(work, iters);
  return {
    nsPerCall: best,
    gcPerCall: accounted.gcPerCall,
    gcMs: accounted.gcMs,
    stats: engine.stats(),
  };
}

/**
 * Fastest of `ROUNDS` single passes, with the engine's own count checked in each.
 *
 * `measure` runs the workload six times, so it cannot answer "did the engine
 * see exactly N executions" -- and the scaling claim needs that answer.
 */
function measureScaling(engine, work, iters) {
  let best = Infinity;
  for (let round = 0; round < ROUNDS; round++) {
    const before = engine.stats().execs;
    const start = process.hrtime.bigint();
    for (let i = 0; i < iters; i++) work();
    const ns = Number(process.hrtime.bigint() - start) / iters;
    const counted = engine.stats().execs - before;
    if (counted !== iters) throw new Error(`the engine counted ${counted} of ${iters} executions`);
    if (ns < best) best = ns;
  }
  return { nsPerCall: best, stats: engine.stats() };
}

/**
 * Nanoseconds of an FFI call that does no work at all.
 *
 * `omni_abi_version` reads a constant and returns it, so this is koffi's
 * overhead alone: the part of `ns/call` that is not the engine, and the part a
 * batch amortises over its whole list.
 */
function crossingCost(iters) {
  const bindings = require('./lib/bindings.js').loadBindings();
  const abi = bindings.abiVersion;
  abi();
  let best = Infinity;
  for (let round = 0; round < ROUNDS; round++) {
    const start = process.hrtime.bigint();
    for (let i = 0; i < iters; i++) abi();
    const ns = Number(process.hrtime.bigint() - start) / iters;
    if (ns < best) best = ns;
  }
  return best;
}

async function main() {
  const iters = Number(process.argv[2] || 20000);
  let failures = 0;

  console.log(
    `OmniEngine Node SDK benchmark -- ${iters} iterations x ${ROUNDS} rounds per scenario, fastest round`
  );
  const probe = new Engine();
  console.log(`library: ${probe.libraryPath}, ABI ${probe.abiVersion}, Node ${process.version}`);
  probe.close();

  console.log(`\n${'scenario'.padEnd(10)} ${'ns/call'.padStart(9)} ${'gc/call'.padStart(9)} ${'gc-ms'.padStart(8)} ${'arena-B'.padStart(9)} ${'egress-B'.padStart(9)} ${'high-B'.padStart(8)}`);
  const loopNs = {};
  for (const scenario of SCENARIOS) {
    const engine = prepare(scenario.source, scenario.hostFn);
    const m = await measure(engine, () => engine.run(scenario.source), iters);
    loopNs[scenario.name] = m.nsPerCall;
    console.log(
      `${scenario.name.padEnd(10)} ${m.nsPerCall.toFixed(1).padStart(9)} ${m.gcPerCall.toFixed(3).padStart(9)} ` +
        `${m.gcMs.toFixed(1).padStart(8)} ${m.stats.arenaBytesReserved.toString().padStart(9)} ` +
        `${m.stats.egressBytesReserved.toString().padStart(9)} ${m.stats.egressHighWater.toString().padStart(8)}`
    );
    engine.close();
  }

  console.log(`\nempty crossing -- omni_abi_version(): ${crossingCost(iters * 5).toFixed(1)} ns/call`);

  // The batch claim, on the workload that builds the most JavaScript objects.
  const stringSource = SCENARIOS[1].source;
  const engine = prepare(stringSource, false);
  console.log('\nbatch -- the `string` scenario, N items per crossing (per item)');
  console.log(
    `${'N'.padStart(5)} ${'ns/item'.padStart(10)} ${'gc/item'.padStart(9)} ${'gc-ms'.padStart(8)} ` +
      `${'egress-B'.padStart(9)} ${'high-B'.padStart(8)} ${'vs loop'.padStart(9)}`
  );
  const reservations = new Set();
  for (const size of BATCH_SIZES) {
    const programs = new Array(size).fill(stringSource);
    for (let i = 0; i < 8; i++) engine.runMany(programs);
    const rounds = Math.max(Math.floor(iters / size), 1);
    // One `work()` is one crossing here, so the per-item figures fall out of the
    // per-crossing ones by dividing by N.
    const m = await measure(engine, () => engine.runMany(programs), rounds);
    reservations.add(m.stats.egressBytesReserved);
    console.log(
      `${String(size).padStart(5)} ${(m.nsPerCall / size).toFixed(1).padStart(10)} ` +
        `${(m.gcPerCall / size).toFixed(3).padStart(9)} ${m.gcMs.toFixed(1).padStart(8)} ` +
        `${m.stats.egressBytesReserved.toString().padStart(9)} ${m.stats.egressHighWater.toString().padStart(8)} ` +
        `${(loopNs.string / (m.nsPerCall / size)).toFixed(2).padStart(8)}x`
    );
  }
  if (reservations.size !== 1) {
    console.error(`FAIL: egress reservation moved between batch sizes: ${[...reservations]}`);
    failures += 1;
  }
  // The scratch the SDK keeps is proof of the reuse the loop cannot have.
  console.log(`scratch: capacity ${engine._batchCapacity} for a max batch of ${omni.MAX_BATCH}`);
  engine.close();

  // The scaling claim: ten times the work may not cost more per item, and must
  // not reserve one extra byte.
  console.log('\nscaling -- the `string` scenario at three sizes');
  const scaled = prepare(stringSource, false);
  let baseNs = 0;
  let baseReserved = 0;
  for (const n of [1000, 10000, 100000]) {
    const { nsPerCall, stats } = measureScaling(scaled, () => scaled.run(stringSource), n);
    console.log(
      `${String(n).padStart(8)} ${nsPerCall.toFixed(1).padStart(10)} ns/call ` +
        `egress ${String(stats.egressBytesReserved).padStart(9)} high-water ${String(stats.egressHighWater).padStart(9)} ` +
        `execs ${stats.execs}`
    );
    if (n === 1000) {
      baseNs = nsPerCall;
      baseReserved = stats.egressBytesReserved;
    } else if (nsPerCall > baseNs * 2) {
      console.error(`FAIL: ns/call grew from ${baseNs.toFixed(1)} to ${nsPerCall.toFixed(1)} at ${n}`);
      failures += 1;
    }
    if (stats.egressBytesReserved !== baseReserved) {
      console.error(`FAIL: egress reservation grew to ${stats.egressBytesReserved} at ${n}`);
      failures += 1;
    }
  }
  scaled.close();

  console.log(failures === 0 ? '\nall benchmark checks passed' : `\n${failures} benchmark check(s) failed`);
  return failures === 0 ? 0 : 1;
}

main().then(
  (code) => process.exit(code),
  (err) => {
    console.error(err);
    process.exit(1);
  }
);
