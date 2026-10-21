//! Pure aggregation for long-range usage statistics.
//!
//! The store runs the SQL aggregates (counts, tallies, attempt intervals); this module turns them
//! into per-bucket series, sweeps attempt intervals for concurrency, and merges finished-day
//! rollups. Like the pool core it is synchronous and takes time as plain `i64` milliseconds.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MINUTE_MS: i64 = 60_000;
pub const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const DAY_MS: i64 = 24 * HOUR_MS;
/// Longest attempt the stats look back for when counting concurrency, and how long after a UTC
/// day ends before its rollup is final (rows are written when an attempt finishes).
pub const MAX_ATTEMPT_MS: i64 = 6 * HOUR_MS;
pub const DEFAULT_RETENTION_DAYS: i64 = 730;
/// Key of the single series for `Dim::Total`.
pub const TOTAL_KEY: &str = "*";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dim {
    Total,
    Model,
    Account,
}

impl Dim {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Model => "model",
            Self::Account => "account",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "total" => Some(Self::Total),
            "model" => Some(Self::Model),
            "account" => Some(Self::Account),
            _ => None,
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::Total, Self::Model, Self::Account]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// Distinct client requests (failover attempts share a request id).
    pub requests: i64,
    /// Upstream attempts.
    pub attempts: i64,
    /// Distinct prompt-cache keys.
    pub sessions: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Successful attempts whose token usage was not reported.
    pub unmeasured: i64,
    /// Most upstream attempts in flight at once.
    pub peak_concurrency: i64,
    /// Time-weighted mean of attempts in flight, times 1000.
    pub mean_concurrency_milli: i64,
    /// Attempt outcomes: stream outcomes (`completed`, `failed`, ...), `http_<status>`,
    /// `network_error`, or `unknown` for rows recorded before outcomes existed.
    #[serde(default)]
    pub outcomes: BTreeMap<String, i64>,
    /// Upstream events by kind (`overloaded`, `policy_blocked`, `pool_saturated`, ...).
    #[serde(default)]
    pub events: BTreeMap<String, i64>,
}

/// One SQL aggregate row for a bucket index and series key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CountRow {
    pub bucket: i64,
    pub key: String,
    pub label: String,
    pub requests: i64,
    pub attempts: i64,
    pub sessions: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub unmeasured: i64,
}

/// A named count (an outcome or event kind) for a bucket index and series key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TallyRow {
    pub bucket: i64,
    pub key: String,
    pub label: String,
    pub name: String,
    pub count: i64,
}

/// One upstream attempt's lifetime, `[start, end)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interval {
    pub key: String,
    pub start: i64,
    pub end: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub start_ms: i64,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Series {
    pub key: String,
    pub label: String,
    pub buckets: Vec<Bucket>,
}

pub fn bucket_count(from: i64, to: i64, bucket_ms: i64) -> usize {
    if to <= from || bucket_ms <= 0 {
        return 0;
    }
    ((to - from + bucket_ms - 1) / bucket_ms) as usize
}

/// @cc [owner:ghuntley,label:accounting] concurrency-sweep
/// For each bucket `[from + i*bucket_ms, min(from + (i+1)*bucket_ms, to))`, `concurrency` MUST
/// report the maximum number of half-open intervals covering any instant in the bucket and the
/// time-weighted mean coverage times 1000 (floored); intervals outside the bucket MUST NOT count.
pub fn concurrency(
    intervals: &[(i64, i64)],
    from: i64,
    to: i64,
    bucket_ms: i64,
) -> Vec<(i64, i64)> {
    let count = bucket_count(from, to, bucket_ms);
    let mut points: Vec<(i64, i64)> = Vec::with_capacity(intervals.len() * 2);
    for &(start, end) in intervals {
        if end > start {
            points.push((start, 1));
            points.push((end, -1));
        }
    }
    // Half-open intervals: an attempt ending at t is gone before one starting at t arrives.
    points.sort_unstable();
    let mut out = Vec::with_capacity(count);
    let mut index = 0;
    let mut active: i64 = 0;
    for bucket in 0..count {
        let bucket_start = from + bucket as i64 * bucket_ms;
        let bucket_end = (bucket_start + bucket_ms).min(to);
        while index < points.len() && points[index].0 <= bucket_start {
            active += points[index].1;
            index += 1;
        }
        let mut peak = active;
        let mut area: i128 = 0;
        let mut previous = bucket_start;
        while index < points.len() && points[index].0 < bucket_end {
            let (at, delta) = points[index];
            area += i128::from(active) * i128::from(at - previous);
            previous = at;
            active += delta;
            peak = peak.max(active);
            index += 1;
        }
        area += i128::from(active) * i128::from(bucket_end - previous);
        let mean_milli = (area * 1000 / i128::from(bucket_end - bucket_start)) as i64;
        out.push((peak, mean_milli));
    }
    out
}

