//! Abuse limits shared by the server and discovery. All tables are bounded.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The unit limits are applied to: an IPv4 address, or an IPv6 /64 (one
/// device can trivially rotate through addresses inside its prefix).
pub fn peer_key(ip: IpAddr) -> IpAddr {
    match super::interfaces::canonical(ip) {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// Caps concurrent connections per peer and in total.
pub struct ConnectionLimiter {
    per_peer: usize,
    total: usize,
    state: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

pub struct ConnectionPermit {
    limiter: Arc<ConnectionLimiter>,
    key: IpAddr,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut state = self.limiter.state.lock().unwrap();
        state.0 -= 1;
        if let Some(n) = state.1.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                state.1.remove(&self.key);
            }
        }
    }
}

impl ConnectionLimiter {
    pub fn new(per_peer: usize, total: usize) -> Arc<Self> {
        Arc::new(Self { per_peer, total, state: Mutex::new((0, HashMap::new())) })
    }

    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ConnectionPermit> {
        let key = peer_key(ip);
        let mut state = self.state.lock().unwrap();
        if state.0 >= self.total {
            return None;
        }
        let n = state.1.entry(key).or_insert(0);
        if *n >= self.per_peer {
            return None;
        }
        *n += 1;
        state.0 += 1;
        Some(ConnectionPermit { limiter: self.clone(), key })
    }
}

/// Token-bucket rate limiter per key.
pub struct RateLimiter {
    rate_per_sec: f64,
    burst: f64,
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

const MAX_TRACKED: usize = 4096;

impl RateLimiter {
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self { rate_per_sec, burst, buckets: Mutex::new(HashMap::new()) }
    }

    /// Takes one token for `ip`; `false` means "slow down".
    pub fn check(&self, ip: IpAddr) -> bool {
        let key = peer_key(ip);
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap();
        if buckets.len() >= MAX_TRACKED && !buckets.contains_key(&key) {
            // Drop buckets that have refilled completely (they carry no state).
            let full_after = Duration::from_secs_f64(self.burst / self.rate_per_sec);
            buckets.retain(|_, (_, at)| now.duration_since(*at) < full_after);
            if buckets.len() >= MAX_TRACKED {
                return false;
            }
        }
        let (tokens, at) = buckets.entry(key).or_insert((self.burst, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * self.rate_per_sec).min(self.burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Failed-PIN tracking: a few attempts per peer, a global ceiling against
/// distributed guessing, and lockouts that expire (upstream locks out until
/// restart and never forgets).
pub struct FailureTracker {
    per_peer: u32,
    global: u32,
    window: Duration,
    state: Mutex<FailureState>,
}

struct FailureState {
    peers: HashMap<IpAddr, (u32, Instant)>,
    global: (u32, Instant),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Attempt {
    Allowed,
    LockedOut,
}

impl FailureTracker {
    pub fn new(per_peer: u32, global: u32, window: Duration) -> Self {
        Self { per_peer, global, window, state: Mutex::new(FailureState { peers: HashMap::new(), global: (0, Instant::now()) }) }
    }

    pub fn check(&self, ip: IpAddr) -> Attempt {
        let key = peer_key(ip);
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        if now.duration_since(state.global.1) > self.window {
            state.global = (0, now);
        }
        if state.global.0 >= self.global {
            return Attempt::LockedOut;
        }
        match state.peers.get(&key) {
            Some((n, at)) if *n >= self.per_peer && now.duration_since(*at) <= self.window => Attempt::LockedOut,
            _ => Attempt::Allowed,
        }
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let key = peer_key(ip);
        let now = Instant::now();
        let window = self.window;
        let mut state = self.state.lock().unwrap();
        if state.peers.len() >= MAX_TRACKED {
            state.peers.retain(|_, (_, at)| now.duration_since(*at) <= window);
        }
        let entry = state.peers.entry(key).or_insert((0, now));
        if now.duration_since(entry.1) > window {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.1 = now;
        state.global.0 += 1;
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.state.lock().unwrap().peers.remove(&peer_key(ip));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_peers_are_grouped_by_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(peer_key(a), peer_key(b));
        assert_ne!(peer_key(a), peer_key(c));
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert_eq!(peer_key(mapped), "10.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn connection_limits_per_peer_and_total() {
        let limiter = ConnectionLimiter::new(2, 3);
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let p1 = limiter.try_acquire(a).unwrap();
        let _p2 = limiter.try_acquire(a).unwrap();
        assert!(limiter.try_acquire(a).is_none());
        let _p3 = limiter.try_acquire(b).unwrap();
        assert!(limiter.try_acquire(b).is_none()); // total reached
        drop(p1);
        assert!(limiter.try_acquire(b).is_some());
    }

    #[test]
    fn rate_limiter_allows_burst_then_throttles() {
        let limiter = RateLimiter::new(1.0, 3.0);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(limiter.check(ip));
        assert!(limiter.check(ip));
        assert!(limiter.check(ip));
        assert!(!limiter.check(ip));
        assert!(limiter.check("10.0.0.2".parse().unwrap()));
    }

    #[test]
    fn pin_lockout_is_per_peer_and_expires() {
        let tracker = FailureTracker::new(3, 100, Duration::from_millis(50));
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..3 {
            assert_eq!(tracker.check(ip), Attempt::Allowed);
            tracker.record_failure(ip);
        }
        assert_eq!(tracker.check(ip), Attempt::LockedOut);
        assert_eq!(tracker.check("10.0.0.2".parse().unwrap()), Attempt::Allowed);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(tracker.check(ip), Attempt::Allowed);
    }

    #[test]
    fn global_pin_ceiling_stops_distributed_guessing() {
        let tracker = FailureTracker::new(3, 5, Duration::from_secs(60));
        for i in 0..5 {
            tracker.record_failure(format!("10.0.0.{i}").parse().unwrap());
        }
        assert_eq!(tracker.check("10.0.0.99".parse().unwrap()), Attempt::LockedOut);
    }
}
