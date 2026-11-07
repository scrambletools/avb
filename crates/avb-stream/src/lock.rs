//! The locks the stream threads share with the program's other threads.
//! On Linux a thread waiting on one lends its priority to the holder, as
//! a real-time talker does to an audio thread it shares a buffer with:
//! the holder, preempted while holding it, runs again at once instead of
//! keeping the stream waiting for its next turn on a CPU. Elsewhere they
//! are the standard library's.

#[cfg(target_os = "linux")]
pub(crate) use inheriting::{Guard, Lock};

#[cfg(not(target_os = "linux"))]
pub(crate) use plain::{Guard, Lock};

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod inheriting {
    use std::cell::UnsafeCell;
    use std::io;
    use std::marker::PhantomData;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::{AtomicU32, Ordering};

    thread_local! {
        // SAFETY: gettid has no arguments and cannot fail.
        static THREAD: u32 = unsafe { libc::gettid() } as u32;
    }

    /// A value behind a priority-inheriting futex: its word is 0 while
    /// free, the holder's thread id while held, with the kernel's bit for
    /// others waiting on it.
    pub(crate) struct Lock<T> {
        word: AtomicU32,
        value: UnsafeCell<T>,
    }

    // SAFETY: the value is reached only through a guard, which one thread
    // at a time holds.
    unsafe impl<T: Send> Send for Lock<T> {}
    // SAFETY: as above.
    unsafe impl<T: Send> Sync for Lock<T> {}

    /// The value while held; dropping it lets the lock go. It stays on
    /// the thread that took it, which the kernel knows as the holder.
    pub(crate) struct Guard<'a, T> {
        lock: &'a Lock<T>,
        _thread: PhantomData<*const ()>,
    }

    impl<T: Default> Default for Lock<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T> Lock<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self {
                word: AtomicU32::new(0),
                value: UnsafeCell::new(value),
            }
        }

        /// Waits for the lock, the holder running at this thread's
        /// priority meanwhile where that is higher than its own.
        pub(crate) fn lock(&self) -> Guard<'_, T> {
            let thread = THREAD.with(|thread| *thread);
            if self
                .word
                .compare_exchange(0, thread, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                loop {
                    if futex(&self.word, libc::FUTEX_LOCK_PI) == 0 {
                        break;
                    }
                    let error = io::Error::last_os_error();
                    // The holder was exiting, or a signal came: try again.
                    if !matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
                        panic!("could not take a lock: {error}");
                    }
                }
            }
            Guard {
                lock: self,
                _thread: PhantomData,
            }
        }
    }

    impl<T> Drop for Guard<'_, T> {
        fn drop(&mut self) {
            let thread = THREAD.with(|thread| *thread);
            // With others waiting, the kernel hands the lock to the
            // highest priority of them.
            if self
                .lock
                .word
                .compare_exchange(thread, 0, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                futex(&self.lock.word, libc::FUTEX_UNLOCK_PI);
            }
        }
    }

    impl<T> Deref for Guard<'_, T> {
        type Target = T;

        fn deref(&self) -> &T {
            // SAFETY: this guard holds the lock.
            unsafe { &*self.lock.value.get() }
        }
    }

    impl<T> DerefMut for Guard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: this guard holds the lock.
            unsafe { &mut *self.lock.value.get() }
        }
    }

    /// One of the priority-inheriting futex operations on `word`, private
    /// to this process.
    fn futex(word: &AtomicU32, operation: libc::c_int) -> libc::c_long {
        // SAFETY: `word` is a live, aligned futex word, and these
        // operations take no timeout and no second word.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                operation | libc::FUTEX_PRIVATE_FLAG,
                0,
                std::ptr::null::<libc::timespec>(),
            )
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod plain {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    pub(crate) type Guard<'a, T> = MutexGuard<'a, T>;

    #[derive(Default)]
    pub(crate) struct Lock<T>(Mutex<T>);

    impl<T> Lock<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self(Mutex::new(value))
        }

        pub(crate) fn lock(&self) -> Guard<'_, T> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_take_turns() {
        let count = Lock::new(0u64);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..100_000 {
                        *count.lock() += 1;
                    }
                });
            }
        });
        assert_eq!(*count.lock(), 400_000);
    }

    #[test]
    fn a_waiting_thread_takes_it_once_let_go() {
        let held = Lock::new(Vec::new());
        let guard = held.lock();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| held.lock().push(2));
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut guard = guard;
            guard.push(1);
            drop(guard);
            waiting.join().unwrap();
        });
        assert_eq!(*held.lock(), [1, 2]);
    }
}
