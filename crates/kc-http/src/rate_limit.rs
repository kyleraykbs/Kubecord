use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

#[derive(Debug, Default, Clone)]
struct Bucket {
    remaining: Option<u32>,
    reset_at: Option<Instant>,
}

/// Route-keyed optimistic limiter.
///
/// Discord reports the bucket per route via `X-RateLimit-Bucket`; we key on the
/// normalised route ourselves because the first response has not told us the
/// bucket yet. Waiting is optimistic: if the server says otherwise, the 429
/// path backs off and retries.
#[derive(Debug, Default, Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    global_until: Arc<Mutex<Option<Instant>>>,
}

impl RateLimiter {
    pub async fn acquire(&self, route: &str) {
        let wait = {
            let global = *self.global_until.lock();
            let now = Instant::now();
            if let Some(until) = global {
                if until > now { Some(until - now) } else { None }
            } else {
                let buckets = self.buckets.lock();
                buckets
                    .get(route)
                    .and_then(|bucket| match (bucket.remaining, bucket.reset_at) {
                        (Some(0), Some(reset_at)) if reset_at > now => Some(reset_at - now),
                        _ => None,
                    })
            }
        };

        if let Some(wait) = wait {
            tokio::time::sleep(wait.min(Duration::from_secs(60))).await;
        }
    }

    pub fn update(&self, route: &str, headers: &reqwest::header::HeaderMap) {
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u32>().ok());
        let reset_after = headers
            .get("x-ratelimit-reset-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok());

        if remaining.is_some() || reset_after.is_some() {
            let reset_at =
                reset_after.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
            self.buckets.lock().insert(
                route.to_owned(),
                Bucket {
                    remaining,
                    reset_at,
                },
            );
        }

        if headers
            .get("x-ratelimit-global")
            .and_then(|value| value.to_str().ok())
            == Some("true")
            && let Some(retry_after) = headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<f64>().ok())
        {
            *self.global_until.lock() = Some(Instant::now() + Duration::from_secs_f64(retry_after));
        }
    }

    pub fn set_global(&self, retry_after: f64) {
        *self.global_until.lock() = Some(Instant::now() + Duration::from_secs_f64(retry_after));
    }
}

/// Replaces snowflakes in a path with `:id` so all `/channels/<id>/messages`
/// requests share a bucket.
pub fn normalise_route(method: &str, path: &str) -> String {
    let mut route = String::with_capacity(path.len());
    for (index, segment) in path.split('/').enumerate() {
        if index > 0 {
            route.push('/');
        }
        if segment.len() >= 15 && segment.bytes().all(|byte| byte.is_ascii_digit()) {
            route.push_str(":id");
        } else {
            route.push_str(segment);
        }
    }
    format!("{method} {route}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_snowflakes_only() {
        assert_eq!(
            normalise_route("GET", "/channels/109494719306248192/messages?limit=50"),
            "GET /channels/:id/messages?limit=50"
        );
        assert_eq!(
            normalise_route("POST", "/channels/109494719306248192/messages"),
            "POST /channels/:id/messages"
        );
        assert_eq!(normalise_route("GET", "/gateway"), "GET /gateway");
    }
}
