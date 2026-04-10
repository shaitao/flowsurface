use super::*;

pub(super) fn china_trading_day(timestamp_ms: u64) -> Option<chrono::NaiveDate> {
    china_datetime(timestamp_ms).map(|dt| dt.date_naive())
}

pub(super) fn china_offset() -> Option<FixedOffset> {
    FixedOffset::east_opt(8 * 60 * 60)
}

pub(super) fn current_china_day() -> Option<NaiveDate> {
    Some(
        chrono::Utc::now()
            .with_timezone(&china_offset()?)
            .date_naive(),
    )
}

pub(super) fn china_datetime(timestamp_ms: u64) -> Option<chrono::DateTime<FixedOffset>> {
    let offset = china_offset()?;
    chrono::DateTime::from_timestamp_millis(timestamp_ms as i64).map(|dt| dt.with_timezone(&offset))
}

pub(super) fn qmt_timeframe_ms(timeframe: Timeframe) -> Option<u64> {
    match timeframe {
        Timeframe::MS100
        | Timeframe::MS200
        | Timeframe::MS300
        | Timeframe::MS500
        | Timeframe::MS1000
        | Timeframe::MS3000
        | Timeframe::MS4000
        | Timeframe::MS6000 => None,
        _ => Some(timeframe.to_milliseconds()),
    }
}

fn qmt_gapless_axis_timeframe_ms(timeframe: Timeframe) -> Option<u64> {
    match timeframe {
        Timeframe::MS3000 | Timeframe::MS4000 | Timeframe::MS6000 => {
            Some(timeframe.to_milliseconds())
        }
        _ => qmt_timeframe_ms(timeframe),
    }
}

pub(super) fn qmt_default_kline_range(timeframe: Timeframe) -> Option<(u64, u64)> {
    let interval_ms = qmt_timeframe_ms(timeframe)?;
    let end = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let start = end.saturating_sub(DEFAULT_QMT_INITIAL_KLINE_BARS * interval_ms);
    Some((start, end))
}

