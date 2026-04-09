# QMT Integration Notes

This note captures the main pitfalls, debugging results, and current working rules from integrating QMT into this repo.

The conclusions below were verified mainly on `600309.SH`, especially:

- `2026-04-07 09:30:00` to `2026-04-07 11:30:59`
- completed trading days `2026-03-30` to `2026-04-03`

## Main Conclusions

- For historical `tick`, the reliable path in this environment is:
  - `download_history_data2(..., "tick", ...)`
  - then `get_market_data_ex_ori(..., period="tick", ...)`
- For historical minute bars such as `5m` and `30m`, the reliable path in this environment is:
  - `download_history_data(..., "5m"/"30m", ...)`
  - then `get_market_data_ex(..., period="5m"/"30m", ...)`
- `download_history_data2(...)` does not return the historical payload. Treat it as a cache-fill trigger only.
- Historical `tick.pvolume` was effectively unusable in our tests. Historical `tick.volume` was usable.
- Historical `tick.volume` behaves like cumulative volume in lots/hands, not raw shares. For A-shares, `1 lot = 100 shares`.
- Synthetic trade price should use `lastPrice`, not `amount_delta / volume_delta`.
- Synthetic trade quantity should use `delta(volume) * 100`.
- Historical `tick.stockStatus` was often `0` for every row, so it is not reliable as the only session filter.
- Historical `tick.high` and `tick.low` are session-cumulative extrema, not per-trade extrema.
- Opening buckets need a seed/baseline tick from before the requested range if we want accurate first-bar volume and price.
- Closing ticks can arrive slightly after `15:00:00`; a small close-grace window is required so the final bucket does not drop the last price/volume jump.
- QMT historical data should be modeled as a single raw source: `tick`.
- For QMT footprint history, fetching `Kline` and `Trades` as two separate history jobs is the wrong architecture when both are derived from the same `tick` payload.
- Current QMT footprint history now uses one historical `tick` fetch to derive both `Kline` and `Trade`.
- Completed-day tick cache should be per-symbol, per-trading-day. Current-day tick cache should be separate and mutable.
- For current-day cache merge, `timestamp` is the only dedupe key. If history and live both contain the same timestamp, the later record overrides the earlier one.
- QMT trading-calendar support in this client is incomplete. The bridge must not assume that `get_trading_calendar()` or `get_holidays()` works.
- For holiday handling in this environment, `exchange_calendars` is the practical fallback.

## Important Recent Changes

### 1. QMT history is now a single `tick` source

Current architecture:

- bridge provides raw historical `tick`
- Rust fetches `tick` once
- Rust derives both:
  - synthetic `Kline`
  - synthetic `Trade`

This replaced the previous split design where:

- history `Kline` fetch triggered one tick download
- history `Trades` fetch triggered another tick download

That split caused duplicated QMT work, inconsistent completion timing, and footprint history being starved by kline backfill.

### 2. QMT footprint initial fetch now uses a combined history request

For QMT footprint panes:

- initial pane-open history fetch uses one combined request
- visible-gap backfill also uses one combined request
- the combined result is inserted as:
  - historical klines
  - historical trades

This removed a major source of "kline is visible but footprint is still empty".

### 3. Tick cache is now per symbol, not global

Current completed-day cache rule:

- key: `(symbol, trading_day)`
- retention: recent trading days per symbol
- current cap: `32` trading days per symbol

This is important because a global cache allowed one active symbol to evict another symbol's recent days too aggressively.

### 4. Current-day cache is separate from completed-day cache

Completed days:

- can be cached as immutable day snapshots

Current day:

- must stay mutable
- history and live websocket ticks are merged before synthetic derivation

Current merge rule:

- use `timestamp` as the unique key
- sort by `timestamp`
- if the same timestamp appears twice, the later record overrides the earlier record

This is intentionally not additive:

- do not add `volume`
- do not add `amount`
- do not blend `price`

QMT tick is cumulative data. The correct merge behavior is replacement/override, not accumulation.

### 5. Failed historical day fetches now cool down briefly

If one historical `(symbol, trading_day)` fetch fails or times out:

- Rust records a short failure cooldown
- repeated requests for the same day within that cooldown return quickly instead of hammering the bridge again immediately

Current cooldown:

- `60s`

This prevents the UI from repeatedly re-requesting a day that just timed out.

### 6. Non-trading days are rejected before tick download

Bridge-side historical tick fetch now resolves trading days first.

If the requested day is not a trading day:

- return an empty tick result immediately
- do not call `download_history_data2(...)`

This matters because otherwise holiday gaps can trigger useless:

- `download_history_data2(...)`
- `get_market_data_ex_ori(...)`
- worker timeout
- repeated HTTP `500`

