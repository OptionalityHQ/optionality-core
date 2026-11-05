#!/usr/bin/env node
// Read-only chain/API verification. Never runs the worker or sends a transaction.
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { createHash, createPublicKey, verify } from "node:crypto";

const artifact = JSON.parse(readFileSync("data/confidential-probe-artifact.json"));
const receipt = JSON.parse(readFileSync("data/confidential-probe-receipt.json"));
if (artifact.network !== "testnet" || receipt.network !== "testnet") throw Error("testnet evidence only");
const directory = mkdtempSync(join(tmpdir(), "optionality-attestation-"));
try {
  const bundlePath = join(directory, "proof.json");
  const run = (args) => {
    const result = spawnSync(process.env.OUTLAYER_VERIFY_BIN ?? "outlayer-verify", args, {
      encoding: "utf8", timeout: 60_000, maxBuffer: 1024 * 1024,
    });
    if (result.error || result.status !== 0) {
      // Do not modify the saved success evidence on a network or verifier error.
      throw Error(`Attestation verification did not pass (exit ${result.status ?? "unavailable"}); saved evidence unchanged`);
    }
  };
  run(["tx", receipt.transaction, "--network", "testnet", "--api", "https://testnet-api.outlayer.ai",
    "--rpc", "https://test.rpc.fastnear.com", "--archival-rpc", "https://test.rpc.fastnear.com",
    "--bundle", bundlePath, "--short"]);
  run(["bundle", bundlePath, "--offline", "--short"]);
  const bytes = readFileSync(bundlePath);
  const proof = JSON.parse(bytes);
  if (proof.network !== "testnet" || proof.attestation.wasm_hash !== artifact.hash
    || proof.attestation.transaction_hash !== receipt.transaction
    || proof.attestation.project_id !== receipt.project) throw Error("attested execution does not match the pinned probe");
  for (const field of ["authenticity", "identity", "binding", "input", "output"]) {
    if (proof.verification[field]?.verdict !== "pass") throw Error(`Unproven attestation field: ${field}`);
  }
  // The bundle binds these exact output bytes to the hardware quote.
  const output = JSON.parse(proof.output_raw);
  if (!output.probe_only || !output.sealed_storage || !Number.isSafeInteger(output.counter) || output.counter < 1) {
    throw Error("capability probe did not succeed");
  }
  const count = Buffer.alloc(8); count.writeBigUInt64LE(BigInt(output.counter));
  const expectedDigest = createHash("sha256").update("optionality-confidential-probe-v1").update(count).digest();
  if (expectedDigest.toString("hex") !== output.digest) throw Error("probe digest mismatch");
  const publicKey = createPublicKey({ format: "der", type: "spki",
    key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), Buffer.from(output.public_key, "hex")]) });
  if (!verify(null, expectedDigest, publicKey, Buffer.from(output.signature, "hex"))) throw Error("probe receipt signature is invalid");
  writeFileSync("data/confidential-probe-attestation.json", bytes);
  const updated = { ...receipt, output, signatureVerified: true, hardwareAttestationVerified: true,
    verifiedAt: new Date().toISOString(), attestation: {
      taskId: proof.attestation.task_id, wasmHash: proof.attestation.wasm_hash,
      evidence: "data/confidential-probe-attestation.json",
      evidenceSha256: createHash("sha256").update(bytes).digest("hex"), offlineRecheckPassed: true,
    } };
  writeFileSync("data/confidential-probe-receipt.json", JSON.stringify(updated, null, 2) + "\n");
  console.log(JSON.stringify({ probeOnly: true, attestationVerified: true, signatureVerified: true,
    artifactHash: artifact.hash, transaction: receipt.transaction, noTransactionSent: true }));
} finally { rmSync(directory, { recursive: true, force: true }); }
