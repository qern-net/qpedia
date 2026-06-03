//! Per-tenant token-bucket rate limiter for the chat endpoint.
//!
//! Default refill at `QPEDIA_CHAT_RPM` requests/minute up to a burst of
//! `QPEDIA_CHAT_BURST`. Out of the box: **30 RPM** (one every 2 s) with
//! a burst of **10** — generous for interactive use, hostile to a
//! runaway client. Defaults trade some friendliness for a hard ceiling
//! on LLM spend per tenant.
//!
//! Limitation: in-process. Two `qpedia-api` instances behind a load
//! balancer each have their own buckets, so the effective limit is
//! `N × QPEDIA_CHAT_RPM` across the fleet. `qpedia-pvt` swaps in a
//! Redis-backed implementation by calling
//! `AppBuilder::with_chat_rate_limiter` with its own Arc.

use qpedia_core::tenant::Tenant;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// In-process token-bucket limiter, one bucket per tenant. The bucket
/// is created on first use; refills lazily on every `check` call.
pub struct ChatRateLimiter {
    rpm: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl ChatRateLimiter {
    /// Construct from env (`QPEDIA_CHAT_RPM`, `QPEDIA_CHAT_BURST`).
    /// Both are clamped to a minimum of 1.0.
    pub fn from_env() -> Self {
        let rpm: f64 = std::env::var("QPEDIA_CHAT_RPM")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(30.0)
            .max(1.0);
        let burst: f64 = std::env::var("QPEDIA_CHAT_BURST")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(10.0)
            .max(1.0);
        Self::new(rpm, burst)
    }

    /// Construct with explicit limits. Useful for tests + overlay impls.
    pub fn new(rpm: f64, burst: f64) -> Self {
        Self {
            rpm,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Try to take one token for `tenant`. Returns `Ok(())` if allowed,
    /// `Err(retry_after_seconds)` if denied. The retry-after value is
    /// the integer ceiling of "time until the next token is available"
    /// in seconds, minimum 1.
    pub fn check(&self, tenant: &Tenant) -> Result<(), u64> {
        let mut map = self
            .buckets
            .lock()
            .expect("ChatRateLimiter bucket mutex poisoned");
        let now = Instant::now();
        let bucket = map.entry(tenant.as_str().to_string()).or_insert(Bucket {
            tokens: self.burst,
            last_refill: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rpm / 60.0).min(self.burst);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let need = 1.0 - bucket.tokens;
            let secs = (need * 60.0 / self.rpm).ceil() as u64;
            Err(secs.max(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Tenant {
        Tenant::new("test")
    }

    #[test]
    fn allows_burst_then_denies() {
        let lim = ChatRateLimiter::new(60.0, 3.0); // 60 RPM, burst 3
        assert!(lim.check(&t()).is_ok());
        assert!(lim.check(&t()).is_ok());
        assert!(lim.check(&t()).is_ok());
        // 4th in the same instant is denied.
        let r = lim.check(&t()).unwrap_err();
        assert!(r >= 1, "retry should be at least 1s, got {r}");
    }

    #[test]
    fn tenants_isolated() {
        let lim = ChatRateLimiter::new(60.0, 1.0);
        let a = Tenant::new("a");
        let b = Tenant::new("b");
        assert!(lim.check(&a).is_ok());
        assert!(lim.check(&a).is_err()); // a is dry
        assert!(lim.check(&b).is_ok()); // b is independent
    }
}