This was the direct reason Qingming-adjacent gaps were producing noisy `/api/v1/ticks` failures.

### 7. QMT footprint history now derives `Kline` and `Trade` together

The combined history path is now explicit in Rust:

- fetch one historical tick range
- derive `Kline`
- derive `Trade`
- deliver both together to the chart

This is implemented as a combined `KlinesAndTrades` fetch result instead of two unrelated historical jobs.

Practical effect:

- footprint no longer depends on a second independent historical trade job finishing later
- visible kline history and footprint history stay in sync

### 8. QMT combined history is now fetched one trading day at a time

The current footprint/history UX problem was not only "how much data is fetched", but also "how long one historical task lives".

Earlier behavior:

- one combined historical task could keep walking many trading days in sequence
- while that long task was still alive, viewport changes could not redirect historical priority cleanly
- bridge logs looked like a long uninterrupted series of `/api/v1/ticks` calls
- the pane could stay in `fetching` even though some visible history had already been fetched

Current rule:

- one QMT combined historical request should only return one trading-day batch
- after that batch is inserted, the chart reevaluates what range is still missing
- viewport changes are then free to change the next requested day

### 9. "Latest trading day chunk" must skip empty day overlaps

A subtle scheduling bug existed in `qmt_latest_history_chunk_range(...)`.

Failure pattern:

- the requested range ended exactly at the open of an already-loaded later day
- the helper looked at that latest day first
- overlap with that day was empty
- the helper returned `None`
- the adapter then fell back to the entire multi-day range

Observed symptom:

- the bridge fetched many days in one go
- the UI stayed in `fetching`
- an earlier day such as `2026-03-23` never got its own `/api/v1/ticks` request even though `trading_days` already covered it

Current fix:

- walk trading days from latest to earliest
- choose the latest day whose overlap with the requested range is non-empty
- do not fall back to the whole multi-day range just because the latest candidate overlap is empty

### 10. Non-aligned bucket starts matter for A-share timeframes

A-shares do not align every timeframe bucket to epoch-rounded minute boundaries.

Example:

- `30m` is naturally fine with `09:30 / 10:00 / 10:30`
- `1H` morning buckets are `09:30 / 10:30`, not `09:00 / 10:00`

Bug that existed:

- trade insertion into existing buckets used raw `trade.time / interval`
- `09:45` on a `1H` chart mapped to `09:00`
- the kline bucket existed at `09:30`
- result: kline visible, footprint empty

Current fix:

- when klines already exist, insert trades by matching into the existing bucket range
- do not reconstruct bucket start from epoch-rounded division

### 11. Custom minute timeframes are supported only on the synthetic QMT path

For QMT, minute bars are synthetic-from-tick, so custom minute bars are valid as long as session bucketing is respected.

Current behavior:

- the UI now accepts custom minute inputs for QMT kline/footprint charts
- session boundaries still force bar closes at lunch and market close
- this is not a native QMT bar API feature; it is a synthetic aggregation feature in Rust

### 12. QMT heatmap should not expose sub-3s timeframes

In this environment, QMT updates effectively arrive on a `3s` rhythm.

Practical implication:

- heatmap timeframes below `3s` look misleading
- they create the impression of higher temporal precision than the data source actually has

Current rule:

- for `SSH/SSZ`, heatmap timeframe options below `3s` are hidden
- `3s` is the minimum meaningful QMT heatmap basis

### 13. Bridge historical tick reads should stay one-shot by default

We briefly experimented with a chunked bridge strategy:

- split one day into `30m` chunks
- `download_history_data2(...)` + `get_market_data_ex_ori(...)` per chunk
- optionally repair suspicious gaps

Measured result on this setup:

- normal one-shot reads were usually faster
- chunking made the code harder to reason about
- fallback chunking created misleading timing noise and could waste time on future intraday windows

Current rule:

- historical bridge reads are back to a single one-shot path
- one request:
  - `download_history_data2(...)`
  - then `get_market_data_ex_ori(...)`
  - then retry local-cache reads if needed
- no default chunk/repair pipeline remains in the bridge

This keeps the bridge simple and makes timing logs easier to interpret.

### 14. Current-day history readiness now gates live synthetic output

For the current trading day, historical tick snapshot and live websocket tick stream overlap.

Current rule:

- cache live ticks immediately as they arrive
- do not emit synthetic live `Trade` yet
- do not emit synthetic live `Kline` yet
- only start those synthetic live outputs after current-day history has been merged into the mutable current-day cache

Reason:

- otherwise the same cumulative QMT tick counters can be observed once from history and again from live before the merge boundary is stable
- that creates double-counting risk and brief "latest bar jumped, then corrected" behavior

Practical effect:

- raw depth can appear immediately
- synthetic live trades / footprint updates / live kline updates intentionally wait for `current_day_history_ready`

