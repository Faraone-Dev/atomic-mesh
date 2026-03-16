use std::sync::atomic::{AtomicU64, Ordering};
use std::fmt;

const BUCKET_COUNT: usize = 10;

/// Bucket thresholds in nanoseconds.
const THRESHOLDS: [u64; 9] = [
    100,             // 100 ns
    1_000,           // 1 μs
    10_000,          // 10 μs
    100_000,         // 100 μs
    1_000_000,       // 1 ms
    10_000_000,      // 10 ms
    100_000_000,     // 100 ms
    1_000_000_000,   // 1 s
    10_000_000_000,  // 10 s
];

const BUCKET_LABELS: [&str; BUCKET_COUNT] = [
    "<100ns", "<1μs", "<10μs", "<100μs", "<1ms",
    "<10ms", "<100ms", "<1s", "<10s", "≥10s",
];

/// Fixed-bucket histogram for nanosecond latencies.
/// Lock-free, thread-safe, zero-allocation after init.
pub struct LatencyHistogram {
    name: &'static str,
    buckets: [AtomicU64; BUCKET_COUNT],
    sum_ns: AtomicU64,
    count: AtomicU64,
    min_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl LatencyHistogram {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            buckets: [
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            sum_ns: AtomicU64::new(0),
            count: AtomicU64::new(0),
            min_ns: AtomicU64::new(u64::MAX),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Record a latency sample in nanoseconds.
    pub fn record(&self, ns: u64) {
        let idx = THRESHOLDS.iter().position(|&t| ns < t).unwrap_or(BUCKET_COUNT - 1);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);

        // Update min (CAS loop)
        let mut current = self.min_ns.load(Ordering::Relaxed);
        while ns < current {
            match self.min_ns.compare_exchange_weak(current, ns, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }

        // Update max
        let mut current = self.max_ns.load(Ordering::Relaxed);
        while ns > current {
            match self.max_ns.compare_exchange_weak(current, ns, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn sum_ns(&self) -> u64 {
        self.sum_ns.load(Ordering::Relaxed)
    }

    pub fn avg_ns(&self) -> u64 {
        let c = self.count();
        if c == 0 { return 0; }
        self.sum_ns() / c
    }

    pub fn min_ns(&self) -> u64 {
        let v = self.min_ns.load(Ordering::Relaxed);
        if v == u64::MAX { 0 } else { v }
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns.load(Ordering::Relaxed)
    }

    /// Approximate percentile (from bucket boundaries).
    pub fn percentile(&self, pct: f64) -> u64 {
        let total = self.count();
        if total == 0 { return 0; }
        let target = (total as f64 * pct / 100.0).ceil() as u64;
        let mut cumulative = 0u64;
        for (i, bucket) in self.buckets.iter().enumerate() {
            cumulative += bucket.load(Ordering::Relaxed);
            if cumulative >= target {
                return if i < THRESHOLDS.len() { THRESHOLDS[i] } else { u64::MAX };
            }
        }
        u64::MAX
    }

    /// Snapshot of all buckets.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut buckets = [0u64; BUCKET_COUNT];
        for (i, b) in self.buckets.iter().enumerate() {
            buckets[i] = b.load(Ordering::Relaxed);
        }
        HistogramSnapshot {
            name: self.name,
            buckets,
            count: self.count(),
            sum_ns: self.sum_ns(),
            min_ns: self.min_ns(),
            max_ns: self.max_ns(),
        }
    }

    /// Reset all counters.
    pub fn reset(&self) {
        for b in &self.buckets {
            b.store(0, Ordering::Relaxed);
        }
        self.sum_ns.store(0, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
        self.min_ns.store(u64::MAX, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct HistogramSnapshot {
    pub name: &'static str,
    pub buckets: [u64; BUCKET_COUNT],
    pub count: u64,
    pub sum_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
}

impl HistogramSnapshot {
    pub fn avg_ns(&self) -> u64 {
        if self.count == 0 { 0 } else { self.sum_ns / self.count }
    }
}

impl fmt::Display for HistogramSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  {} (n={}, avg={}, min={}, max={} ns)",
            self.name, self.count,
            format_ns(self.avg_ns()), format_ns(self.min_ns), format_ns(self.max_ns)
        )?;
        for (i, &count) in self.buckets.iter().enumerate() {
            if count > 0 {
                let pct = count as f64 / self.count.max(1) as f64 * 100.0;
                writeln!(f, "    {:<8} {:>8} ({:.1}%)", BUCKET_LABELS[i], count, pct)?;
            }
        }
        Ok(())
    }
}

fn format_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    } else if ns >= 1_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.2}μs", ns as f64 / 1_000.0)
    } else {
        format!("{}ns", ns)
    }
}

/// RAII timer: records elapsed time to a histogram on drop.
pub struct StageTimer<'a> {
    start: std::time::Instant,
    histogram: &'a LatencyHistogram,
}

impl<'a> StageTimer<'a> {
    pub fn start(histogram: &'a LatencyHistogram) -> Self {
        Self {
            start: std::time::Instant::now(),
            histogram,
        }
    }

