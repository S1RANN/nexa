// Benchmark v7 - warm V8 comparison harness (JIT decision input c4).
//
// This script mirrors the three comparable pure-computation product
// workloads from tools/benchmark-v7/src/main.rs (LANGUAGE_SOURCE) in
// idiomatic JavaScript: plain numbers, plain arrays, object literals,
// Math.trunc for i32 division. Every value stays far inside the SMI
// range, so double arithmetic is exact and the results must equal the
// Nexa interpreter's returns bit for bit.
//
// Protocol (BENCHMARK_PROTOCOL_V1.md shape): per-process warmup then
// fixed samples timed with process.hrtime.bigint(); the xtask driver
// spawns independent processes and takes the median across process
// medians. Warm V8 means the JIT has seen the workload during warmup;
// that asymmetry versus the Nexa interpreter is the point of the
// comparison (JIT_DECISION_V1.md).
"use strict";

// Pinned expected results; the xtask driver additionally cross-checks
// them against the Nexa side via `nexa-benchmark-v7 --verify-products`.
const EXPECTED = {
  product_data_sweep: 32640,
  product_combat_tick: 633,
  product_grid_score: 157992,
};

function productDataSweep() {
  const values = [];
  let index = 0;
  while (index < 256) {
    const cell = { value: index, wide: 9, label: "sweep" };
    values.push(cell.value);
    index = index + 1;
  }
  let total = 0;
  let cursor = 0;
  while (cursor < 256) {
    total = total + values[cursor];
    cursor = cursor + 1;
  }
  return total;
}

function productCombatTick() {
  const attack = [];
  const defense = [];
  const health = [];
  let index = 0;
  while (index < 128) {
    attack.push(10 + index);
    defense.push(3 + Math.trunc(index / 2));
    health.push(100);
    index = index + 1;
  }
  let round = 0;
  let defeated = 0;
  while (round < 8) {
    let cursor = 0;
    while (cursor < 128) {
      const raw = attack[cursor] - defense[127 - cursor];
      let damage = raw;
      if (raw < 1) {
        damage = 1;
      }
      const remaining = health[cursor] - damage;
      health[cursor] = remaining;
      if (remaining < 1) {
        defeated = defeated + 1;
      }
      cursor = cursor + 1;
    }
    round = round + 1;
  }
  return defeated + health[0];
}

function productGridScore() {
  let score = 0;
  let y = 0;
  while (y < 64) {
    let x = 0;
    while (x < 64) {
      const dx = x - 32;
      const dy = y - 32;
      let cell = dx * dx + dy * dy;
      if (cell > 1024) {
        cell = 1024;
      }
      score = score + Math.trunc(cell / 16);
      x = x + 1;
    }
    y = y + 1;
  }
  return score;
}

const WORKLOADS = [
  ["product_data_sweep", productDataSweep],
  ["product_combat_tick", productCombatTick],
  ["product_grid_score", productGridScore],
];

function argumentValue(name, fallback) {
  const index = process.argv.indexOf(name);
  if (index < 0 || index + 1 >= process.argv.length) {
    return fallback;
  }
  const parsed = Number.parseInt(process.argv[index + 1], 10);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`invalid value for ${name}`);
  }
  return parsed;
}

// Mirrors the Rust harness exactly: samples[(len - 1) * percent / 100]
// with integer division, so per-process percentiles are comparable.
function percentile(sorted, percent) {
  if (sorted.length === 0) {
    return 0n;
  }
  return sorted[Math.trunc(((sorted.length - 1) * percent) / 100)];
}

function benchCase(name, operation, samples, warmup) {
  const expected = EXPECTED[name];
  // Warm V8 on purpose: the JIT tiers up during these iterations.
  for (let i = 0; i < warmup; i += 1) {
    if (operation() !== expected) {
      throw new Error(`${name} warmup diverged from the pinned result`);
    }
  }
  const durations = new Array(samples);
  for (let i = 0; i < samples; i += 1) {
    const started = process.hrtime.bigint();
    const result = operation();
    const elapsed = process.hrtime.bigint() - started;
    // The comparison is a real use of the result, so V8 cannot delete
    // the workload as dead code.
    if (result !== expected) {
      throw new Error(`${name} sample diverged from the pinned result`);
    }
    durations[i] = elapsed;
  }
  durations.sort((left, right) => (left < right ? -1 : left > right ? 1 : 0));
  let total = 0n;
  for (const duration of durations) {
    total += duration;
  }
  return {
    case: name,
    samples,
    result: expected,
    mean_ns: Number(total / BigInt(samples)),
    p50_ns: Number(percentile(durations, 50)),
    p90_ns: Number(percentile(durations, 90)),
    p95_ns: Number(percentile(durations, 95)),
    p99_ns: Number(percentile(durations, 99)),
    min_ns: Number(durations[0]),
    max_ns: Number(durations[durations.length - 1]),
  };
}

function main() {
  const samples = argumentValue("--samples", 1000);
  const warmup = argumentValue("--warmup", 100);
  const processIndex = argumentValue("--process-index", 0);
  if (samples === 0) {
    throw new Error("samples must be positive");
  }
  const cases = WORKLOADS.map(([name, operation]) =>
    benchCase(name, operation, samples, warmup),
  );
  const report = {
    schema: 1,
    harness: "benchmark-v7-v8-comparison",
    node_version: process.version,
    v8_version: process.versions.v8,
    process_index: processIndex,
    samples,
    warmup,
    cases,
  };
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
}

main();