### 15. Current-day history now uses a wider same-day fetch window

Completed historical trading days still use normal trading-session bounds.

Current day is different.

Current rule:

- completed day tick fetch: use trading-session bounds
- current day tick fetch: use a wider `00:00:00 ~ 16:00:00` China-time window

Why this exists:

- the current-day path needs a stable same-day baseline near the open
- it also needs tolerance for slightly late final close ticks
- the wider same-day window makes history/live merge behavior less fragile than a strict `09:30 ~ 15:00` read

### 16. QMT live kline stream now exists, but it is still synthetic-from-tick

There is still no separate native QMT kline websocket in this integration.

Current live rule:

- subscribe to `/ws/tick`
- merge that tick into current-day cache
- rebuild the current synthetic bar from the tick snapshot
- emit `KlineReceived`

Important limitation:

- one live ticker at a time is still the supported QMT mode for synthetic trade stream and synthetic kline stream
- this is a source-model limitation of the current bridge/client design, not a generic Flowsurface rule

### 17. Initial QMT history fetch now starts as part of stream resolution

Current dashboard behavior:

- pane resolves runtime streams
- stream resolution immediately schedules the initial history fetch task
- QMT footprint panes therefore start their combined `KlinesAndTrades` request right after stream readiness, instead of waiting for a later manual trigger

Practical effect:

- pane-open behavior is more deterministic
- the first visible history batch is less likely to be delayed by unrelated UI timing
 
### 18. Bridge defaults and endpoints are now important enough to document explicitly

Current defaults in this repo:

- Rust adapter default bridge base: `http://127.0.0.1:8765`
- override via environment variable: `QMT_BRIDGE_BASE`
- Python bridge default history timeout: `1200s`
- Python bridge default history read retries: `20`
- Python bridge default history read interval: `0.5s`
- Python bridge default search limit: `40`
- Python bridge max search limit: `200`

Current bridge routes:

- `GET /api/v1/ticks`
- `GET /api/v1/trading_days`
- `GET /api/v1/search`
- `GET /ws/tick`

This matters operationally because moving the bridge to another host/port now requires updating `QMT_BRIDGE_BASE`, not just restarting Python.

## API Pitfalls

### 1. `download_history_data2` is not a data-read API

`download_history_data2(...)` only triggers download into local cache.

In practice:

- It may return `{}`.
- It may return a small summary/status object.
- It does not return tick rows.

Use it as:

1. `download_history_data2(...)`
2. `get_market_data_ex_ori(...)` or another read API

### 2. `get_market_data_ex_ori("tick")` and `get_market_data_ex("5m")` do not behave the same

Historical tick:

- `get_market_data_ex_ori(..., period="tick")` returned `dict[str, numpy structured ndarray]`
- This path was stable and inspectable

Historical minute bars:

- `get_market_data_ex(..., period="5m" / "30m")` returned `dict[str, pandas.DataFrame]`
- This path worked only after `download_history_data(...)`

In this environment, trying to read minute bars after only `download_history_data2(...)` often produced empty results.

### 3. Multi-day native minute-bar reads are inconsistent

For native `30m`, multi-day reads were not always reliable when requested as one big range.

The stable workaround was:

- loop by trading day
- `download_history_data(symbol, "30m", day_start, day_end, ...)`
- `get_market_data_ex(..., period="30m", start_time=day_start, end_time=day_end, ...)`
- merge in Python/Rust

### 4. QMT trading-calendar APIs exist in docs, but may not work in the client

The vendored docs and wrapper both include:

- `get_holidays()`
- `get_trading_calendar(market, start_time, end_time)`
- `download_holiday_data()`

However, in this environment the client returned:

- `function not realize`

for the trading-calendar path.

Observed behavior on this machine:

- `get_trading_calendar(...)` failed
- `download_holiday_data(...)` followed by `get_trading_calendar(...)` still failed
- `get_holidays()` returned no useful holiday data

Implication:

- do not assume QMT holiday/calendar features are available just because the wrapper exposes them
- test the actual client build

### 5. `exchange_calendars` is the practical holiday fallback

The bridge now uses this fallback order:

1. QMT `get_trading_calendar(...)`
2. `download_holiday_data(...)` then `get_trading_calendar(...)`
3. Python `exchange_calendars` with `XSHG`
4. only as a last resort: weekday-based fallback

Reason:

- weekday-based fallback misclassifies working-day holidays such as Qingming makeup schedules / exchange closures
- `exchange_calendars` correctly handled the verified 2025 holiday windows in this environment

### 6. Runtime support can lag behind vendored docs

The local vendored docs and Python wrapper both expose:

- `get_holidays()`
- `get_trading_calendar(...)`
- `download_holiday_data()`

