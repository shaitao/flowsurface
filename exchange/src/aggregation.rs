use crate::{Kline, Trade, Volume};

/// Minimal market-data input that can seed a bar aggregation step.
pub trait AggregationInput {
    fn as_kline_seed(&self) -> Kline;
}

/// Mutable bar builder used by client-side aggregation.
///
/// Today it owns the canonical OHLCV bar state.
/// In the future this is the natural place to attach extra aggregation
/// metadata such as trade count, amount/notional, or rule progress.
#[derive(Debug, Clone, Copy)]
pub struct BarAccumulator {
    bar: Kline,
}

impl BarAccumulator {
    pub fn from_seed(bar: Kline) -> Self {
        Self { bar }
    }

    pub fn from_input<I>(input: &I) -> Self
    where
        I: AggregationInput,
    {
        Self::from_seed(input.as_kline_seed())
    }

    pub fn from_bucketed_input<I>(input: &I, bucket_time: u64) -> Self
    where
        I: AggregationInput,
    {
        let mut seed = input.as_kline_seed();
        seed.time = bucket_time;
        Self::from_seed(seed)
    }

    pub fn push<I>(&mut self, input: &I)
    where
        I: AggregationInput,
    {
        self.push_seed(input.as_kline_seed());
    }

    pub fn push_seed(&mut self, seed: Kline) {
        merge_kline(&mut self.bar, &seed);
    }

    pub fn snapshot(&self) -> Kline {
        self.bar
    }

    pub fn finish(self) -> Kline {
        self.bar
    }
}

impl AggregationInput for Kline {
    fn as_kline_seed(&self) -> Kline {
        *self
    }
}

impl AggregationInput for Trade {
    fn as_kline_seed(&self) -> Kline {
        Kline {
            time: self.time,
            open: self.price,
            high: self.price,
            low: self.price,
            close: self.price,
            volume: Volume::empty_buy_sell().add_trade_qty(self.is_sell, self.qty),
        }
    }
}

/// Aggregates sorted market data into arbitrary client-side time buckets.
///
/// The input slice is expected to be sorted by ascending timestamp.
pub fn aggregate_by_interval_ms<I>(source: &[I], interval_ms: u64) -> Vec<Kline>
where
    I: AggregationInput,
{
    if source.is_empty() || interval_ms == 0 {
        return vec![];
    }

    let mut output = Vec::new();
    let mut current: Option<BarAccumulator> = None;
    let mut current_bucket = 0;

    for item in source {
        let bucket = align_time_to_bucket(item.as_kline_seed().time, interval_ms);

        match current.as_mut() {
            Some(bar) if current_bucket == bucket => {
                bar.push_seed(bucketed_seed(item, bucket));
            }
            Some(_) => {
                output.push(current.take().expect("current bar must exist").finish());
                current = Some(BarAccumulator::from_bucketed_input(item, bucket));
                current_bucket = bucket;
            }
            None => {
                current = Some(BarAccumulator::from_bucketed_input(item, bucket));
                current_bucket = bucket;
            }
        }
    }

    if let Some(bar) = current {
        output.push(bar.finish());
    }

    output
}

/// Aggregates sorted market data into a stream of incremental time-bucket snapshots.
///
/// Each input item produces one output snapshot representing the current state of its bucket.
pub fn aggregate_stream_by_interval_ms<I>(source: &[I], interval_ms: u64) -> Vec<Kline>
where
    I: AggregationInput,
{
    if source.is_empty() || interval_ms == 0 {
        return vec![];
    }

    let mut output = Vec::with_capacity(source.len());
    let mut current: Option<BarAccumulator> = None;
    let mut current_bucket = 0;

    for item in source {
        let bucket = align_time_to_bucket(item.as_kline_seed().time, interval_ms);

        match current.as_mut() {
            Some(bar) if current_bucket == bucket => {
                bar.push_seed(bucketed_seed(item, bucket));
                output.push(bar.snapshot());
            }
            Some(_) | None => {
                current = Some(BarAccumulator::from_bucketed_input(item, bucket));
                current_bucket = bucket;
                output.push(current.expect("current bar must exist").snapshot());
            }
        }
    }

    output
}

/// Convenience wrapper for kline-to-kline re-sampling.
pub fn aggregate_klines_by_interval_ms(source: &[Kline], interval_ms: u64) -> Vec<Kline> {
    aggregate_by_interval_ms(source, interval_ms)
}