/// @cc [owner:ghuntley,label:accounting] stats-assemble-partition
/// `assemble` MUST place every count, tally, and interval only in the series named by its key and
/// in the bucket it reports, MUST emit one bucket per `bucket_count(from, to, bucket_ms)` for every
/// series that has any row, and MUST order series by key.
pub fn assemble(
    from: i64,
    to: i64,
    bucket_ms: i64,
    counts: &[CountRow],
    outcomes: &[TallyRow],
    events: &[TallyRow],
    intervals: &[Interval],
) -> Vec<Series> {
    let count = bucket_count(from, to, bucket_ms);
    let mut series: BTreeMap<String, Series> = BTreeMap::new();
    for row in counts {
        if let Some(totals) = bucket_mut(
            &mut series,
            count,
            from,
            bucket_ms,
            &row.key,
            &row.label,
            row.bucket,
        ) {
            totals.requests += row.requests;
            totals.attempts += row.attempts;
            totals.sessions += row.sessions;
            totals.input_tokens += row.input_tokens;
            totals.output_tokens += row.output_tokens;
            totals.unmeasured += row.unmeasured;
        }
    }
    for row in outcomes {
        if let Some(totals) = bucket_mut(
            &mut series,
            count,
            from,
            bucket_ms,
            &row.key,
            &row.label,
            row.bucket,
        ) {
            *totals.outcomes.entry(row.name.clone()).or_default() += row.count;
        }
    }
    for row in events {
        if let Some(totals) = bucket_mut(
            &mut series,
            count,
            from,
            bucket_ms,
            &row.key,
            &row.label,
            row.bucket,
        ) {
            *totals.events.entry(row.name.clone()).or_default() += row.count;
        }
    }
    let mut by_key: BTreeMap<&str, Vec<(i64, i64)>> = BTreeMap::new();
    for interval in intervals {
        by_key
            .entry(interval.key.as_str())
            .or_default()
            .push((interval.start, interval.end));
    }
    for (key, spans) in by_key {
        let Some(series) = series.get_mut(key) else {
            continue;
        };
        for (bucket, (peak, mean)) in series
            .buckets
            .iter_mut()
            .zip(concurrency(&spans, from, to, bucket_ms))
        {
            bucket.totals.peak_concurrency = peak;
            bucket.totals.mean_concurrency_milli = mean;
        }
    }
    series.into_values().collect()
}

fn bucket_mut<'a>(
    series: &'a mut BTreeMap<String, Series>,
    count: usize,
    from: i64,
    bucket_ms: i64,
    key: &str,
    label: &str,
    bucket: i64,
) -> Option<&'a mut Totals> {
    let index = usize::try_from(bucket).ok().filter(|i| *i < count)?;
    let series = series.entry(key.to_string()).or_insert_with(|| Series {
        key: key.to_string(),
        label: label.to_string(),
        buckets: (0..count)
            .map(|i| Bucket {
                start_ms: from + i as i64 * bucket_ms,
                totals: Totals::default(),
            })
            .collect(),
    });
    if series.label.is_empty() || series.label == series.key {
        series.label = label.to_string();
    }
    Some(&mut series.buckets[index].totals)
}

/// @cc [owner:ghuntley,label:accounting] stats-combine
/// `combine` MUST sum every additive field and named count, take the maximum peak, and weight
/// mean concurrency by each part's span; a sum of distinct counts (requests, sessions) is an
/// upper bound that callers MUST replace with an exact distinct count when one is available.
pub fn combine(parts: &[(&Totals, i64)]) -> Totals {
    let mut out = Totals::default();
    let mut area: i128 = 0;
    let mut span: i128 = 0;
    for (totals, part_span) in parts {
        out.requests += totals.requests;
        out.attempts += totals.attempts;
        out.sessions += totals.sessions;
        out.input_tokens += totals.input_tokens;
        out.output_tokens += totals.output_tokens;
        out.unmeasured += totals.unmeasured;
        out.peak_concurrency = out.peak_concurrency.max(totals.peak_concurrency);
        for (name, count) in &totals.outcomes {
            *out.outcomes.entry(name.clone()).or_default() += count;
        }
        for (name, count) in &totals.events {
            *out.events.entry(name.clone()).or_default() += count;
        }
        area += i128::from(totals.mean_concurrency_milli) * i128::from(*part_span);
        span += i128::from(*part_span);
    }
    if span > 0 {
        out.mean_concurrency_milli = (area / span) as i64;
    }
    out
}

/// Parses a window like `90m`, `24h`, `7d`, or `2y` into milliseconds.
pub fn parse_since(value: &str) -> Option<i64> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let amount: i64 = value[..split].parse().ok().filter(|n| *n > 0)?;
    let unit = match &value[split..] {
        "m" => MINUTE_MS,
        "h" => HOUR_MS,
        "d" => DAY_MS,
        "w" => 7 * DAY_MS,
        "y" => 365 * DAY_MS,
        _ => return None,
    };
    amount.checked_mul(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_counts_overlap_per_bucket() {
        let spans = [(0, 10), (5, 15), (10, 20)];
        let buckets = concurrency(&spans, 0, 20, 10);
        assert_eq!(buckets[0], (2, 1500));
        assert_eq!(buckets[1], (2, 1500));
    }

    #[test]
    fn back_to_back_attempts_do_not_overlap() {
        assert_eq!(concurrency(&[(0, 5), (5, 10)], 0, 10, 10), vec![(1, 1000)]);
    }

    #[test]
    fn interval_started_before_window_counts() {
        assert_eq!(concurrency(&[(-100, 5)], 0, 10, 10), vec![(1, 500)]);
    }

    #[test]
    fn combine_weights_mean_by_span() {
        let a = Totals {
            mean_concurrency_milli: 1000,
            peak_concurrency: 2,
            attempts: 1,
            ..Default::default()
        };
        let b = Totals {
            mean_concurrency_milli: 4000,
            peak_concurrency: 5,
            attempts: 2,
            ..Default::default()
        };
        let out = combine(&[(&a, 3), (&b, 1)]);
        assert_eq!(
            (
                out.attempts,
                out.peak_concurrency,
                out.mean_concurrency_milli
            ),
            (3, 5, 1750)
        );
    }

    #[test]
    fn since_units() {
        assert_eq!(parse_since("24h"), Some(DAY_MS));
        assert_eq!(parse_since("730d"), Some(730 * DAY_MS));
        assert_eq!(parse_since("2y"), Some(730 * DAY_MS));
        assert_eq!(parse_since("0d"), None);
        assert_eq!(parse_since("d"), None);
        assert_eq!(parse_since("5x"), None);
    }
}