That does not guarantee the installed QMT client backend actually supports them.

On this machine, the client returned:

- `function not realize`

So the safe rule is:

- trust runtime behavior over wrapper surface area
- verify the real client build before relying on a documented API

### 7. The bridge websocket path is intentionally single-live-symbol today

The current Python bridge keeps one active live symbol for `/ws/tick`.

Observed/current behavior:

- if a websocket client subscribes to symbol `A`, the live worker attaches to `A`
- if a later websocket client subscribes to symbol `B`, the bridge switches the live worker to `B`
- existing clients for `A` are closed

Implication:

- this is acceptable for the current QMT live trade / live kline design, which already assumes one live ticker at a time
- it is not yet a transparent multi-symbol websocket hub like a public exchange stream multiplexer

Important distinction:

- search and history HTTP routes do not share this live-symbol limitation
- the limitation is specifically on the live websocket side

## Historical Tick Semantics

### 1. `pvolume` was not usable

On `600309.SH` historical tick data for `2026-04-07`, `pvolume` was effectively `0`.

That means:

- do not build quantity from `pvolume`
- do not treat missing `pvolume` as an error

### 2. `volume` is the usable cumulative field

Historical tick `volume` increased normally and could be differenced across adjacent ticks.

But the unit is not raw shares. In practice it behaves like lots/hands:

- `amount_delta / volume_delta` gave prices around `8200` instead of `82`
- multiplying `volume_delta` by `100` fixed the unit mismatch

Current rule:

- `qty_raw = (current_volume - previous_volume) * 100`

This should be read as:

- historical `tick.volume` is usable
- its unit behaves like hands/lots
- synthetic share quantity must multiply by `100`

### 3. `stockStatus` is unreliable in historical tick

Historical `tick.stockStatus` often came back as `0` for every row.

That means:

- it cannot be trusted as the only filter for continuous trading
- time-window filtering is still needed as fallback

### 4. `tick.high` and `tick.low` are session-cumulative

Historical tick rows repeatedly carried the full-day running high/low.

Example effect:

- at `10:30`, `tick.low` could still be `81.80` from the open
- that value does not mean the current 30-minute bucket traded down there

So:

- do not blindly use raw tick `high/low` as per-trade prices
- only use them as cumulative extrema signals

## Current Synthetic Trade Rules

These are the rules that produced the best alignment so far.

### Quantity

- use `delta(volume) * 100`
- allow reset only across trading-day boundaries
- if counters move backward inside the same day, drop that pair

### Price

- use `current_tick.lastPrice`
- do not derive price from `amount_delta / volume_delta`
- do not fall back to `previous_tick.lastPrice`

Reason:

- `amount_delta / volume_delta` produced pathological spikes on some adjacent tick pairs
- `lastPrice` matched QMT native minute bars much better

### Direction

Synthetic `is_sell` is still heuristic:

- compare against `bid1/ask1`
- then midpoint
- then `lastPrice` movement
- then L1 volume drop heuristics

This is only for footprint side classification. It is not true exchange-side aggressor data.

## Current Synthetic Kline Rules

Synthetic bars are built from synthetic trades, then supplemented with tick-derived information.

### OHLC

- `open/high/low/close` come from synthetic trades priced at `lastPrice`
- bar `high/low` are also supplemented by cumulative tick high/low changes inside the bucket
- when fetching historical tick for synthetic klines, fetch a seed range before the requested start
- for the opening bucket of a trading day, use tick-provided `open` to correct the bar open/high/low
- for the closing bucket, map `15:00:01 ~ 15:00:05` into the final `14:30~15:00` bucket

### Volume

- volume is synthetic and based on `delta(volume) * 100`
- closing-bucket volume must include the small post-`15:00:00` final tick jump when QMT reports it a few seconds late

### Important limitation

The first bar of a day is only trustworthy if a usable baseline tick was fetched before the requested opening bucket.

Typical symptom when the seed range is missing or insufficient:

- first bar `open` or `low` is slightly too high
- first bar volume is slightly too low

## Trading Calendar and Holiday Pitfalls

### 1. Do not validate future-year exchange holidays by default

If the exchange has not yet fully published the next year's schedule, a future-year validation is weak evidence.

Better practice:

- validate calendar behavior on a completed prior year first
- for example, use `2025` holiday windows before trusting `2026`

Reason:

- current-year or next-year exchange notices may not be fully published yet
- incomplete future holiday schedules produce false confidence

### 2. Qingming / Labor Day / National Day checks on this machine

After adding `exchange_calendars` fallback, the validation script showed:

- Qingming 2025:
  - trading days: `20250403, 20250407, 20250408`
  - `20250404` correctly excluded
- Labor Day 2025:
  - trading days: `20250430, 20250506, 20250507, 20250508`
  - `20250501`, `20250502`, `20250505` correctly excluded
