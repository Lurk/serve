//! Latency histogram math: fixed-bucket binning and percentile recovery.
#![allow(clippy::cast_precision_loss, clippy::missing_const_for_fn)]

/// Exclusive upper bounds (ms) for the first 12 buckets. A 13th open bucket
/// holds everything `>= 10s`.
pub const LAT_BOUNDS_MS: [u64; 12] = [1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];
/// Histogram bucket count.
pub const N_BUCKETS: usize = LAT_BOUNDS_MS.len() + 1;

/// Map a latency in milliseconds to its bucket index, `0..=12`.
#[must_use]
pub fn bucket_index(ms: u64) -> usize {
    let mut i = 0;
    while i < LAT_BOUNDS_MS.len() && ms >= LAT_BOUNDS_MS[i] {
        i += 1;
    }
    i
}

/// Recover the `p`-th percentile (0..=100) in ms from a count histogram of
/// length [`N_BUCKETS`] by linear interpolation across cumulative counts. The
/// open top bucket reports its lower bound as the floor.
#[must_use]
pub fn percentile_ms(hist: &[u64], p: f64) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let rank = p / 100.0 * total as f64;
    let mut cum: u64 = 0;
    for (i, &c) in hist.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let next = cum + c;
        if next as f64 >= rank {
            if i >= LAT_BOUNDS_MS.len() {
                // Open top bucket: floor at the last finite bound. Guard runs
                // before the `i - 1` index below so an over-long `hist` can't
                // read past `LAT_BOUNDS_MS`.
                return LAT_BOUNDS_MS[LAT_BOUNDS_MS.len() - 1] as f64;
            }
            let lower = if i == 0 {
                0.0
            } else {
                LAT_BOUNDS_MS[i - 1] as f64
            };
            let upper = LAT_BOUNDS_MS[i] as f64;
            let within = (rank - cum as f64) / c as f64;
            return lower + within * (upper - lower);
        }
        cum = next;
    }
    LAT_BOUNDS_MS[LAT_BOUNDS_MS.len() - 1] as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_index_maps_boundaries() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(1), 1);
        assert_eq!(bucket_index(4), 1);
        assert_eq!(bucket_index(5), 2);
        assert_eq!(bucket_index(10), 3);
        assert_eq!(bucket_index(9999), 11);
        assert_eq!(bucket_index(10_000), 12);
        assert_eq!(bucket_index(60_000), 12);
    }

    #[test]
    fn percentile_interpolates_within_bucket() {
        // 10 samples all in [5,10): p50 -> 7.5ms.
        let mut h = [0u64; N_BUCKETS];
        h[2] = 10;
        assert!((percentile_ms(&h, 50.0) - 7.5).abs() < 1e-6);
    }

    #[test]
    fn percentile_open_top_floors_at_last_bound() {
        let mut h = [0u64; N_BUCKETS];
        h[12] = 4;
        assert!((percentile_ms(&h, 99.0) - 10_000.0).abs() < 1e-6);
    }

    #[allow(clippy::float_cmp)]
    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile_ms(&[0u64; N_BUCKETS], 95.0), 0.0);
    }

    #[test]
    fn percentile_over_long_hist_does_not_panic() {
        // A slice longer than N_BUCKETS with weight past the open bucket must
        // floor at the last bound instead of indexing past LAT_BOUNDS_MS.
        let mut h = [0u64; N_BUCKETS + 2];
        h[N_BUCKETS + 1] = 3;
        assert!((percentile_ms(&h, 99.0) - 10_000.0).abs() < 1e-6);
    }
}
