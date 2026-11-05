#!/usr/bin/env node
// Controlled read-only comparison. Every request is dry; public mode is only a
// diagnostic control, never an execution fallback. Never creates funding addresses.
import { randomBytes } from "node:crypto";
import { verifyQuoteSignature } from "@defuse-protocol/one-click-sdk-typescript";
const key = process.env.NEAR_INTENTS_API_KEY;
if (!key) throw new Error("Load .env with node --env-file=.env");
const headers = { "content-type": "application/json", "X-API-Key": key };
const assetsResponse = await fetch("https://1click.chaindefuser.com/v0/tokens", { headers, signal: AbortSignal.timeout(20_000) });
if (!assetsResponse.ok) throw new Error(`Assets HTTP ${assetsResponse.status}`);
const assets = await assetsResponse.json();
const source = assets.find((t) => t.blockchain === "eth" && t.symbol === "USDC");
const destination = assets.find((t) => t.blockchain === "arb" && t.symbol === "USDC");
if (!source || !destination) throw new Error("Diagnostic assets are not in the live token catalog");
const recipient = "0x" + randomBytes(20).toString("hex");
const refundTo = "0x" + randomBytes(20).toString("hex");
for (const mode of ["public", "basic", "advanced"]) {
  const request = { dry: true, swapType: "EXACT_INPUT", slippageTolerance: 100,
    originAsset: source.assetId, destinationAsset: destination.assetId,
    amount: (100n * 10n ** BigInt(source.decimals)).toString(),
    depositType: "ORIGIN_CHAIN", recipientType: "DESTINATION_CHAIN", refundType: "ORIGIN_CHAIN",
    // Fixed dummy destinations are used ONLY in dry quote diagnostics.
    recipient, refundTo,
    confidentiality: mode, quoteWaitingTimeMs: 3000,
    deadline: new Date(Date.now() + 20 * 60_000).toISOString() };
  const started = Date.now();
  try {
    const response = await fetch("https://1click.chaindefuser.com/v0/quote", { method: "POST", headers, body: JSON.stringify(request), redirect: "error", signal: AbortSignal.timeout(25_000) });
    const body = await response.json();
    if (!response.ok) console.log(JSON.stringify({ mode, dry: true, status: response.status, elapsedMs: Date.now() - started, message: String(body.message ?? "request failed").replaceAll(key, "[REDACTED]"), correlationId: body.correlationId }));
    else {
      if (body.quote?.depositAddress) throw new Error("dry response contains deposit address; refusing it");
      let valid = false; try { valid = verifyQuoteSignature(body); } catch {}
      console.log(JSON.stringify({ mode, dry: true, status: response.status, elapsedMs: Date.now() - started, signatureValid: valid, echoedMode: body.quoteRequest?.confidentiality, amountIn: body.quote?.amountIn, amountOut: body.quote?.amountOut }));
    }
  } catch (error) { console.log(JSON.stringify({ mode, dry: true, error: String(error.message).replaceAll(key, "[REDACTED]") })); }
}