- National Day 2025:
  - trading days: `20250929, 20250930, 20251009, 20251010`
  - `20251001` to `20251008` gap handled correctly for exchange closures

This confirms:

- the QMT client calendar path was broken
- the `exchange_calendars:XSHG` fallback fixed the verified 2025 holiday windows

### 3. Non-trading days should be rejected before historical tick download

Bridge-side rule:

- if the requested day is not a trading day, return empty tick history immediately
- do not call `download_history_data2(...)` for that day

Reason:

- otherwise holiday gaps such as Qingming can trigger useless day downloads
- those useless day downloads can also time out and poison the UI with repeated 500s

### 4. `2026-04-06` was the concrete failure case

Observed bad behavior before the fix:

- the system treated `2026-04-06` as a trading day
- it issued `/api/v1/ticks?symbol=600309.SH&start=2026-04-06 09:30:00&end=2026-04-06 15:00:00`
- the bridge then timed out waiting for the historical worker

Interpretation:

- the old fallback was not correctly excluding Qingming closure
- the visible weekend/holiday gap problem was not a chart-axis bug by itself
- it was a calendar-resolution bug upstream

## Validation Results

### 1. `600309.SH`, `2026-04-07`, morning session, `5m`

Comparison:

- synthetic `5m` from historical tick
- native QMT `5m`

After switching synthetic trade price to `lastPrice` and adding opening-bucket correction:

- all 24 bars aligned
- `high` MAE: `0.0`
- `close` MAE: `0.0`
- `open` MAE: `0.0`
- `low` MAE: `0.0`

This confirmed that:

- `lastPrice` is the right synthetic trade price for this environment
- opening-bucket seeding/correction removed the first-bar price mismatch

### 2. `600309.SH`, completed-day afternoon checks, `30m`

Comparison:

- synthetic `30m` from historical tick
- native QMT `30m`
- checked on completed trading days only

Two important isolated checks:

- `2026-03-31 15:00`
  - synthetic close: `79.45`
  - native close: `79.45`
  - synthetic volume: `2,007,100` shares
  - native volume: `20,071` hands
  - after `x100`, volume matched exactly
- `2026-04-02 15:00`
  - synthetic close: `82.71`
  - native close: `82.71`
  - synthetic volume: `2,462,200` shares
  - native volume: `24,622` hands
  - after `x100`, volume matched exactly

This confirmed that the earlier closing-bar mismatch was not a QMT data truncation problem. It was a bucketing problem: the last tick existed, but arrived slightly after `15:00:00` and needed to be mapped back into the final bucket.

### 3. Validation pitfall: do not use incomplete days for close checks

`2026-04-07` was still intraday during earlier comparisons.

That means:

- do not use that date to judge tail/close alignment
- only use completed sessions or completed trading days when validating close handling
- if a multi-day compare script produces a strange edge mismatch, re-check the same day in isolation before trusting the result

### 4. Volume mismatch interpretation

After the opening seed fix and closing grace fix:

- middle buckets aligned well
- opening buckets aligned when a seed tick was available
- closing-bucket volume aligned exactly in isolated completed-day checks

Current judgment:

- OHLC correctness is now in good shape
- closing-bucket volume is also in good shape after grace-window handling
- if a remaining mismatch appears, first suspect missing opening baseline or a bad validation window before suspecting QMT volume itself

### 5. Historical holiday validation using `exchange_calendars`

Validation environment:

- Python: `D:\miniconda3\python.exe`
- calendar library: `exchange_calendars`
- exchange calendar used: `XSHG`

Result:

- `exchange_calendars` imported successfully
- verified holiday windows for `2025` produced exchange-like trading-day lists
- this is currently the most reliable holiday source available in this setup

## Timeout and Stability Pitfalls

### 1. Historical tick timeout is a bridge worker timeout

When `/api/v1/ticks` fails with:

- `TimeoutError: worker timed out after ... seconds`

the failure is currently:

- worker process launched
- worker calls `download_history_data2(...)`
- parent waits on multiprocessing queue
- parent times out after `history_timeout`

So this error means:

- the historical tick worker did not finish in time
- not that JSON parsing failed
- not that Rust deserialization failed

Operational note:

- the default bridge timeout was later raised from the earlier `180s` setting to a larger value
- but the meaning of the error is unchanged: it is still a worker-side history timeout, not a parse failure

### 2. Successful day fetches and timed-out day fetches can coexist

Observed example for `600309.SH`:

- `2026-04-07 09:30:00~15:00:00` returned `200` with thousands of rows
- nearby day requests such as `2026-04-02` and `2026-04-03` timed out

Implication:

