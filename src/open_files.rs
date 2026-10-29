//! How many files the Server process may hold open at once.
//!
//! A Server holds pipes to every Provider process it runs and spawns Git for
//! every reading of a checkout, so the soft `RLIMIT_NOFILE` it inherits — 256
//! on macOS — is soon reached, and from then on nothing it spawns can start.
//! The Server raises its soft limit to its hard limit as it starts. Windows
//! has no such per-process limit to raise.

/// Raises the process's soft limit on open files as far as it may go, and
/// logs where it stands. A limit that cannot be read or raised is logged and
/// otherwise left as it was.
pub fn raise_limit() {
    #[cfg(unix)]
    match unix::raise() {
        Ok(Raised { from, to }) if from == to => {
            tracing::info!(limit = to, "Open file limit already at its ceiling")
        }
        Ok(Raised { from, to }) => tracing::info!(from, to, "Raised the open file limit"),
        Err(error) => tracing::warn!("Open file limit could not be raised: {error}"),
    }
}

/// The soft limit on open files before and after it was raised.
#[cfg(unix)]
struct Raised {
    from: u64,
    to: u64,
}

/// The soft limit to set given the current `soft` and `hard` limits and the
/// platform's own `ceiling` on any one process's, never lower than `soft`.
#[cfg(any(unix, test))]
fn raised_soft_limit(soft: u64, hard: u64, ceiling: Option<u64>) -> u64 {
    let target = ceiling.map_or(hard, |ceiling| hard.min(ceiling));
    target.max(soft)
}

#[cfg(unix)]
mod unix {
    use super::{Raised, raised_soft_limit};

    pub(super) fn raise() -> std::io::Result<Raised> {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is a valid `rlimit` for the call to fill in.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let from = limit.rlim_cur;
        let to = raised_soft_limit(from, limit.rlim_max, ceiling());
        if to != from {
            limit.rlim_cur = to;
            // SAFETY: `limit` is a valid `rlimit`, its soft limit within its
            // hard one.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Raised { from, to })
    }

    /// `OPEN_MAX` from macOS's `<sys/syslimits.h>`, which `libc` leaves out.
    #[cfg(target_os = "macos")]
    const OPEN_MAX: u64 = 10_240;

    /// macOS refuses a soft limit past `kern.maxfilesperproc` — so an
    /// unlimited hard limit cannot simply be copied — and `OPEN_MAX` stands
    /// in where that cannot be read.
    #[cfg(target_os = "macos")]
    fn ceiling() -> Option<u64> {
        let mut most: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: the name is NUL-terminated, and `most` and `size` describe
        // a buffer of the `int` this sysctl answers with.
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut most).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        Some(match u64::try_from(most) {
            Ok(most) if read == 0 && most > 0 => most,
            _ => OPEN_MAX,
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn ceiling() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::raised_soft_limit;

    #[test]
    fn the_soft_limit_rises_to_the_hard_limit() {
        assert_eq!(raised_soft_limit(256, 4096, None), 4096);
    }

    #[test]
    fn a_platform_ceiling_bounds_an_unlimited_hard_limit() {
        assert_eq!(raised_soft_limit(256, u64::MAX, Some(10_240)), 10_240);
        assert_eq!(
            raised_soft_limit(256, 2048, Some(10_240)),
            2048,
            "a hard limit beneath the ceiling still bounds it"
        );
    }

    #[test]
    fn a_soft_limit_is_never_lowered() {
        assert_eq!(raised_soft_limit(8192, 8192, Some(4096)), 8192);
        assert_eq!(raised_soft_limit(1024, 1024, None), 1024);
    }

    /// The process's own limit, raised for real, stands at or past where it
    /// was and within its hard limit.
    #[cfg(unix)]
    #[test]
    fn raising_the_process_limit_leaves_it_within_its_hard_limit() {
        let raised = super::unix::raise().expect("raise the open file limit");
        assert!(raised.to >= raised.from);
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is a valid `rlimit` for the call to fill in.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        assert_eq!(limit.rlim_cur, raised.to);
        assert!(limit.rlim_cur <= limit.rlim_max);
    }
}
