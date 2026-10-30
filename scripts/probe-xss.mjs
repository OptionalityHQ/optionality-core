// Red-team probe: can a series name planted by the venue operator put script
// into every visitor's page? Local sandbox only.
import { Chain, failureText } from "../api/src/chain.ts";

const chain = await Chain.connect("data/chain-session.json");
const nowNs = BigInt(Date.now()) * 1_000_000n;
const payload = "<img src=x onerror=alert(1)>";

try {
  const out = await chain.call("admin", "create_series", {
    underlying: payload,
    kind: "Put",
    expiry_ns: Number(nowNs / 1_000_000_000n) * 1_000_000_000 + 3_600_000_000_000,
    accept_until_ns: Number(nowNs / 1_000_000_000n) * 1_000_000_000 + 3_900_000_000_000,
    finalize_until_ns: Number(nowNs / 1_000_000_000n) * 1_000_000_000 + 4_800_000_000_000,
    low_strike: "2000000000",
    high_strike: "2100000000",
    price_scale: "1000000",
    min_confidence_bps: 3000,
    oracle_feed: "ETH-USD",
  });
  console.log("ACCEPTED, series", JSON.stringify(out.value));
} catch (error) {
  console.log("REFUSED:", failureText(error).slice(0, 200));
}

const series = await chain.view("get_series", { series_id: process.argv[2] ?? 0 });
console.log("read back:", JSON.stringify(series).slice(0, 200));