- one successful day does not prove the whole visible window is healthy
- a mixed pattern of `200` and timeout is possible
- cache and cooldown are necessary to keep the UI responsive

### 3. "Still fetching" does not always mean the bridge is still working on the visible day

One misleading failure mode was:

- bridge had already returned some visible-day `/api/v1/ticks` results
- but the pane still showed `fetching`
- meanwhile a long historical task was busy on later follow-up days or a widened fallback range

This means:

- "still fetching" can be a scheduling problem, not just a slow QMT response
- always compare the visible gap with the actual `/api/v1/ticks` day ranges in the bridge log
- if the expected day never appears as its own `/api/v1/ticks` range, the bug is upstream in request scheduling

### 4. Measured slowness was in Python/QMT fetch, not Rust synthetic derivation

After adding explicit timing logs on both sides, the pattern was clear.

Observed examples:

- one current-day batch:
  - `fetch_elapsed ≈ 23.1s`
  - `derive_elapsed ≈ 2ms`
- one historical-day batch:
  - `fetch_elapsed ≈ 7.6s`
  - `derive_elapsed ≈ 5ms`

Interpretation:

- `fetch_elapsed` is the HTTP wait on `/api/v1/ticks`
- that time is dominated by Python bridge work and underlying QMT history retrieval
- Rust synthetic steps:
  - `tick -> synthetic trade`
  - `trade -> kline`
  are effectively negligible by comparison

So when the user perceives "K-line aggregation is slow", the first suspect should be Python/QMT history read latency, not Rust bar derivation.

### 5. Bridge worker timeout covers the whole historical worker, not just one QMT call

The bridge-side `history_timeout` applies to the entire worker process lifetime for one `/api/v1/ticks` request.

That includes:

- `load_xtquant(...)`
- `connect_quote_client(...)`
- trading-day resolution
- `download_history_data2(...)`
- `get_market_data_ex_ori(...)` polling
- final serialization/filtering

So a timeout such as:

- `worker timed out after 600.0 seconds`

means:

- the whole historical worker did not finish in time
- not merely that one socket read or one JSON parse timed out

Later this default was increased again to `1200s` in the bridge, but the meaning of the timeout stayed the same.

### 6. The bridge access-log `200` is not "early", but it is still not a UI-complete signal

In Python `BaseHTTPRequestHandler`, the standard access log is emitted when `send_response(...)` is called.

In this bridge, that happens only after:

- `fetch_ticks(...)` has already returned a result to `_handle_fetch_ticks(...)`

So:

- `GET /api/v1/ticks ... 200`
  means the worker has finished and the bridge is now sending the HTTP response

But it still does **not** mean:

- Rust has finished synthetic derivation
- the dashboard has inserted the batch
- the pane has left `fetching`

For that, the later Rust-side markers are still required:

- `QMT derived history ...`
- `QMT batched fetch ... batch ...`
- `Dashboard received QMT KlinesAndTrades ...`

## Cache Semantics

### 1. "Recent" means recently accessed, not close to today

The completed-day tick cache is effectively per-symbol LRU.

That means:

- if `2025-04-03` was accessed recently, it can stay cached
- if yesterday was not accessed recently, it can be evicted

So:

- cache retention is based on recent use
- not on distance from the current date

### 2. Current day must not be treated like a closed immutable day

If the day is still trading:

- historical fetch is only a partial snapshot
- websocket ticks keep extending and correcting that day

Therefore current day is handled separately:

- no immutable day-cache assumption
- merge history snapshot with live ticks
- keep only one record per `timestamp`
- later record overrides earlier record

This avoids:

- summing cumulative fields
- freezing an unfinished trading day as if it were complete

## Testing Discipline

### 1. Always separate completed-day validation from intraday validation

Use completed days for:

- close handling
- final bucket checks
- volume alignment on the last bar

Use intraday days only for:

- currently visible history behavior
- morning-session alignment
- live/history merge behavior

Do not use an intraday date to conclude that closing logic is right or wrong.

### 2. Report tests in a fixed format

For each validation, record:

- environment
- command
- result
- key output
- conclusion

This keeps later debugging comparable and prevents vague "seems fixed" conclusions.

## Flowsurface-Specific Pitfalls

### 1. QMT synthetic kline support must include all required minute timeframes

Because these klines are built from tick, any supported minute timeframe should work.

A bug existed where `3m` was not mapped in QMT timeframe conversion and produced:

- `unsupported QMT timeframe for synthetic klines: 3m`

This was fixed by adding `M3`.

### 1a. Full datetime display should use a stable numeric format

The older chart UI mixed in locale-like labels such as:

- `Apr`
- `Tue`

For debugging and cross-checking with bridge logs, this was a poor fit.

Current rule:

- full date display should prefer numeric forms like `2026-01-10 13:30`
- compact axis labels can still stay compact
- but any detailed timestamp display should be log-friendly and unambiguous

