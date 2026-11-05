#!/usr/bin/env node
// Read-only: dry quote only. Does not fund, sign, broadcast or generate a deposit address.
import { previewConfidentialFunding } from "../api/src/confidential-intents.ts";
const read = (name) => {
  const at = process.argv.indexOf(name);
  if (at < 0 || !process.argv[at + 1]) throw new Error(`Missing ${name}`);
  return process.argv[at + 1];
};
const typeAt = process.argv.indexOf("--recipient-type");
const recipientType = typeAt < 0 ? "DESTINATION_CHAIN" : process.argv[typeAt + 1];
const quote = await previewConfidentialFunding({
  recipientType,
  originAsset: read("--origin-asset"), destinationAsset: read("--destination-asset"),
  amount: read("--amount"), recipient: read("--recipient"), refundTo: read("--refund-to"),
});
// Print only economic summary, never keys, full payloads or destination addresses.
console.log(JSON.stringify({ dry: true, requestedConfidentiality: quote.quoteRequest.confidentiality,
  amountIn: quote.quote.amountIn, amountOut: quote.quote.amountOut,
  minAmountOut: quote.quote.minAmountOut, signatureVerified: true,
  note: "A signed quote is not proof that funds have been shielded or settled." }, null, 2));
