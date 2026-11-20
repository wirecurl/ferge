//! Summary statistics over durations.

use std::fmt;
use std::time::Duration;

/// Order statistics of a sample.
#[derive(Clone, Debug)]
pub(crate) struct Summary {
    sorted: Vec<Duration>,
}

impl Summary {
    pub(crate) fn new(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self { sorted: samples }
    }

    pub(crate) fn len(&self) -> usize {
        self.sorted.len()
    }

    /// Nearest-rank percentile, `p` in `0..=100`. Zero for an empty sample.
    pub(crate) fn percentile(&self, p: u32) -> Duration {
        if self.sorted.is_empty() {
            return Duration::ZERO;
        }
        let rank = (self.sorted.len() * p as usize).div_ceil(100).max(1);
        self.sorted[rank.min(self.sorted.len()) - 1]
    }

    pub(crate) fn median(&self) -> Duration {
        self.percentile(50)
    }

    pub(crate) fn max(&self) -> Duration {
        self.sorted.last().copied().unwrap_or_default()
    }

    /// Number of samples above `limit`.
    pub(crate) fn count_above(&self, limit: Duration) -> usize {
        self.sorted.len() - self.sorted.partition_point(|sample| *sample <= limit)
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "p50 {} · p95 {} · max {} (n={})",
            ms(self.median()),
            ms(self.percentile(95)),
            ms(self.max()),
            self.len()
        )
    }
}

/// Formats a duration in milliseconds with adaptive precision.
pub(crate) fn ms(duration: Duration) -> String {
    let millis = duration.as_secs_f64() * 1000.0;
    if millis < 1.0 {
        format!("{millis:.3} ms")
    } else if millis < 100.0 {
        format!("{millis:.2} ms")
    } else {
        format!("{millis:.0} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let summary = Summary::new((1..=100).rev().map(Duration::from_millis).collect());
        assert_eq!(summary.median(), Duration::from_millis(50));
        assert_eq!(summary.percentile(95), Duration::from_millis(95));
        assert_eq!(summary.percentile(100), Duration::from_millis(100));
        assert_eq!(summary.percentile(0), Duration::from_millis(1));
        assert_eq!(summary.count_above(Duration::from_millis(90)), 10);
        assert_eq!(Summary::new(Vec::new()).median(), Duration::ZERO);
    }
}
