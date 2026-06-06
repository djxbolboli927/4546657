use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Aggregate-only counters. The hot path touches nothing but relaxed atomics —
/// no per-update logging, no allocation. A background task prints a one-line
/// summary every reporting window.
pub struct Metrics {
    /// Total Account updates received from the upstream stream.
    pub from_upstream: AtomicU64,
    /// Account updates handed to the bot sink (had a populated account info).
    pub to_bot: AtomicU64,
    /// Most recent slot seen on the upstream stream (gauge).
    pub last_slot: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            from_upstream: AtomicU64::new(0),
            to_bot: AtomicU64::new(0),
            last_slot: AtomicU64::new(0),
        }
    }

    pub fn spawn_reporter(self: &Arc<Self>, every: Duration) {
        let m = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                let up = m.from_upstream.swap(0, Ordering::Relaxed);
                let bot = m.to_bot.swap(0, Ordering::Relaxed);
                let slot = m.last_slot.load(Ordering::Relaxed);
                eprintln!(
                    "[fanout] updates_from_upstream={up} updates_to_bot={bot} last_upstream_slot={slot}"
                );
            }
        });
    }
}