    pub fn elapsed_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

impl<'a> Drop for StageTimer<'a> {
    fn drop(&mut self) {
        self.histogram.record(self.elapsed_ns());
    }
}

/// Pipeline metrics: one histogram per stage.
pub struct PipelineMetrics {
    pub feed_recv: LatencyHistogram,
    pub feed_normalize: LatencyHistogram,
    pub ring_enqueue: LatencyHistogram,
    pub strategy_compute: LatencyHistogram,
    pub risk_check: LatencyHistogram,
    pub order_submit: LatencyHistogram,
    pub order_to_ack: LatencyHistogram,
    pub order_to_fill: LatencyHistogram,
    pub event_processing: LatencyHistogram,
    pub state_hash: LatencyHistogram,
    // Counters
    pub total_events: AtomicU64,
    pub total_orders: AtomicU64,
    pub total_fills: AtomicU64,
    pub total_rejects: AtomicU64,
    pub sequence_gaps: AtomicU64,
}

impl PipelineMetrics {
    pub const fn new() -> Self {
        Self {
            feed_recv: LatencyHistogram::new("feed_recv"),
            feed_normalize: LatencyHistogram::new("feed_normalize"),
            ring_enqueue: LatencyHistogram::new("ring_enqueue"),
            strategy_compute: LatencyHistogram::new("strategy_compute"),
            risk_check: LatencyHistogram::new("risk_check"),
            order_submit: LatencyHistogram::new("order_submit"),
            order_to_ack: LatencyHistogram::new("order_to_ack"),
            order_to_fill: LatencyHistogram::new("order_to_fill"),
            event_processing: LatencyHistogram::new("event_processing"),
            state_hash: LatencyHistogram::new("state_hash"),
            total_events: AtomicU64::new(0),
            total_orders: AtomicU64::new(0),
            total_fills: AtomicU64::new(0),
            total_rejects: AtomicU64::new(0),
            sequence_gaps: AtomicU64::new(0),
        }
    }

    /// Print a full metrics report.
    pub fn report(&self) -> String {
        let histograms = [
            self.feed_recv.snapshot(),
            self.feed_normalize.snapshot(),
            self.ring_enqueue.snapshot(),
            self.strategy_compute.snapshot(),
            self.risk_check.snapshot(),
            self.order_submit.snapshot(),
            self.order_to_ack.snapshot(),
            self.order_to_fill.snapshot(),
            self.event_processing.snapshot(),
            self.state_hash.snapshot(),
        ];

        let mut report = String::new();
        report.push_str("═══ ATOMIC MESH METRICS ═══\n");
        report.push_str(&format!(
            "  events: {}  orders: {}  fills: {}  rejects: {}  seq_gaps: {}\n",
            self.total_events.load(Ordering::Relaxed),
            self.total_orders.load(Ordering::Relaxed),
            self.total_fills.load(Ordering::Relaxed),
            self.total_rejects.load(Ordering::Relaxed),
            self.sequence_gaps.load(Ordering::Relaxed),
        ));
        for h in &histograms {
            if h.count > 0 {
                report.push_str(&format!("{}", h));
            }
        }
        report
    }

    /// Reset all metrics.
    pub fn reset(&self) {
        self.feed_recv.reset();
        self.feed_normalize.reset();
        self.ring_enqueue.reset();
        self.strategy_compute.reset();
        self.risk_check.reset();
        self.order_submit.reset();
        self.order_to_ack.reset();
        self.order_to_fill.reset();
        self.event_processing.reset();
        self.state_hash.reset();
        self.total_events.store(0, Ordering::Relaxed);
        self.total_orders.store(0, Ordering::Relaxed);
        self.total_fills.store(0, Ordering::Relaxed);
        self.total_rejects.store(0, Ordering::Relaxed);
        self.sequence_gaps.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_records_and_counts() {
        let h = LatencyHistogram::new("test");
        h.record(50);     // <100ns bucket
        h.record(500);    // <1μs bucket
        h.record(5_000);  // <10μs bucket

        assert_eq!(h.count(), 3);
        assert_eq!(h.min_ns(), 50);
        assert_eq!(h.max_ns(), 5_000);
    }

    #[test]
    fn histogram_percentile() {
        let h = LatencyHistogram::new("test");
        for _ in 0..100 {
            h.record(50); // all in <100ns bucket
        }
        // p50 should be in the <100ns bucket threshold
        assert_eq!(h.percentile(50.0), 100);
        assert_eq!(h.percentile(99.0), 100);
    }

    #[test]
    fn stage_timer_records_on_drop() {
        let h = LatencyHistogram::new("test");
        {
            let _timer = StageTimer::start(&h);
            // do some work
            std::hint::black_box(42);
        }
        assert_eq!(h.count(), 1);
        assert!(h.min_ns() > 0 || h.max_ns() > 0);
    }

    #[test]
    fn pipeline_metrics_report() {
        let m = PipelineMetrics::new();
        m.feed_recv.record(1_000);
        m.strategy_compute.record(50_000);
        m.total_events.fetch_add(1, Ordering::Relaxed);

        let report = m.report();
        assert!(report.contains("ATOMIC MESH METRICS"));
        assert!(report.contains("feed_recv"));
        assert!(report.contains("strategy_compute"));
    }
}
