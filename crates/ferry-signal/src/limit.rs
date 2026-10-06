//! Token buckets and per-IP-group limits.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use crate::config::Limits;
use crate::net::IpGroup;

/// Upper bound on tracked IP groups; beyond it, idle state is purged first
/// and new groups are refused if that does not help.
const MAX_TRACKED_GROUPS: usize = 100_000;

/// A classic token bucket.
#[derive(Clone, Debug)]
pub(crate) struct TokenBucket {
    capacity: f64,
    rate_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub(crate) fn new(rate_per_sec: f64, burst: f64) -> Self {
        let capacity = burst.max(1.0);
        Self { capacity, rate_per_sec: rate_per_sec.max(0.0), tokens: capacity, last: Instant::now() }
    }

    pub(crate) fn per_minute(count: u32) -> Self {
        Self::new(f64::from(count) / 60.0, f64::from(count))
    }

    pub(crate) fn try_take(&mut self) -> bool {
        self.try_take_at(Instant::now())
    }

    fn try_take_at(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn is_full_at(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.capacity
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.capacity);
        self.last = now;
    }
}

/// Per-IP-group request buckets and concurrent connection counts.
pub(crate) struct GroupLimiter {
    state: Mutex<LimiterState>,
    max_conns_per_group: u32,
    max_conns: u32,
    attempt_rate: f64,
    attempt_burst: f64,
    code_joins_per_minute: u32,
}

#[derive(Default)]
struct LimiterState {
    /// HTTP requests (`/v1/ws`, `/v1/turn`).
    attempts: HashMap<IpGroup, TokenBucket>,
    /// Short-code room joins.
    code_joins: HashMap<IpGroup, TokenBucket>,
    conns: HashMap<IpGroup, u32>,
    total: u32,
}

impl LimiterState {
    fn purge(&mut self, now: Instant) {
        self.attempts.retain(|_, bucket| !bucket.is_full_at(now));
        self.code_joins.retain(|_, bucket| !bucket.is_full_at(now));
    }
}

/// Why [`GroupLimiter::acquire`] refused a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotError {
    /// The IP group has too many open connections.
    Group,
    /// The server has too many open connections.
    Server,
}

impl GroupLimiter {
    pub(crate) fn new(limits: &Limits) -> Self {
        Self {
            state: Mutex::default(),
            max_conns_per_group: limits.max_conns_per_group,
            max_conns: limits.max_conns,
            attempt_rate: limits.conn_attempts_per_sec,
            attempt_burst: limits.conn_attempt_burst,
            code_joins_per_minute: limits.code_joins_per_minute,
        }
    }

    fn lock(&self) -> MutexGuard<'_, LimiterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one HTTP request from `group`; `false` means rate limited.
    pub(crate) fn allow_attempt(&self, group: IpGroup) -> bool {
        let bucket = || TokenBucket::new(self.attempt_rate, self.attempt_burst);
        take(&mut self.lock(), |s| &mut s.attempts, group, bucket)
    }

    /// Counts one short-code `ROOM_JOIN` from `group`; `false` means rate limited.
    pub(crate) fn allow_code_join(&self, group: IpGroup) -> bool {
        let bucket = || TokenBucket::per_minute(self.code_joins_per_minute);
        take(&mut self.lock(), |s| &mut s.code_joins, group, bucket)
    }

    /// Reserves a connection slot for `group`, released when the slot drops.
    pub(crate) fn acquire(self: &Arc<Self>, group: IpGroup) -> Result<ConnSlot, SlotError> {
        let mut state = self.lock();
        if state.total >= self.max_conns {
            return Err(SlotError::Server);
        }
        let count = state.conns.entry(group).or_insert(0);
        if *count >= self.max_conns_per_group {
            if *count == 0 {
                state.conns.remove(&group);
            }
            return Err(SlotError::Group);
        }
        *count += 1;
        state.total += 1;
        Ok(ConnSlot { limiter: Arc::clone(self), group })
    }

    /// Forgets request buckets that have fully refilled.
    pub(crate) fn purge(&self) {
        self.lock().purge(Instant::now());
    }
}

/// A reserved connection slot (see [`GroupLimiter::acquire`]).
pub(crate) struct ConnSlot {
    limiter: Arc<GroupLimiter>,
    group: IpGroup,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut state = self.limiter.lock();
        state.total = state.total.saturating_sub(1);
        if let Entry::Occupied(mut count) = state.conns.entry(self.group) {
            *count.get_mut() = count.get().saturating_sub(1);
            if *count.get() == 0 {
                count.remove();
            }
        }
    }
}

