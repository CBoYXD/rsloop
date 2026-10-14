//! Mutex-protected state with a coherent, lock-free snapshot of hot flags.
use std::{
    ops::{Deref, DerefMut},
    sync::{
        Condvar, LockResult, Mutex, MutexGuard, PoisonError, WaitTimeoutResult,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

pub trait SnapshotFlags {
    fn snapshot_flags(&self) -> u8;
}

pub struct SnapshotMutex<T: SnapshotFlags> {
    inner: Mutex<T>,
    flags: AtomicU8,
}

pub struct SnapshotGuard<'a, T: SnapshotFlags> {
    owner: &'a SnapshotMutex<T>,
    guard: Option<MutexGuard<'a, T>>,
    dirty: bool,
}

impl<T: SnapshotFlags> SnapshotMutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            flags: AtomicU8::new(value.snapshot_flags()),
            inner: Mutex::new(value),
        }
    }

    #[inline]
    pub fn flags(&self) -> u8 {
        self.flags.load(Ordering::Acquire)
    }

    pub fn lock(&self) -> LockResult<SnapshotGuard<'_, T>> {
        self.inner
            .lock()
            .map(|guard| self.wrap(guard))
            .map_err(|error| PoisonError::new(self.wrap(error.into_inner())))
    }

    fn wrap<'a>(&'a self, guard: MutexGuard<'a, T>) -> SnapshotGuard<'a, T> {
        SnapshotGuard {
            owner: self,
            guard: Some(guard),
            dirty: false,
        }
    }

    pub fn wait_timeout<'a>(
        &'a self,
        cv: &Condvar,
        mut guard: SnapshotGuard<'a, T>,
        duration: Duration,
    ) -> LockResult<(SnapshotGuard<'a, T>, WaitTimeoutResult)> {
        guard.publish();
        let inner = guard.guard.take().expect("live snapshot guard");
        cv.wait_timeout(inner, duration)
            .map(|(inner, timeout)| (self.wrap(inner), timeout))
            .map_err(|error| {
                let (inner, timeout) = error.into_inner();
                PoisonError::new((self.wrap(inner), timeout))
            })
    }

    #[cfg(test)]
    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}

impl<T: SnapshotFlags> SnapshotGuard<'_, T> {
    fn publish(&mut self) {
        if self.dirty {
            let flags = self
                .guard
                .as_ref()
                .expect("live snapshot guard")
                .snapshot_flags();
            if self.owner.flags.load(Ordering::Relaxed) != flags {
                self.owner.flags.store(flags, Ordering::Release);
            }
            self.dirty = false;
        }
    }
}

impl<T: SnapshotFlags> Deref for SnapshotGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().expect("live snapshot guard")
    }
}
impl<T: SnapshotFlags> DerefMut for SnapshotGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        self.guard.as_mut().expect("live snapshot guard")
    }
}
impl<T: SnapshotFlags> Drop for SnapshotGuard<'_, T> {
    fn drop(&mut self) {
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct State(u8);
    impl SnapshotFlags for State {
        fn snapshot_flags(&self) -> u8 {
            self.0
        }
    }

    #[test]
    fn mutation_and_unwind_publish_flags_before_unlocking() {
        let state = SnapshotMutex::new(State(0));
        {
            state.lock().unwrap().0 = 3;
        }
        assert_eq!(state.flags(), 3);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.lock().unwrap().0 = 5;
            let mut guard = state.lock().unwrap();
            guard.0 = 7;
            panic!("state update panic");
        }));
        assert!(panic.is_err());
        assert_eq!(state.flags(), 7);
        assert!(state.is_poisoned());
    }
}
