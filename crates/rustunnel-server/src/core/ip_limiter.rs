//! Sliding-window IP-based rate limiter.
//!
//! Keeps one `VecDeque<Instant>` per source IP.  Each incoming request appends
//! the current timestamp; timestamps older than `window` are pruned.  If the
//! deque length exceeds `max_per_window` after pruning, the request is denied.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use dashmap::DashMap;

pub struct IpRateLimiter {
    windows: DashMap<IpAddr, VecDeque<Instant>>,
    window: Duration,
    max_per_window: usize,
}

impl IpRateLimiter {
    /// Create a new limiter allowing `max_per_second` requests per IP
    /// measured over a 1-second sliding window.
    pub fn new(max_per_second: u32) -> Self {
        Self {
            windows: DashMap::new(),
            window: Duration::from_secs(1),
            max_per_window: max_per_second as usize,
        }
    }

    /// Returns `true` when the request is within budget (allowed).
    /// Returns `false` when the IP is rate-limited.
    pub fn check(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let cutoff = now - self.window;

        let mut entry = self.windows.entry(ip).or_default();
        // Evict timestamps outside the window.
        while entry.front().map(|&t| t < cutoff).unwrap_or(false) {
            entry.pop_front();
        }

        if entry.len() >= self.max_per_window {
            return false;
        }

        entry.push_back(now);
        true
    }

    /// Evict entries for IPs that have been idle (no requests) for longer than
    /// the window.  Call periodically to avoid unbounded memory growth.
    pub fn evict_idle(&self) {
        let cutoff = Instant::now() - self.window;
        self.windows
            .retain(|_, deque| deque.back().map(|&t| t >= cutoff).unwrap_or(false));
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(1, 2, 3, n))
    }

    #[test]
    fn allows_up_to_max_per_second() {
        let limiter = IpRateLimiter::new(3);
        assert!(limiter.check(ip(1)));
        assert!(limiter.check(ip(1)));
        assert!(limiter.check(ip(1)));
        assert!(!limiter.check(ip(1)));
    }

    #[test]
    fn independent_ips_do_not_interfere() {
        let limiter = IpRateLimiter::new(1);
        assert!(limiter.check(ip(1)));
        assert!(!limiter.check(ip(1)));
        // Different IP is unaffected.
        assert!(limiter.check(ip(2)));
    }
}
