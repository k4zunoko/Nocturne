use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nocturne_core::ClockPort;

#[derive(Debug, Clone, Default)]
pub struct LocalClock {
    tick: Arc<AtomicU64>,
}

impl LocalClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ClockPort for LocalClock {
    fn now(&self) -> String {
        let second = self.tick.fetch_add(1, Ordering::Relaxed) % 60;
        format!("2026-04-23T12:34:{second:02}Z")
    }
}
