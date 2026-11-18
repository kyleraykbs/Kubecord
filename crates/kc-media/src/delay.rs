//! Fixed-delay, paced sender for captured video/audio frames.

use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

/// Buffers items for a fixed delay and releases them at a steady frame cadence.
/// Delaying both a stream's video and audio keeps their relative timing.
pub struct DelayedSender<T> {
    tx: Sender<(Instant, T, u32)>,
}

impl<T> Clone for DelayedSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<T: Send + 'static> DelayedSender<T> {
    pub fn start(delay: Duration, send: impl FnMut(T, u32) + Send + 'static) -> Self {
        Self::start_with_limit(delay, None, send)
    }

    /// Like [`start`](Self::start), but drops items whose queue latency
    /// exceeds `max_extra_latency`. The configured stream delay itself does
    /// not count against the limit.
    pub fn start_with_limit(
        delay: Duration,
        max_extra_latency: Option<Duration>,
        mut send: impl FnMut(T, u32) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<(Instant, T, u32)>();
        std::thread::spawn(move || {
            let mut next = Instant::now() + delay;
            let mut dropped = 0u64;
            let mut last_drop_log: Option<Instant> = None;
            while let Ok((queued, item, frame_ms)) = rx.recv() {
                let excess = queued.elapsed().saturating_sub(delay);
                if max_extra_latency.is_some_and(|limit| excess > limit) {
                    dropped += 1;
                    if last_drop_log.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)) {
                        tracing::debug!(
                            excess_ms = excess.as_millis(),
                            dropped,
                            "dropping late video frame"
                        );
                        dropped = 0;
                        last_drop_log = Some(Instant::now());
                    }
                    continue;
                }
                let due = (queued + delay).max(next);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                send(item, frame_ms);
                next = due + Duration::from_millis(frame_ms.max(1) as u64);
            }
        });
        Self { tx }
    }

    pub fn send(&self, item: T, frame_ms: u32) {
        let _ = self.tx.send((Instant::now(), item, frame_ms));
    }
}
