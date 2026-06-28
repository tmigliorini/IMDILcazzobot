use std::sync::Arc;
use std::time::{Duration, Instant};
use derive_more::Display;
use flurry::HashMap;
use crate::config::FeatureToggles;

use crate::handlers::utils::callbacks::CallbackDataWithPrefix;

// TODO: create a Redis based implementation
pub trait LockCallbackServiceImplTrait : Clone + Send + Sync {
    type Guard;

    fn try_lock<T>(&mut self, callback_data: &T) -> Option<Self::Guard>
    where Self::Guard: Guard,
          T: CallbackDataWithPrefix;
}

pub trait Guard: Send + Sync {}

#[derive(Clone)]
pub enum LockCallbackServiceFacade {
    NoOp,
    InMemory(InMemoryLockCallbackService),
}

impl LockCallbackServiceFacade {
    pub fn from_config(features: FeatureToggles) -> Self {
        if features.pvp.callback_locks {
            log::info!("LockCallbackService: in-memory");
            Self::InMemory(InMemoryLockCallbackService::default())
        } else {
            log::info!("LockCallbackService: none");
            Self::NoOp
        }
    }

    pub fn try_lock<T>(&mut self, callback_data: &T) -> Option<Box<dyn Guard>>
    where T: CallbackDataWithPrefix,
    {
        match self {
            Self::NoOp => Some(Box::<NoOpGuard>::default()),
            Self::InMemory(service) => service.try_lock(callback_data)
                .map(|guard| Box::new(guard) as Box<dyn Guard>),
        }
    }
}

#[derive(Default)]
pub struct NoOpGuard {}
impl Guard for NoOpGuard {}

/// How long a lock may be held before it's considered abandoned and a new attempt is let through
/// anyway. Guards normally self-release within milliseconds (their `Drop` runs as soon as the
/// callback handler holding them returns, success or error alike) - this only ever matters if a
/// handler gets stuck for real (e.g. a hung database call that never times out), which would
/// otherwise leave that one specific button permanently unusable until the process restarts.
const LOCK_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub struct InMemoryLockCallbackService {
    inner_map: Arc<HashMap<String, Instant>>
}

impl LockCallbackServiceImplTrait for InMemoryLockCallbackService {
    type Guard = InMemorySetGuard;

    fn try_lock<T>(&mut self, callback_data: &T) -> Option<Self::Guard>
    where Self::Guard: Guard,
          T: CallbackDataWithPrefix
    {
        self.try_lock_with_ttl(callback_data, LOCK_TTL)
    }
}

impl InMemoryLockCallbackService {
    /// The actual logic behind `try_lock`, with the TTL as a parameter so tests can use a tiny
    /// one instead of waiting out the real `LOCK_TTL`.
    fn try_lock_with_ttl<T>(&mut self, callback_data: &T, ttl: Duration) -> Option<InMemorySetGuard>
    where T: CallbackDataWithPrefix
    {
        let key = callback_data.to_string();
        let guard = self.inner_map.guard();
        let now = Instant::now();
        if let Some(&locked_at) = self.inner_map.get(&key, &guard) {
            if now.duration_since(locked_at) < ttl {
                log::debug!("double attack on: {key}");
                return None
            }
            log::warn!("a stale lock for '{key}' outlived its {ttl:?} TTL (likely a hung handler that never released it) - allowing a new attempt");
        }
        self.inner_map.insert(key.clone(), now, &guard);
        Some(InMemorySetGuard::new(&self.inner_map, key, now))
    }
}

#[derive(Debug, Display, Clone)]
#[display("InMemorySetGuard({key})")]
pub struct InMemorySetGuard {
    map_ref: Arc<HashMap<String, Instant>>,
    key: String,
    locked_at: Instant,
}

impl InMemorySetGuard {
    pub fn new(map_ref: &Arc<HashMap<String, Instant>>, key: String, locked_at: Instant) -> Self {
        let map_ref = Arc::clone(map_ref);
        let guard = Self { map_ref, key, locked_at };
        log::debug!("taking a lock guard: {guard}");
        guard
    }
}

impl Drop for InMemorySetGuard {
    fn drop(&mut self) {
        log::debug!("dropping the lock guard: {self}");
        let guard = self.map_ref.guard();
        // only clear the entry if it's still the one *this* guard created - if it already
        // outlived the TTL and got re-acquired by someone else, this (very late) drop must not
        // clear out that newer, still-valid lock.
        if self.map_ref.get(&self.key, &guard) == Some(&self.locked_at) {
            self.map_ref.remove(&self.key, &guard);
        }
    }
}

impl Guard for InMemorySetGuard {}

#[cfg(test)]
mod test {
    use std::thread::sleep;
    use std::time::Duration;
    use derive_more::Display;
    use crate::handlers::utils::callbacks::{CallbackDataWithPrefix, InvalidCallbackData};
    use super::InMemoryLockCallbackService;

    #[derive(Display)]
    #[display("key")]
    struct DummyCallbackData;

    impl CallbackDataWithPrefix for DummyCallbackData {
        fn prefix() -> &'static str { "dummy" }
    }

    impl TryFrom<String> for DummyCallbackData {
        type Error = InvalidCallbackData;

        fn try_from(_data: String) -> Result<Self, Self::Error> {
            Ok(Self)
        }
    }

    #[test]
    fn a_second_attempt_on_the_same_key_is_rejected_while_the_first_guard_is_held() {
        let mut service = InMemoryLockCallbackService::default();
        let first = service.try_lock_with_ttl(&DummyCallbackData, Duration::from_secs(30));
        assert!(first.is_some(), "the first attempt must succeed");
        assert!(service.try_lock_with_ttl(&DummyCallbackData, Duration::from_secs(30)).is_none(),
            "a concurrent attempt on the same key must be rejected while the guard is alive");
    }

    #[test]
    fn dropping_the_guard_releases_the_key_immediately() {
        let mut service = InMemoryLockCallbackService::default();
        let first = service.try_lock_with_ttl(&DummyCallbackData, Duration::from_secs(30));
        drop(first);
        assert!(service.try_lock_with_ttl(&DummyCallbackData, Duration::from_secs(30)).is_some(),
            "once the guard is dropped, a new attempt on the same key must succeed");
    }

    #[test]
    fn a_lock_older_than_its_ttl_is_treated_as_abandoned() {
        let mut service = InMemoryLockCallbackService::default();
        let ttl = Duration::from_millis(20);
        let first = service.try_lock_with_ttl(&DummyCallbackData, ttl);
        assert!(first.is_some());
        // simulate a hung handler: the guard is still alive (not dropped), but enough time has
        // passed that the lock must be treated as abandoned.
        sleep(ttl * 3);
        assert!(service.try_lock_with_ttl(&DummyCallbackData, ttl).is_some(),
            "a stale lock past its TTL must let a new attempt through, even if the original guard is still alive");
    }

    #[test]
    fn a_very_late_drop_of_an_expired_guard_does_not_clear_a_newer_lock() {
        let mut service = InMemoryLockCallbackService::default();
        let ttl = Duration::from_millis(20);
        let first = service.try_lock_with_ttl(&DummyCallbackData, ttl).expect("the first attempt must succeed");
        sleep(ttl * 3);
        let second = service.try_lock_with_ttl(&DummyCallbackData, ttl).expect("the stale lock must let a new attempt through");
        // the first guard finally gets dropped late, well after the second one took over.
        drop(first);
        assert!(service.try_lock_with_ttl(&DummyCallbackData, ttl).is_none(),
            "the late drop of the expired first guard must not clear the second, still-valid lock");
        drop(second);
    }
}
