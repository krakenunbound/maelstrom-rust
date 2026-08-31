//! Keep compute-worker wakeups from taking interactive thread priority on Windows.

#[cfg(windows)]
pub(super) fn configure_monitor_worker() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriorityBoost};

    // Keep the existing base priority. These threads do sustained video computation, not
    // input handling. Automatic wake boosts can preempt the thread still submitting layers.
    // SAFETY: the pseudo-handle belongs to this live worker; no handle is transferred or closed.
    if unsafe { SetThreadPriorityBoost(GetCurrentThread(), 1) } == 0 {
        let error = std::io::Error::last_os_error();
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(%error, "could not disable monitor worker wake priority boosts; retaining Windows defaults");
        }
    }
}

#[cfg(not(windows))]
pub(super) fn configure_monitor_worker() {}

#[cfg(all(test, windows))]
pub(super) fn monitor_worker_policy(
    worker: &std::thread::JoinHandle<()>,
) -> std::io::Result<(bool, i32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::{GetThreadPriority, GetThreadPriorityBoost};
    let mut disabled = 0;
    // SAFETY: the borrowed join handle remains live for both queries; output storage is valid.
    unsafe {
        if GetThreadPriorityBoost(worker.as_raw_handle(), &mut disabled) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((disabled != 0, GetThreadPriority(worker.as_raw_handle())))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, GetPriorityClass, GetThreadPriority,
        GetThreadPriorityBoost, SetThreadPriority, SetThreadPriorityBoost,
        THREAD_PRIORITY_BELOW_NORMAL,
    };

    fn current_boost_disabled() -> bool {
        let mut disabled = 0;
        // SAFETY: this thread is live and the output integer is writable.
        assert_ne!(
            unsafe { GetThreadPriorityBoost(GetCurrentThread(), &mut disabled) },
            0
        );
        disabled != 0
    }

    #[test]
    fn worker_policy_is_local_idempotent_and_preserves_base_priority() {
        let caller_boost_disabled = current_boost_disabled();
        // SAFETY: current-process pseudo-handle is live and borrowed, not closed.
        let process_priority = unsafe { GetPriorityClass(GetCurrentProcess()) };
        std::thread::spawn(|| {
            // Change only this temporary test thread so preservation is not a default-value test.
            // SAFETY: both setters target this live test thread's pseudo-handle.
            unsafe {
                assert_ne!(
                    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL),
                    0
                );
                assert_ne!(SetThreadPriorityBoost(GetCurrentThread(), 0), 0);
            }
            assert!(!current_boost_disabled());
            for _ in 0..2 {
                configure_monitor_worker();
                assert!(current_boost_disabled());
                // SAFETY: query of this live test thread's pseudo-handle.
                assert_eq!(
                    unsafe { GetThreadPriority(GetCurrentThread()) },
                    THREAD_PRIORITY_BELOW_NORMAL
                );
            }
        })
        .join()
        .unwrap();
        assert_eq!(current_boost_disabled(), caller_boost_disabled);
        // SAFETY: query of the unchanged live process.
        assert_eq!(
            unsafe { GetPriorityClass(GetCurrentProcess()) },
            process_priority
        );
    }
}