/// Takes a token from `group`'s bucket in the map that `map` selects. The
/// number of tracked groups is bounded: when a map is full, idle buckets are
/// purged first and new groups are refused if that does not help.
fn take(
    state: &mut LimiterState,
    map: fn(&mut LimiterState) -> &mut HashMap<IpGroup, TokenBucket>,
    group: IpGroup,
    new_bucket: impl FnOnce() -> TokenBucket,
) -> bool {
    let now = Instant::now();
    let buckets = map(state);
    if !buckets.contains_key(&group) && buckets.len() >= MAX_TRACKED_GROUPS {
        state.purge(now);
        if map(state).len() >= MAX_TRACKED_GROUPS {
            return false;
        }
    }
    map(state).entry(group).or_insert_with(new_bucket).try_take_at(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    #[test]
    fn bucket_allows_burst_then_refills() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(2.0, 3.0);
        bucket.last = start;
        assert!(bucket.try_take_at(start));
        assert!(bucket.try_take_at(start));
        assert!(bucket.try_take_at(start));
        assert!(!bucket.try_take_at(start));
        assert!(bucket.try_take_at(start + Duration::from_millis(500)));
        assert!(!bucket.try_take_at(start + Duration::from_millis(500)));
        assert!(bucket.is_full_at(start + Duration::from_secs(10)));
        assert!(bucket.try_take_at(start + Duration::from_secs(10)));
        assert!(!bucket.is_full_at(start + Duration::from_secs(10)));
    }

    #[test]
    fn per_minute_bucket() {
        let start = Instant::now();
        let mut bucket = TokenBucket::per_minute(10);
        bucket.last = start;
        for _ in 0..10 {
            assert!(bucket.try_take_at(start));
        }
        assert!(!bucket.try_take_at(start + Duration::from_secs(5)));
        assert!(bucket.try_take_at(start + Duration::from_secs(7)));
    }

    #[test]
    fn connection_slots_are_counted_and_released() {
        let limits = Limits { max_conns_per_group: 2, max_conns: 3, ..Limits::default() };
        let limiter = Arc::new(GroupLimiter::new(&limits));
        let a = IpGroup::V4(Ipv4Addr::new(192, 0, 2, 1));
        let b = IpGroup::V4(Ipv4Addr::new(192, 0, 2, 2));
        let c = IpGroup::V4(Ipv4Addr::new(192, 0, 2, 3));
        let s1 = limiter.acquire(a).unwrap();
        let _s2 = limiter.acquire(a).unwrap();
        assert_eq!(limiter.acquire(a).err(), Some(SlotError::Group));
        let _s3 = limiter.acquire(b).unwrap();
        assert_eq!(limiter.acquire(c).err(), Some(SlotError::Server));
        drop(s1);
        let _s4 = limiter.acquire(a).unwrap();
        assert_eq!(limiter.lock().total, 3);
    }

    #[test]
    fn code_joins_are_limited_per_group() {
        let limits = Limits { code_joins_per_minute: 2, ..Limits::default() };
        let limiter = GroupLimiter::new(&limits);
        let a = IpGroup::V4(Ipv4Addr::new(192, 0, 2, 1));
        let b = IpGroup::V4(Ipv4Addr::new(192, 0, 2, 2));
        assert!(limiter.allow_code_join(a));
        assert!(limiter.allow_code_join(a));
        assert!(!limiter.allow_code_join(a));
        assert!(limiter.allow_code_join(b));
        // Independent of the HTTP request buckets.
        assert!(limiter.allow_attempt(a));
    }

    #[test]
    fn attempts_are_limited_per_group_and_purged() {
        let limits = Limits { conn_attempts_per_sec: 0.0, conn_attempt_burst: 2.0, ..Limits::default() };
        let limiter = GroupLimiter::new(&limits);
        let a = IpGroup::V4(Ipv4Addr::new(192, 0, 2, 1));
        let b = IpGroup::V6([0x2001, 0xdb8, 0, 1]);
        assert!(limiter.allow_attempt(a));
        assert!(limiter.allow_attempt(a));
        assert!(!limiter.allow_attempt(a));
        assert!(limiter.allow_attempt(b));
        limiter.purge();
        // `a` is empty and `b` partially used: neither is full, both stay.
        assert_eq!(limiter.lock().attempts.len(), 2);
    }
}
