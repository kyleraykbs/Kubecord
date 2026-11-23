use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

#[derive(Debug, Default, Clone)]
struct Bucket {
    remaining: Option<u32>,
    reset_at: Option<Instant>,
}

/// Route-keyed limiter.
///
/// Discord reports each route's bucket in the `X-RateLimit-*` headers; we key
/// on the normalised route ourselves (see [`normalise_route`]) because the
/// first response has not told us the bucket yet. A request takes a slot from
/// its bucket before it goes out, so several requests sent at once cannot
/// overrun a nearly spent bucket; when it is spent they wait for its reset.
/// If the server still says otherwise, the 429 path backs off and retries.
#[derive(Debug, Default, Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    global_until: Arc<Mutex<Option<Instant>>>,
}

impl RateLimiter {
    pub async fn acquire(&self, route: &str) {
        loop {
            let Some(wait) = self.reserve(route, Instant::now()) else {
                return;
            };
            tokio::time::sleep(wait.min(Duration::from_secs(60))).await;
        }
    }

    /// Takes a slot for `route` now, or says how long to wait for one.
    fn reserve(&self, route: &str, now: Instant) -> Option<Duration> {
        if let Some(until) = *self.global_until.lock()
            && until > now
        {
            return Some(until - now);
        }
        let mut buckets = self.buckets.lock();
        let bucket = buckets.get_mut(route)?;
        match (bucket.remaining, bucket.reset_at) {
            (Some(0), Some(reset_at)) if reset_at > now => Some(reset_at - now),
            // The window reset: the next response says how many it allows.
            (_, Some(reset_at)) if reset_at <= now => {
                bucket.remaining = None;
                bucket.reset_at = None;
                None
            }
            (Some(left), _) => {
                bucket.remaining = Some(left.saturating_sub(1));
                None
            }
            _ => None,
        }
    }

    pub fn update(&self, route: &str, headers: &wreq::header::HeaderMap) {
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

/// The rate-limit key for a request. Discord limits per route and per
/// "major parameter" (the channel, server or webhook it acts on), and the
/// query string is not part of the route. So the first channel / guild /
/// webhook id (and a webhook's token) is kept, any other id becomes `:id`, a
/// reaction's emoji becomes `:emoji`, and the query is dropped: every page of
/// a channel's history shares that channel's bucket.
pub fn normalise_route(method: &str, path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let mut route = String::with_capacity(path.len());
    let mut major_kept = false;
    let mut previous = "";
    for (index, segment) in path.split('/').enumerate() {
        if index > 0 {
            route.push('/');
        }
        let is_id = segment.len() >= 15 && segment.bytes().all(|byte| byte.is_ascii_digit());
        let major = !major_kept && matches!(previous, "channels" | "guilds" | "webhooks");
        if is_id && major {
            major_kept = true;
            route.push_str(segment);
        } else if is_id {
            route.push_str(":id");
        } else if previous == "reactions" {
            route.push_str(":emoji");
        } else {
            // Anything else stays, a webhook's token included (it is part of
            // the webhook's major parameter).
            route.push_str(segment);
        }
        previous = segment;
    }
    format!("{method} {route}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_keep_the_major_parameter_and_drop_the_query() {
        // Every page of one channel's history shares its bucket ...
        assert_eq!(
            normalise_route("GET", "/channels/109494719306248192/messages?limit=50"),
            "GET /channels/109494719306248192/messages"
        );
        assert_eq!(
            normalise_route(
                "GET",
                "/channels/109494719306248192/messages?limit=50&before=1094947193062481999"
            ),
            "GET /channels/109494719306248192/messages"
        );
        // ... while other ids collapse, and the emoji is one bucket.
        assert_eq!(
            normalise_route("DELETE", "/channels/109494719306248192/messages/1094947193062481999"),
            "DELETE /channels/109494719306248192/messages/:id"
        );
        assert_eq!(
            normalise_route(
                "PUT",
                "/channels/109494719306248192/messages/1094947193062481999/reactions/%F0%9F%91%8D/@me"
            ),
            "PUT /channels/109494719306248192/messages/:id/reactions/:emoji/@me"
        );
        assert_eq!(
            normalise_route("PATCH", "/guilds/109494719306248192/members/1094947193062481999"),
            "PATCH /guilds/109494719306248192/members/:id"
        );
        assert_eq!(normalise_route("GET", "/gateway"), "GET /gateway");
    }

    #[test]
    fn a_spent_bucket_waits_and_concurrent_requests_take_slots() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        limiter.buckets.lock().insert(
            "POST /x".to_owned(),
            Bucket {
                remaining: Some(2),
                reset_at: Some(now + Duration::from_secs(5)),
            },
        );
        // Two slots, taken by two requests going out at once ...
        assert_eq!(limiter.reserve("POST /x", now), None);
        assert_eq!(limiter.reserve("POST /x", now), None);
        // ... and the third waits for the reset instead of overrunning it.
        let wait = limiter.reserve("POST /x", now).expect("must wait");
        assert!(wait > Duration::from_secs(4) && wait <= Duration::from_secs(5));
        // After the reset the window is unknown again, so requests go.
        assert_eq!(limiter.reserve("POST /x", now + Duration::from_secs(6)), None);
        // An unseen route never waits.
        assert_eq!(limiter.reserve("GET /y", now), None);
    }
}
