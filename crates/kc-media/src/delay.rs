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
    pub fn start(
        delay: Duration,
        mut send: impl FnMut(T, u32) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<(Instant, T, u32)>();
        std::thread::spawn(move || {
            let mut next = Instant::now() + delay;
            while let Ok((queued, item, frame_ms)) = rx.recv() {
                let latency = queued.elapsed();
                if latency > Duration::from_millis(100) {
                    tracing::debug!(
                        latency_ms = latency.as_millis(),
                        frame_ms,
                        "video delay queue backlog"
                    );
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
