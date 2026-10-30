// Red-team probe #2: can a caller price the vault?
//
// `pool_buy` and `pool_close` used to take whatever `mid_units` the sender named,
// so one contract could be bought for 2 units and handed back to the vault at a
// mid of 100,000,000 — the LPs paying 90.00 for a ticket that cost 0.000002,
// repeatable until the equity ran out.
//
// The signature answers it: a taker sends a quantity and bounds, never a price.
// This probe tries the three doors that remain and measures the round trip.
//
// Local sandbox, signed by a role the gateway never uses, exactly as a direct
// caller would. Exits non-zero if any attack goes through.
import { Chain } from "../api/src/chain.ts";

const chain = await Chain.connect("data/chain-session.json");
const attacker = "maker-2";
const SCALE = 1_000_000;

const stats = await chain.view("stats");
const open = [];
for (let id = 0; id < Number(stats.series_count); id += 1) {
  const series = await chain.view("get_series", { series_id: id });
  if (series?.settlement === "Open" && Number(series.expiry_ns) > Number(Date.now()) * 1e6) open.push({ id, series });
}
// The cheapest cell the vault is actually selling, so the round trip below runs
// on one contract's price rather than on a budget someone guessed. Read in the
// batches the contract allows — 65 ids in one call is a refusal, not a slow read.
const priced = [];
for (let start = 0; start < open.length; start += 64) {
  const batch = open.slice(start, start + 64).map((o) => o.id);
  const rows = await chain.view("get_pool_prices", { series_ids: batch, quantity: String(SCALE) });
  priced.push(...rows);
}
if (!Array.isArray(priced) || priced.length === 0) throw new Error("no open series carries a live venue quote");
const sellable = priced.filter((row) => BigInt(row.entry_units) >= 10_000n);
const entry = sellable.reduce((cheapest, row) => (BigInt(row.entry_units) < BigInt(cheapest.entry_units) ? row : cheapest));
const target = open.find((o) => o.id === Number(entry.series_id));
const quote = await chain.view("get_pool_quote", { series_id: target.id });
const budget = BigInt(entry.entry_units) * 3n;
console.log(
  `series ${target.id} ${target.series.underlying} ${target.series.low_strike}/${target.series.high_strike} ·`,
  "the venue's own quote:",
  JSON.stringify(quote),
  "· entry the vault charges:",
  entry.entry_units,
  "units",
);

const attacks = [];
const attackerAccount = chain.session.roles[attacker].account;
const tried = async (label, run) => {
  try {
    const value = await run();
    // Only the contract's return value: a call result also carries `gas_burnt`,
    // which is a BigInt and cannot be stringified.
    const shown = typeof value === "object" && value !== null && "value" in value ? value.value : value;
    attacks.push(`${label}: ALLOWED (${JSON.stringify(shown)})`);
    console.log(`${label}: ALLOWED`, JSON.stringify(shown));
    return shown;
  } catch (error) {
    const why = String(error instanceof Error ? error.message : error).split("\n")[0];
    console.log(`${label}: refused — ${why}`);
    return null;
  }
};

// The read is capped, and the cap is what stops one view call from pricing a
// thousand series in a block nobody else can afford to replay.
await tried("65 series priced in one view call", () =>
  chain.view("get_pool_prices", { series_ids: open.slice(0, 65).map((o) => o.id), quantity: String(SCALE) }));

// 1. A price sent to a call that no longer has a price argument. serde drops the
//    unknown field, so the number cannot reach the arithmetic: the ticket costs
//    what the vault charges, which is the whole finding.
const bought = await chain.call(attacker, "pool_buy", {
  series_id: target.id,
  quantity: String(SCALE),
  mid_units: "1",
  max_total_premium: budget.toString(),
}).then((out) => Number(out.value));
const held = await chain.view("get_ticket", { ticket_id: bought });
const charged = BigInt(held.premium);
console.log(
  `buy with "mid_units": 1 attached → ticket ${bought} cost ${charged} units,` +
    ` the vault's loaded entry of ${entry.entry_units} — not the 1 unit sent`,
);
console.log(charged === BigInt(entry.entry_units) ? "  PASS: the argument is inert" : "  FAIL: the caller moved the price");
if (charged !== BigInt(entry.entry_units)) attacks.push(`the sent mid was used: ${charged} vs ${entry.entry_units}`);

// 2. The sell half: a floor is a right to refuse, not a price to be paid.
await tried(`close ${bought} demanding ten times the mid back`, () =>
  chain.call(attacker, "pool_close", { ticket_id: bought, min_exit_units: (BigInt(entry.entry_units) * 10n).toString() }));
const still = await chain.view("get_ticket", { ticket_id: bought });
console.log("a refused close left the ticket with:", still?.buyer);
if (still?.buyer !== undefined && still.buyer !== attackerAccount) attacks.push("a refused close moved the ticket");

// 3. And the honest round trip, both numbers the vault's own: what a taker pays
//    to open and to close is the margin plus the haircut, in that direction.
const paid = await chain.call(attacker, "pool_close", { ticket_id: bought, min_exit_units: null });
const exit = BigInt(paid.value);
console.log(
  `round trip: paid ${charged} units, got back ${exit} units →`,
  `${(Number(exit - charged) / SCALE).toFixed(6)} USDT to the taker`,
);
console.log(exit < charged ? "  PASS: a round trip costs, it does not print" : "  FAIL: the round trip paid the taker");
if (exit >= charged) attacks.push(`round trip returned ${exit} on a cost of ${charged}`);

// 4. A stranger cannot answer for the venue.
await tried("stranger sets the venue's quote", () =>
  chain.call(attacker, "set_pool_quote", {
    series_id: target.id,
    mid_units: String(100 * SCALE),
    stamp_ns: Number(Date.now()) * 1e6,
  }));
const standing = await chain.view("get_pool_quote", { series_id: target.id });
console.log("the venue's quote is still", JSON.stringify(standing));

console.log("\nverdict:", attacks.length === 0 ? "the vault prices itself; a taker only sets bounds" : attacks.join("\n"));
process.exit(attacks.length === 0 ? 0 : 1);
