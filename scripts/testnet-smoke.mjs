#!/usr/bin/env node
// Live public-testnet lifecycle. Uses only test tokens and records public receipts.
import assert from "node:assert/strict";
import { readFileSync, writeFileSync, existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import * as nearAPI from "near-api-js";
import { Chain } from "../api/src/chain.ts";

if (!process.argv.includes("--execute")) throw new Error("This sends testnet transactions. Pass --execute.");
const base = JSON.parse(readFileSync("data/testnet-session.json", "utf8"));
assert.equal(base.network, "testnet");
const credentialPath = process.env.NEAR_ADMIN_CREDENTIALS;
if (!credentialPath) throw new Error("set NEAR_ADMIN_CREDENTIALS to the venue credential file");
const owner = JSON.parse(readFileSync(credentialPath, "utf8"));
const ownerKey = nearAPI.utils.KeyPair.fromString(owner.private_key ?? owner.secret_key);
const keys = new nearAPI.keyStores.InMemoryKeyStore();
await keys.setKey("testnet", base.admin.account, ownerKey);
const near = await nearAPI.connect({ networkId: "testnet", nodeUrl: base.rpc, keyStore: keys });
const admin = await near.account(base.admin.account);
mkdirSync("data/private", { recursive: true, mode: 0o700 });
const visitorPath = "data/private/testnet-visitor.json";
let visitor;
if (existsSync(visitorPath)) visitor = JSON.parse(readFileSync(visitorPath, "utf8"));
else {
  const key = nearAPI.utils.KeyPair.fromRandom("ed25519");
  visitor = { account: `qa-${Date.now().toString(36)}.${base.admin.account}`, secret: key.toString(), publicKey: key.getPublicKey().toString() };
  writeFileSync(visitorPath, JSON.stringify(visitor), { mode: 0o600 });
}
try { await (await near.account(visitor.account)).state(); }
catch (error) {
  if (!/doesn't exist|does not exist|UNKNOWN_ACCOUNT|AccountDoesNotExist/.test(String(error)) && error?.type !== "AccountDoesNotExist") throw error;
  await admin.createAccount(visitor.account, nearAPI.utils.PublicKey.fromString(visitor.publicKey), 100000000000000000000000n);
}
const signing = { ...base, admin: { ...base.admin, secret: ownerKey.toString() }, roles: { you: visitor } };
const signingPath = "data/private/testnet-smoke-session.json";
writeFileSync(signingPath, JSON.stringify(signing), { mode: 0o600 });
const chain = await Chain.connect(signingPath);
const url = process.env.GATEWAY_URL ?? "http://127.0.0.1:8790";
const events = [];
const evidencePath = "data/testnet-lifecycle.json";
function save(status = "running") {
  writeFileSync(evidencePath, JSON.stringify({ network: "testnet", at: new Date().toISOString(), account: visitor.account, contracts: base.contracts, status, events }, null, 2));
}
async function call(role, method, args, options = {}) {
  const result = await chain.call(role, method, args, options);
  events.push({ method, hash: result.hash, value: result.value }); save();
  console.log(`${method}: ${result.hash}`);
  return result.value;
}
async function planned(body) {
  const r = await fetch(`${url}/plan`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ ...body, account: visitor.account }) });
  const p = await r.json(); assert.equal(p.ok, true, p.error);
  for (const tx of p.transactions) {
    assert.equal(tx.signerId, visitor.account);
    assert.ok([chain.options, chain.token, chain.oracle].includes(tx.receiverId));
    for (const action of tx.actions) {
      const a = action.params;
      await call("you", a.methodName, a.args, { contract: tx.receiverId, gas: BigInt(a.gas), deposit: a.deposit });
    }
  }
}
console.log(`testnet visitor ${visitor.account}`);
const refused = await fetch(`${url}/action`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ type: "withdraw", amount: 1 }) });
assert.equal(refused.status, 403, "public gateway must never sign visitor actions");
const claim = await fetch(`${url}/faucet`, { method: "POST", headers: { "content-type": "application/json", origin: url }, body: JSON.stringify({ account: visitor.account }) });
const grant = await claim.json(); assert.equal(grant.ok, true, grant.error);
events.push({ method: "faucet", hash: grant.hash, amount: grant.amount }); save();
assert.equal(await chain.ftBalance(visitor.account), 1000000000);
await planned({ type: "deposit", amount: 500 });
assert.equal(await chain.balance("you"), 500000000);
await planned({ type: "withdraw", amount: 50 });
assert.equal(await chain.balance("you"), 450000000);
assert.equal(await chain.ftBalance(visitor.account), 550000000);
const spot = (await (await fetch("http://127.0.0.1:8791/state?asset=ETH")).json()).spot;
assert.ok(spot > 0, "live ETH index must be available");
const expiry = BigInt(Date.now() + 150000) * 1000000n;
const low = BigInt(Math.floor(spot) - 25) * 1000000n;
const series = await call("admin", "create_series", {
  underlying: "ETH", kind: "Call", expiry_ns: Number(expiry), accept_until_ns: Number(expiry + 60000000000n), finalize_until_ns: Number(expiry + 300000000000n),
  low_strike: low.toString(), high_strike: (low + 50000000n).toString(), price_scale: "1000000", min_confidence_bps: 3000, oracle_feed: "ETH-USD",
}, { deposit: "3440000000000000000000", gas: 80000000000000n });
await call("admin", "set_pool_quote", { series_id: series, mid_units: "25000000", stamp_ns: Date.now() * 1e6 }, { deposit: "880000000000000000000" });
const ticket = await call("you", "pool_buy", { series_id: series, quantity: "1000000", max_total_premium: "26000000" });
await call("you", "pool_close", { ticket_id: ticket, min_exit_units: "24000000" });
await call("admin", "set_pool_quote", { series_id: series, mid_units: "25000000", stamp_ns: Date.now() * 1e6 });
const held = await call("you", "pool_buy_held", { series_id: series, quantity: "1000000", max_total_premium: "25000000" });
let fixed;
const deadline = Date.now() + 270000;
while (Date.now() < deadline) {
  const row = await chain.view("get_series", { series_id: series });
  if (row?.settlement?.Fixed) { fixed = row; break; }
  console.log(`waiting for automatic expiry settlement of series ${series}`);
  await new Promise((resolve) => setTimeout(resolve, 10000));
}
assert.ok(fixed, "operator must fix the expired series using live oracle observations");
assert.ok(fixed.settle_sources?.length >= 2, "settlement requires at least two sources");
await call("you", "settle_ticket", { ticket_id: held });
const paid = await chain.view("get_ticket", { ticket_id: held });
assert.equal(paid, null, "settled ticket is removed from open tickets");
const available = await chain.balance("you");
await planned({ type: "withdraw", amount: available / 1e6 });
assert.equal(await chain.balance("you"), 0);
const custody = await chain.custody(); assert.equal(custody.conserved, true);
events.push({ method: "verified", series, ticket, held, sources: fixed.settle_sources, custody, pool: await chain.view("get_pool_state"), finalWallet: await chain.ftBalance(visitor.account) });
save("passed");
console.log(`PASSED · receipts: ${evidencePath}`);