/// Defines how much progress a trade contributes toward a threshold bar.
pub trait TradeMetric {
    fn measure(&self, trade: &Trade) -> f64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TickCountMetric;

impl TradeMetric for TickCountMetric {
    fn measure(&self, _trade: &Trade) -> f64 {
        1.0
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeMetric;

impl TradeMetric for VolumeMetric {
    fn measure(&self, trade: &Trade) -> f64 {
        trade.qty.to_f32_lossy() as f64
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NotionalMetric;

impl TradeMetric for NotionalMetric {
    fn measure(&self, trade: &Trade) -> f64 {
        f64::from(trade.price.to_f32_lossy()) * f64::from(trade.qty.to_f32_lossy())
    }
}

/// Generic threshold bar builder for trade-based aggregations.
///
/// The metric decides how much each input trade contributes toward the threshold.
/// Once the running total meets or exceeds `threshold`, the current bar is closed.
///
/// Overshoot is intentionally kept in the closing bar instead of being split into the next one.
pub struct ThresholdAggregator<M> {
    metric: M,
    threshold: f64,
}

impl<M> ThresholdAggregator<M>
where
    M: TradeMetric,
{
    pub fn new(metric: M, threshold: f64) -> Self {
        assert!(threshold.is_finite() && threshold > 0.0);

        Self { metric, threshold }
    }

    pub fn aggregate(&self, source: &[Trade]) -> Vec<Kline> {
        if source.is_empty() {
            return vec![];
        }

        let mut output = Vec::new();
        let mut current: Option<BarAccumulator> = None;
        let mut progress = 0.0_f64;

        for trade in source {
            match current.as_mut() {
                Some(bar) => bar.push(trade),
                None => current = Some(BarAccumulator::from_input(trade)),
            }

            progress += self.metric.measure(trade).max(0.0);

            if progress + f64::EPSILON >= self.threshold {
                output.push(current.take().expect("current bar must exist").finish());
                progress = 0.0;
            }
        }

        if let Some(bar) = current {
            output.push(bar.finish());
        }

        output
    }
}

fn align_time_to_bucket(timestamp: u64, interval_ms: u64) -> u64 {
    (timestamp / interval_ms) * interval_ms
}

fn bucketed_seed<I>(input: &I, bucket_time: u64) -> Kline
where
    I: AggregationInput,
{
    let mut seed = input.as_kline_seed();
    seed.time = bucket_time;
    seed
}

fn merge_kline(target: &mut Kline, next: &Kline) {
    target.high = target.high.max(next.high);
    target.low = target.low.min(next.low);
    target.close = next.close;
    target.volume = merge_volume(target.volume, next.volume);
}

fn merge_volume(lhs: Volume, rhs: Volume) -> Volume {
    match (lhs, rhs) {
        (Volume::BuySell(lhs_buy, lhs_sell), Volume::BuySell(rhs_buy, rhs_sell)) => {
            Volume::BuySell(lhs_buy + rhs_buy, lhs_sell + rhs_sell)
        }
        _ => Volume::TotalOnly(lhs.total() + rhs.total()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NotionalMetric, ThresholdAggregator, TickCountMetric, VolumeMetric,
        aggregate_by_interval_ms, aggregate_klines_by_interval_ms, aggregate_stream_by_interval_ms,
    };
    use crate::unit::{Price, Qty};
    use crate::{Kline, Trade, Volume};

    fn test_kline(minute: u64, open: f32, high: f32, low: f32, close: f32, qty: f32) -> Kline {
        Kline {
            time: minute * 60_000,
            open: Price::from_f32(open),
            high: Price::from_f32(high),
            low: Price::from_f32(low),
            close: Price::from_f32(close),
            volume: Volume::TotalOnly(Qty::from_f32(qty)),
        }
    }

    fn test_trade(time: u64, price: f32, qty: f32, is_sell: bool) -> Trade {
        Trade {
            time,
            is_sell,
            price: Price::from_f32(price),
            qty: Qty::from_f32(qty),
        }
    }

    #[test]
    fn aggregates_klines_into_custom_minute_buckets() {
        let source = vec![
            test_kline(0, 100.0, 101.0, 99.0, 100.5, 1.0),
            test_kline(1, 100.5, 102.0, 100.0, 101.0, 2.0),
            test_kline(2, 101.0, 103.0, 100.5, 102.0, 3.0),
            test_kline(3, 102.0, 102.5, 101.0, 101.5, 4.0),
            test_kline(4, 101.5, 104.0, 101.0, 103.5, 5.0),
            test_kline(5, 103.5, 105.0, 103.0, 104.0, 6.0),
            test_kline(6, 104.0, 106.0, 103.5, 105.5, 7.0),
            test_kline(7, 105.5, 107.0, 105.0, 106.0, 8.0),
            test_kline(8, 106.0, 108.0, 105.5, 107.5, 9.0),
        ];

        let bars = aggregate_klines_by_interval_ms(&source, 7 * 60_000);

        assert_eq!(bars.len(), 2);

        let first = bars[0];
        assert_eq!(first.time, 0);
        assert_eq!(first.open, Price::from_f32(100.0));
        assert_eq!(first.high, Price::from_f32(106.0));
        assert_eq!(first.low, Price::from_f32(99.0));
        assert_eq!(first.close, Price::from_f32(105.5));
        assert_eq!(first.volume.total(), Qty::from_f32(28.0));

        let second = bars[1];
        assert_eq!(second.time, 7 * 60_000);
        assert_eq!(second.open, Price::from_f32(105.5));
        assert_eq!(second.high, Price::from_f32(108.0));
        assert_eq!(second.low, Price::from_f32(105.0));
        assert_eq!(second.close, Price::from_f32(107.5));
        assert_eq!(second.volume.total(), Qty::from_f32(17.0));
    }

    #[test]
    fn aggregates_trades_into_custom_time_buckets() {
        let trades = vec![
            test_trade(0, 100.0, 1.0, false),
            test_trade(60_000, 101.0, 2.0, true),
            test_trade(2 * 60_000, 99.0, 3.0, false),
            test_trade(8 * 60_000, 105.0, 4.0, false),
        ];

        let bars = aggregate_by_interval_ms(&trades, 7 * 60_000);

        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].time, 0);
        assert_eq!(bars[0].open, Price::from_f32(100.0));
        assert_eq!(bars[0].close, Price::from_f32(99.0));
        assert_eq!(bars[0].volume.total(), Qty::from_f32(6.0));
        assert_eq!(bars[1].time, 7 * 60_000);
        assert_eq!(bars[1].close, Price::from_f32(105.0));
    }

    #[test]
    fn aggregates_trades_into_streaming_time_bucket_snapshots() {
        let trades = vec![
            test_trade(0, 100.0, 1.0, false),
            test_trade(60_000, 101.0, 2.0, true),
            test_trade(2 * 60_000, 99.0, 3.0, false),
            test_trade(8 * 60_000, 105.0, 4.0, false),
        ];

        let bars = aggregate_stream_by_interval_ms(&trades, 7 * 60_000);

        assert_eq!(bars.len(), 4);
        assert_eq!(bars[0].time, 0);
        assert_eq!(bars[0].close, Price::from_f32(100.0));
        assert_eq!(bars[1].time, 0);
        assert_eq!(bars[1].close, Price::from_f32(101.0));
        assert_eq!(bars[2].time, 0);
        assert_eq!(bars[2].close, Price::from_f32(99.0));
        assert_eq!(bars[2].volume.total(), Qty::from_f32(6.0));
        assert_eq!(bars[3].time, 7 * 60_000);
        assert_eq!(bars[3].close, Price::from_f32(105.0));
    }

    #[test]
    fn aggregates_trades_by_tick_count_threshold() {
        let trades = vec![
            test_trade(1_000, 100.0, 1.0, false),
            test_trade(2_000, 101.0, 2.0, true),
            test_trade(3_000, 99.5, 1.5, false),
        ];

        let bars = ThresholdAggregator::new(TickCountMetric, 2.0).aggregate(&trades);

        assert_eq!(bars.len(), 2);

        let first = bars[0];
        assert_eq!(first.time, 1_000);
        assert_eq!(first.open, Price::from_f32(100.0));
        assert_eq!(first.high, Price::from_f32(101.0));
        assert_eq!(first.low, Price::from_f32(100.0));
        assert_eq!(first.close, Price::from_f32(101.0));
        assert_eq!(first.volume.buy_qty_or_zero(), Qty::from_f32(1.0));
        assert_eq!(first.volume.sell_qty_or_zero(), Qty::from_f32(2.0));

        let second = bars[1];
        assert_eq!(second.time, 3_000);
        assert_eq!(second.open, Price::from_f32(99.5));
        assert_eq!(second.close, Price::from_f32(99.5));
        assert_eq!(second.volume.total(), Qty::from_f32(1.5));
    }

    #[test]
    fn aggregates_trades_by_volume_threshold() {
        let trades = vec![
            test_trade(1_000, 100.0, 1.0, false),
            test_trade(2_000, 101.0, 2.5, true),
            test_trade(3_000, 102.0, 1.0, false),
        ];

        let bars = ThresholdAggregator::new(VolumeMetric, 3.0).aggregate(&trades);

        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].volume.total(), Qty::from_f32(3.5));
        assert_eq!(bars[0].close, Price::from_f32(101.0));
        assert_eq!(bars[1].volume.total(), Qty::from_f32(1.0));
    }

    #[test]
    fn aggregates_trades_by_notional_threshold() {
        let trades = vec![
            test_trade(1_000, 100.0, 2.0, false),
            test_trade(2_000, 110.0, 3.0, true),
            test_trade(3_000, 120.0, 1.0, false),
        ];

        let bars = ThresholdAggregator::new(NotionalMetric, 500.0).aggregate(&trades);

        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].open, Price::from_f32(100.0));
        assert_eq!(bars[0].close, Price::from_f32(110.0));
        assert_eq!(bars[0].volume.total(), Qty::from_f32(5.0));
        assert_eq!(bars[1].time, 3_000);
        assert_eq!(bars[1].volume.total(), Qty::from_f32(1.0));
    }
}