pub(super) fn qmt_session_bounds(venue: Venue, day: NaiveDate) -> Option<[(u64, u64); 2]> {
    if !is_qmt_trading_day(venue, day) {
        return None;
    }

    let offset = china_offset()?;
    let morning_start = offset
        .from_local_datetime(&day.and_hms_opt(9, 30, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let morning_end = offset
        .from_local_datetime(&day.and_hms_opt(11, 30, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let afternoon_start = offset
        .from_local_datetime(&day.and_hms_opt(13, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let afternoon_end = offset
        .from_local_datetime(&day.and_hms_opt(15, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    Some([
        (morning_start, morning_end),
        (afternoon_start, afternoon_end),
    ])
}

fn qmt_effective_trading_timestamp(
    venue: Venue,
    timestamp_ms: u64,
) -> Option<(NaiveDate, [(u64, u64); 2], u64)> {
    let dt = china_datetime(timestamp_ms)?;
    let day = dt.date_naive();
    if !is_qmt_trading_day(venue, day) {
        return None;
    }

    let sessions = qmt_session_bounds(venue, day)?;
    let final_session_end = sessions.last().map(|(_, session_end)| *session_end)?;
    let mut effective_ts = timestamp_ms;
    if final_session_end <= timestamp_ms
        && timestamp_ms <= final_session_end.saturating_add(QMT_CLOSE_BUCKET_GRACE_MS)
    {
        effective_ts = final_session_end.saturating_sub(1);
    }

    Some((day, sessions, effective_ts))
}

fn qmt_total_trading_ms(venue: Venue, day: NaiveDate) -> Option<u64> {
    let sessions = qmt_session_bounds(venue, day)?;
    Some(
        sessions
            .into_iter()
            .map(|(session_start, session_end)| session_end.saturating_sub(session_start))
            .sum(),
    )
}

fn qmt_trading_elapsed_ms(venue: Venue, timestamp_ms: u64) -> Option<(NaiveDate, u64)> {
    let (day, sessions, effective_ts) = qmt_effective_trading_timestamp(venue, timestamp_ms)?;
    let mut elapsed_ms = 0_u64;

    for (session_start, session_end) in sessions {
        if session_start <= effective_ts && effective_ts < session_end {
            return Some((day, elapsed_ms + (effective_ts - session_start)));
        }
        elapsed_ms = elapsed_ms.saturating_add(session_end.saturating_sub(session_start));
    }

    None
}

fn qmt_timestamp_at_trading_elapsed(venue: Venue, day: NaiveDate, elapsed_ms: u64) -> Option<u64> {
    let sessions = qmt_session_bounds(venue, day)?;
    let mut remaining = elapsed_ms;

    for (index, (session_start, session_end)) in sessions.iter().copied().enumerate() {
        let session_len = session_end.saturating_sub(session_start);
        if remaining < session_len {
            return Some(session_start + remaining);
        }

        if remaining == session_len {
            return sessions
                .get(index + 1)
                .map(|(next_start, _)| *next_start)
                .or(Some(session_end));
        }

        remaining = remaining.saturating_sub(session_len);
    }

    None
}

pub(super) fn qmt_bucket_start(
    venue: Venue,
    timestamp_ms: u64,
    timeframe: Timeframe,
) -> Option<u64> {
    let dt = china_datetime(timestamp_ms)?;
    if !is_qmt_trading_day(venue, dt.date_naive()) {
        return None;
    }

    if timeframe == Timeframe::D1 {
        let offset = china_offset()?;
        return offset
            .from_local_datetime(&dt.date_naive().and_hms_opt(0, 0, 0)?)
            .single()
            .map(|value| value.timestamp_millis() as u64);
    }

    let interval_ms = qmt_timeframe_ms(timeframe)?;
    let (day, elapsed_ms) = qmt_trading_elapsed_ms(venue, timestamp_ms)?;
    let bucket_elapsed = (elapsed_ms / interval_ms) * interval_ms;
    qmt_timestamp_at_trading_elapsed(venue, day, bucket_elapsed)
}

pub fn is_trading_bucket_start(venue: Venue, timestamp_ms: u64, timeframe: Timeframe) -> bool {
    qmt_bucket_start(venue, timestamp_ms, timeframe) == Some(timestamp_ms)
}

pub fn uses_gapless_time_axis(venue: Venue) -> bool {
    matches!(venue, Venue::SSH | Venue::SSZ)
}

pub fn supports_gapless_time_axis_timeframe(venue: Venue, timeframe: Timeframe) -> bool {
    uses_gapless_time_axis(venue) && qmt_gapless_axis_timeframe_ms(timeframe).is_some()
}

pub(super) fn qmt_heatmap_tick_in_session(venue: Venue, timestamp_ms: u64) -> bool {
    qmt_gapless_axis_bucket_start(venue, timestamp_ms, Timeframe::MS3000).is_some()
}

fn qmt_gapless_axis_bucket_start(
    venue: Venue,
    timestamp_ms: u64,
    timeframe: Timeframe,
) -> Option<u64> {
    let dt = china_datetime(timestamp_ms)?;
    if !is_qmt_trading_day(venue, dt.date_naive()) {
        return None;
    }

    if timeframe == Timeframe::D1 {
        let offset = china_offset()?;
        return offset
            .from_local_datetime(&dt.date_naive().and_hms_opt(0, 0, 0)?)
            .single()
            .map(|value| value.timestamp_millis() as u64);
    }

    let interval_ms = qmt_gapless_axis_timeframe_ms(timeframe)?;
    let (day, elapsed_ms) = qmt_trading_elapsed_ms(venue, timestamp_ms)?;
    let bucket_elapsed = (elapsed_ms / interval_ms) * interval_ms;
    qmt_timestamp_at_trading_elapsed(venue, day, bucket_elapsed)
}

fn qmt_trading_bucket_count(total_trading_ms: u64, interval_ms: u64) -> u64 {
    if interval_ms == 0 || total_trading_ms == 0 {
        return 0;
    }

    total_trading_ms.div_ceil(interval_ms)
}

fn qmt_gapless_axis_buckets_per_day(
    venue: Venue,
    day: NaiveDate,
    timeframe: Timeframe,
) -> Option<i64> {
    if timeframe == Timeframe::D1 {
        return is_qmt_trading_day(venue, day).then_some(1);
    }

    let interval_ms = qmt_gapless_axis_timeframe_ms(timeframe)?;
    let total_trading_ms = qmt_total_trading_ms(venue, day)?;
    let buckets = qmt_trading_bucket_count(total_trading_ms, interval_ms);

    i64::try_from(buckets).ok()
}

fn qmt_gapless_axis_bucket_position(
    venue: Venue,
    timestamp_ms: u64,
    timeframe: Timeframe,
) -> Option<(NaiveDate, usize)> {
    if timeframe == Timeframe::D1 {
        let day = china_trading_day(timestamp_ms)?;
        return Some((day, 0));
    }

    let interval_ms = qmt_gapless_axis_timeframe_ms(timeframe)?;
    let (day, elapsed_ms) = qmt_trading_elapsed_ms(venue, timestamp_ms)?;
    Some((day, usize::try_from(elapsed_ms / interval_ms).ok()?))
}

fn qmt_gapless_axis_bucket_at_index(
    venue: Venue,
    day: NaiveDate,
    timeframe: Timeframe,
    bucket_index: usize,
) -> Option<u64> {
    if timeframe == Timeframe::D1 {
        if bucket_index != 0 || !is_qmt_trading_day(venue, day) {
            return None;
        }

        let offset = china_offset()?;
        return offset
            .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
            .single()
            .map(|value| value.timestamp_millis() as u64);
    }

    let interval_ms = qmt_gapless_axis_timeframe_ms(timeframe)?;
    let elapsed_ms = u64::try_from(bucket_index)
        .ok()?
        .saturating_mul(interval_ms);
    let total_trading_ms = qmt_total_trading_ms(venue, day)?;
    if elapsed_ms >= total_trading_ms {
        return None;
    }

    qmt_timestamp_at_trading_elapsed(venue, day, elapsed_ms)
}

fn qmt_trading_day_distance(venue: Venue, from_day: NaiveDate, to_day: NaiveDate) -> i64 {
    if from_day == to_day {
        return 0;
    }

    if from_day < to_day {
        let mut count = 0_i64;
        let mut day = from_day;
        while day < to_day {
            let Some(next_day) = day.checked_add_days(Days::new(1)) else {
                break;
            };
            day = next_day;
            if is_qmt_trading_day(venue, day) {
                count += 1;
            }
        }
        return count;
    }

    -qmt_trading_day_distance(venue, to_day, from_day)
}

pub(super) fn qmt_shift_trading_day(
    venue: Venue,
    day: NaiveDate,
    offset: i64,
) -> Option<NaiveDate> {
    if offset == 0 {
        return is_qmt_trading_day(venue, day).then_some(day);
    }

    let mut current = day;
    let mut remaining = offset.unsigned_abs();
    let forward = offset > 0;

    while remaining > 0 {
        current = if forward {
            current.checked_add_days(Days::new(1))?
        } else {
            current.checked_sub_days(Days::new(1))?
        };

        if is_qmt_trading_day(venue, current) {
            remaining -= 1;
        }
    }

    Some(current)
}

pub fn time_axis_bucket_offset(
    venue: Venue,
    anchor_ms: u64,
    target_ms: u64,
    timeframe: Timeframe,
) -> Option<i64> {
    if !uses_gapless_time_axis(venue) {
        return None;
    }

    let (anchor_day, anchor_bucket_index) =
        qmt_gapless_axis_bucket_position(venue, anchor_ms, timeframe)?;
    let (target_day, target_bucket_index) =
        qmt_gapless_axis_bucket_position(venue, target_ms, timeframe)?;
    let buckets_per_day = qmt_gapless_axis_buckets_per_day(venue, anchor_day, timeframe)?;
    if buckets_per_day <= 0 {
        return None;
    }

    let day_offset = qmt_trading_day_distance(venue, anchor_day, target_day);
    Some(
        day_offset * buckets_per_day + i64::try_from(target_bucket_index).ok()?
            - i64::try_from(anchor_bucket_index).ok()?,
    )
}

pub fn time_axis_bucket_at_offset(
    venue: Venue,
    anchor_ms: u64,
    timeframe: Timeframe,
    bucket_offset: i64,
) -> Option<u64> {
    if !uses_gapless_time_axis(venue) {
        return None;
    }

    let (anchor_day, anchor_bucket_index) =
        qmt_gapless_axis_bucket_position(venue, anchor_ms, timeframe)?;
    let buckets_per_day = qmt_gapless_axis_buckets_per_day(venue, anchor_day, timeframe)?;
    if buckets_per_day <= 0 {
        return None;
    }

    let total_offset = i64::try_from(anchor_bucket_index).ok()? + bucket_offset;
    let day_offset = total_offset.div_euclid(buckets_per_day);
    let bucket_index = usize::try_from(total_offset.rem_euclid(buckets_per_day)).ok()?;
    let day = qmt_shift_trading_day(venue, anchor_day, day_offset)?;
    qmt_gapless_axis_bucket_at_index(venue, day, timeframe, bucket_index)
}

pub(super) fn qmt_trading_bucket_starts(
    venue: Venue,
    start_ms: u64,
    end_ms: u64,
    timeframe: Timeframe,
) -> Vec<u64> {
    if end_ms < start_ms {
        return Vec::new();
    }

    let Some(start_dt) = china_datetime(start_ms) else {
        return Vec::new();
    };
    let Some(end_dt) = china_datetime(end_ms) else {
        return Vec::new();
    };
    let Some(interval_ms) = qmt_timeframe_ms(timeframe) else {
        return Vec::new();
    };

    let mut day = start_dt.date_naive();
    let last_day = end_dt.date_naive();
    let mut buckets = Vec::new();

    while day <= last_day {
        if timeframe == Timeframe::D1 {
            if let Some(bucket) = qmt_bucket_start(
                venue,
                china_offset()
                    .and_then(|offset| {
                        offset
                            .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
                            .single()
                    })
                    .map(|value| value.timestamp_millis() as u64)
                    .unwrap_or(0),
                timeframe,
            ) && bucket <= end_ms
                && bucket.saturating_add(interval_ms) > start_ms
            {
                buckets.push(bucket);
            }
        } else if let Some(total_trading_ms) = qmt_total_trading_ms(venue, day) {
            let bucket_count = qmt_trading_bucket_count(total_trading_ms, interval_ms);
            for bucket_index in 0..bucket_count {
                let Some(bucket) = qmt_timestamp_at_trading_elapsed(
                    venue,
                    day,
                    bucket_index.saturating_mul(interval_ms),
                ) else {
                    continue;
                };

                if bucket <= end_ms && bucket.saturating_add(interval_ms) > start_ms {
                    buckets.push(bucket);
                }
            }
        }

        let Some(next_day) = day.checked_add_days(Days::new(1)) else {
            break;
        };
        day = next_day;
    }

    buckets
}

pub(super) fn qmt_trading_days_between(
    venue: Venue,
    start_day: NaiveDate,
    end_day: NaiveDate,
) -> Vec<NaiveDate> {
    let mut day = start_day;
    let mut trading_days = Vec::new();

    while day <= end_day {
        if is_qmt_trading_day(venue, day) {
            trading_days.push(day);
        }

        let Some(next_day) = day.checked_add_days(Days::new(1)) else {
            break;
        };
        day = next_day;
    }

    trading_days
}

pub(super) fn qmt_tick_fetch_bounds(venue: Venue, day: NaiveDate) -> Option<(u64, u64)> {
    let sessions = qmt_session_bounds(venue, day)?;
    Some((sessions[0].0, sessions[1].1))
}

pub(super) fn qmt_current_day_history_bounds(day: NaiveDate) -> Option<(u64, u64)> {
    let offset = china_offset()?;
    let start = offset
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let end = offset
        .from_local_datetime(&day.and_hms_opt(16, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    Some((start, end))
}

pub(super) fn qmt_latest_history_chunk_range(
    venue: Venue,
    requested_start: u64,
    requested_end: u64,
) -> Option<(u64, u64)> {
    let (start_day, end_day) = trading_day_range_from_timestamps(requested_start, requested_end)?;
    let trading_days = qmt_trading_days_between(venue, start_day, end_day);
    for day in trading_days.into_iter().rev() {
        let (day_start, day_end) = qmt_tick_fetch_bounds(venue, day)?;
        let chunk_start = requested_start.max(day_start);
        let chunk_end = requested_end.min(day_end);

        if chunk_end > chunk_start {
            return Some((chunk_start, chunk_end));
        }
    }

    None
}

pub(super) fn qmt_kline_seed_start(venue: Venue, start_ms: u64) -> Option<u64> {
    let start_day = china_trading_day(start_ms)?;

    if current_china_day() == Some(start_day)
        && let Some((current_day_start, _)) = qmt_tick_fetch_bounds(venue, start_day)
        && start_ms <= current_day_start
    {
        return qmt_current_day_history_bounds(start_day).map(|(start, _)| start);
    }

    if is_qmt_trading_day(venue, start_day)
        && let Some((current_day_start, _)) = qmt_tick_fetch_bounds(venue, start_day)
    {
        if current_day_start < start_ms {
            return Some(current_day_start);
        }

        if let Some(previous_day) = qmt_shift_trading_day(venue, start_day, -1)
            && let Some((previous_day_start, _)) = qmt_tick_fetch_bounds(venue, previous_day)
        {
            return Some(previous_day_start);
        }

        return Some(current_day_start);
    }

    qmt_shift_trading_day(venue, start_day, -1)
        .and_then(|previous_day| qmt_tick_fetch_bounds(venue, previous_day))
        .map(|(previous_day_start, _)| previous_day_start)
}