### 2. Stock footprint tick multiplier should not use the crypto default

For A-shares:

- `min_tick` is often `0.01`
- a large default multiplier like `50` makes footprint bars visually collapse

A stock-friendly default is `1`.

### 3. Footprint wick should reflect the real bar `high/low`

At one point the footprint wick was being clamped to visible footprint cluster prices.

That was misleading because:

- cluster prices are synthetic and incomplete
- bar `high/low` is a separate concept

Current rule:

- footprint wick should use the actual kline `high/low`

### 4. Heatmap history is replayed from current-day tick snapshots, not native historical depth

The heatmap no longer starts as pure live-only accumulation after pane open.

Current behavior:

- on QMT heatmap open, fetch current-day historical ticks once
- derive synthetic trades from those ticks
- derive L1 depth snapshots from those ticks when valid `bid/ask` is present
- replay that history first, then continue with live websocket updates

Important limit:

- this is still not native historical order-book backfill
- it is only current-day replay reconstructed from tick snapshots
- multi-day / true historical depth remains unavailable

### 4a. Heatmap with QMT should be treated as low-frequency live accumulation

Because the source cadence is effectively `3s`:

- heatmap is best understood as a live accumulation view
- not as a fine-grained historical replay tool

This matters when comparing it to crypto venues where sub-second accumulation is meaningful.

### 4b. QMT heatmap lunch-gap compression must use an axis-only `3s` mapper

Comparing against Binance heatmap was useful here.

Binance heatmap behavior:

- subscribe to live `Depth + Trades`
- round both streams into the selected heatmap basis
- use an ordinary linear time axis

QMT heatmap is conceptually the same kind of view:

- live depth accumulation
- live trade accumulation
- current meaningful bases are `3s / 4s / 6s`
- current-day replay comes from tick-derived snapshots, not native depth history

The trap is that QMT also has a session-aware gapless time-axis helper for trading-day charts.

Important detail:

- the generic QMT gapless helpers depend on timeframe support from `qmt_timeframe_ms(...)`
- synthetic kline helpers should still reject millisecond heatmap bases such as `MS3000 / MS4000 / MS6000`

Failure mode when this is wired incorrectly:

- enabling heatmap-only millisecond bases in the wrong helper path can accidentally widen kline/session code that was never meant for heatmap
- faking midday buckets would hide the lunch break by inventing data instead of only compressing coordinates

Current rule:

- keep `qmt_timeframe_ms(...)` unchanged for synthetic-kline logic
- add a dedicated heatmap-axis mapper only for `time_axis_bucket_offset(...) / time_axis_bucket_at_offset(...)`
- compress the lunch gap on the X axis only
- never fabricate midday depth or trade data just to make the chart look continuous

### 4c. Heatmap history sync must not start before the chart is initialized

A sequencing bug existed around pane initialization.

Failure mode:

- the pane still held `Content::Heatmap { chart: None, .. }`
- `ResolveStreams` fired before `ResolveContent`
- heatmap history sync started against the placeholder pane
- the app panicked

Current rule:

- only start heatmap history sync after the real heatmap chart exists
- pane-side sync helpers should fail soft if the chart is still missing

### 4d. Heatmap history replay must ignore stale requests after pane retarget

Another easy mistake is to trust `pane_id` alone.

Failure mode:

- pane starts a QMT heatmap history fetch for ticker A
- before it returns, the pane switches to ticker B
- old ticker A history or fetch failure arrives late
- the late result corrupts ticker B's pane state or cancels its current sync

Current rule:

- apply heatmap history/failure only if the pane still matches the original stream
- stale results should be dropped silently instead of touching the current pane

### 4e. Heatmap basis or tick-size changes must reload current-day history

Changing heatmap basis or tick size clears local replayed state.

Failure mode:

- user changes basis or tick size
- chart clears trades / depth / replay buffers
- subscriptions refresh, but no new history replay is triggered
- pane stays on `Waiting for data...`

Current rule:

- basis / tick-size changes must trigger a fresh current-day heatmap history sync
- `pause_buffer` must also be cleared during those resets so old depth does not leak into the new configuration

### 4f. QMT heatmap history must drop off-session tail ticks

Another QMT-specific trap is that raw `/api/v1/ticks` history can contain dirty tail ticks outside the real session.

Failure mode:

- some symbols return extra ticks after the close, for example around `15:05` or `15:30`
- those rows may still carry `volume` and top-of-book arrays
- the usual `volume > 0 && has_top_of_book` replay filter therefore lets them through
- heatmap `latest_x` gets pushed into off-session time that the QMT gapless axis does not represent
- the heatmap list tooltip can still query data, but the main chart and time axis look broken or missing

Current rule:

