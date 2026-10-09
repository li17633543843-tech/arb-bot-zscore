# Stateful Z-score spread strategy

This fork adds a stateful cross-venue mean-reversion strategy alongside the upstream funding and
spread views. It does not remove or weaken the upstream execution safety controls.

## Signal

For a venue pair, each synchronized book update contributes:

```text
spread_bps = ln(venue_a_mid / venue_b_mid) * 10_000
```

Each pair owns an independent rolling window. The strategy opens only when all gates pass:

1. at least 300 synchronized samples;
2. `2.5 <= abs(z_score) <= 8`;
3. the same direction remains valid for at least 3 seconds;
4. book timestamps differ by no more than 500 ms;
5. projected convergence PnL is at least 0.10 quote currency after entry and exit depth, all four
   taker fees, and safety costs.

The cheaper venue is bought and the more expensive venue is sold relative to the rolling mean.
Convergence exits require `abs(z_score) <= 0.5` and a minimum five-second hold. A Z-score stop at
4 and a two-hour maximum hold remain immediate safety exits.

## Integration boundary

`arb_scanner::zscore::Engine` is pure stateful logic. It performs no network or account IO and
cannot place orders. Public feeds, paper trading and live execution must all call this same engine
so the CLI, dashboard and executor cannot drift into different signal definitions.

The next integration stages are:

1. Bybit and Bitget synchronized public order-book feed;
2. multi-level projected convergence PnL adapter;
3. append-only signal and rejection ledger;
4. paper broker wiring through the existing upstream preflight/executor/reconcile path;
5. only after replay validation, explicitly enabled live execution.

## Attribution

This project is derived from `a3165458/arb-bot` under the MIT License. The original copyright and
license notice remain in `LICENSE`.
