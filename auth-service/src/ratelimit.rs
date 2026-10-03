use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_LOCK: u64 = 15 * 60;
/// Failures older than this no longer count towards the next lock.
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);

struct Entry {
    failures: u32,
    last_failure: Instant,
    locked_until: Option<Instant>,
}

/// Per-key failure counter with exponential lockout. Keys are `user:<login>` (locks after 5
/// failures) and `ip:<addr>` (after 20). The n-th failure past the threshold t locks for 2^(n-t)
/// seconds, capped at 15 minutes.
// ponytail: in-memory, lost on restart; move to the DB if run as multiple instances
#[derive(Default)]
pub struct RateLimiter(Mutex<HashMap<String, Entry>>);

fn threshold(key: &str) -> u32 {
    if key.starts_with("ip:") { 20 } else { 5 }
}

impl RateLimiter {
    /// `Err(remaining)` while the key is locked. Checking does not count as a failure.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key).and_then(|e| e.locked_until) {
            Some(until) if until > Instant::now() => Err(until - Instant::now()),
            _ => Ok(()),
        }
    }

    pub fn fail(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let e = map.entry(key.to_string()).or_insert(Entry { failures: 0, last_failure: now, locked_until: None });
        if now.duration_since(e.last_failure) > FORGET_AFTER {
            e.failures = 0;
        }
        e.failures += 1;
        e.last_failure = now;
        let t = threshold(key);
        if e.failures >= t {
            let secs = (1u64 << (e.failures - t).min(20)).min(MAX_LOCK);
            e.locked_until = Some(now + Duration::from_secs(secs));
        }
    }

    pub fn clear(&self, key: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_threshold_and_clears() {
        let l = RateLimiter::default();
        for _ in 0..4 {
            l.fail("user:a");
            assert!(l.check("user:a").is_ok());
        }
        l.fail("user:a");
        assert!(l.check("user:a").is_err());
        assert!(l.check("user:b").is_ok());
        l.clear("user:a");
        assert!(l.check("user:a").is_ok());
        for _ in 0..19 {
            l.fail("ip:1");
        }
        assert!(l.check("ip:1").is_ok());
        l.fail("ip:1");
        assert!(l.check("ip:1").is_err());
    }
}