- QMT heatmap history replay must only keep ticks that map onto the QMT heatmap session axis
- filter replay ticks by session membership in addition to `volume > 0` and top-of-book checks
- treat ticks such as `15:05` / `15:30` tails as dirty bridge data, not valid replay input

### 4g. Heatmap history replay should behave like zero-delay WS playback

Batch-applying all historical trades first and all historical depth later is not equivalent to the live path.

Failure mode:

- history data reaches the chart, but is replayed through a shortcut path that does not match live update ordering
- anchors such as `latest_x`, `base_price_y`, and `last_price` can end up different from the state produced by real WS updates
- the pane looks inconsistent until later live ticks move the chart again

Current rule:

- merge historical trades and depth snapshots by timestamp
- replay them through the same insert/update path used by live WS events
- history backfill should act like zero-delay WS playback, not a separate chart mutation shortcut

### 5. Footprint history should not be blocked by a separate trade-fetch path

Before the combined QMT history fetch:

- kline history could appear
- footprint could remain empty
- or kline backfill could starve trade backfill

Current fix:

- QMT footprint panes now request a combined historical result derived from one tick fetch
- this keeps kline and footprint history in sync

### 6. Live QMT trades must update the current time bucket immediately

A separate live-update bug existed even after historical footprint loading was mostly correct.

Observed symptom:

- the latest K and latest footprint did not move in real time
- data only appeared after a manual refresh or basis reset
- this was most visible on the current day

Root cause:

- time-based charts were inserting live trades only into existing footprint buckets
- they were not updating the current kline's `high/low/close/volume`
- when a new minute/hour bucket began, the live path could fail to create that new bucket at all

Practical effect:

- the chart looked "stuck" even though live QMT ticks/trades were still arriving
- historical backfill looked fine, but the newest bar lagged behind until refresh

Current fix:

- live trades now update the current kline bucket's OHLC and volume
- live trades can create a new current bucket when the timestamp moves into a new timeframe bucket
- for QMT time-based charts, the live bucket start is computed with the same session-aware bucket logic used by synthetic historical klines

This is especially important for:

- `1H` and other session-aware timeframes
- current-day footprint charts
- cases where the latest bar must keep growing without any manual refresh

### 6a. Live QMT kline updates are rebuilt from tick snapshot, not pushed natively

Current design:

- there is no separate native QMT kline push channel in this integration
- live kline updates are reconstructed from current-day tick snapshot
- this reconstruction intentionally shares the same session-aware bucket logic as synthetic historical klines

Why this matters:

- if live kline behavior looks wrong, the first suspect is usually current-day tick cache / merge / bucket mapping
- the first suspect is usually **not** a missing native kline subscription, because no such dedicated push source exists in this design

## Practical Recommendations

- For history tick work, use `get_market_data_ex_ori(..., period="tick")`
- For native minute-bar checks, use `download_history_data(...) + get_market_data_ex(...)`
- Use `lastPrice` for synthetic trade price
- Use `volume * 100` logic for synthetic trade quantity
- Pull a seed range before the requested start when validating or exporting synthetic bars
- Treat `15:00:01 ~ 15:00:05` as part of the final closing bucket
- Do not judge close alignment on an intraday date
- Validate new synthetic-bar logic against native QMT bars with one concrete symbol and one concrete date range before trusting it generally
- For holiday handling in this environment, prefer `exchange_calendars` over QMT calendar APIs
- Keep current-day tick cache separate from completed-day cache
- Never merge history/live cumulative ticks by summing fields; merge by `timestamp` override only
- If a historical day timed out, let the short cooldown absorb repeated UI retries instead of hammering QMT immediately again
- When debugging QMT heatmap, compare its data path to Binance heatmap first; both are `Depth + Trades` accumulation views
- For QMT heatmap history replay, drop `volume=0` and empty-top-of-book ticks before deriving depth/trades
- Do not reuse generic synthetic-kline timeframe helpers just to make heatmap `3s` axis gapless
- When basis or tick size changes on a QMT heatmap, replay current-day history again instead of waiting for live data to refill the pane
- If the bridge moves off `127.0.0.1:8765`, update `QMT_BRIDGE_BASE`
- Expect only one active live QMT symbol on the websocket path today

## Known Open Issues

- opening bucket still depends on having a usable pre-range seed tick
- synthetic side classification is heuristic only
- no true Level 2 transaction stream is available here, so synthetic trades are still approximations
- holiday handling still depends on `exchange_calendars` being installed if QMT calendar APIs are unavailable
- heatmap still has no true multi-day or native historical depth backfill
- live websocket path is still effectively one active symbol at a time
- if current-day history never reaches the merged-ready state, synthetic live trades / live kline updates can remain intentionally silent even while raw depth still updates
