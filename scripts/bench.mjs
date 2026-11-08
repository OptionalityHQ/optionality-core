#!/usr/bin/env node
import { spawn } from "node:child_process";

const PORT = 9875;
const ENGINE_URL = `http://127.0.0.1:${PORT}`;

async function runBenchmark(url, concurrency, totalRequests) {
  let completed = 0;
  const latencies = [];
  const start = performance.now();

  async function worker() {
    while (completed < totalRequests) {
      completed++;
      const reqStart = performance.now();
      try {
        const res = await fetch(url);
        await res.text();
        const duration = performance.now() - reqStart;
        latencies.push(duration);
      } catch (err) {
        latencies.push(null);
      }
    }
  }

  const workers = Array.from({ length: concurrency }, () => worker());
  await Promise.all(workers);
  const totalDuration = (performance.now() - start) / 1000;

  const validLatencies = latencies.filter((l) => l !== null).sort((a, b) => a - b);
  const sum = validLatencies.reduce((a, b) => a + b, 0);
  const avg = sum / validLatencies.length;
  const p50 = validLatencies[Math.floor(validLatencies.length * 0.5)];
  const p95 = validLatencies[Math.floor(validLatencies.length * 0.95)];
  const p99 = validLatencies[Math.floor(validLatencies.length * 0.99)];
  const rps = validLatencies.length / totalDuration;

  return { rps, avg, p50, p95, p99, totalDuration, count: validLatencies.length };
}

async function main() {
  const engine = spawn(
    "./target/release/engine",
    ["--port", String(PORT), "--assets", "ETH,BTC", "--replay", "data/feed-sample.jsonl", "--quiet"],
    { stdio: "ignore" }
  );

  await new Promise((r) => setTimeout(r, 800));

  console.log("=== Benchmarking Rust Pricing Engine (Release Build) ===");

  for (const endpoint of ["/health", "/feed", "/state?asset=ETH", "/plan?asset=ETH"]) {
    console.log(`\nEndpoint: ${endpoint}`);
    for (const c of [1, 10, 50]) {
      const res = await runBenchmark(`${ENGINE_URL}${endpoint}`, c, 2000);
      console.log(
        `  Concurrency ${c.toString().padEnd(2)}: ${res.rps.toFixed(0).padStart(6)} req/s | avg: ${res.avg.toFixed(2)}ms | p50: ${res.p50.toFixed(2)}ms | p95: ${res.p95.toFixed(2)}ms | p99: ${res.p99.toFixed(2)}ms`
      );
    }
  }

  engine.kill();
  process.exit(0);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
