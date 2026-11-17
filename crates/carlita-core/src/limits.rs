//! The open file limit Carlita and the programs it starts run with.
//!
//! Desktops commonly start programs with a soft limit of 1024 open files and
//! a far higher hard limit. Plugins run through yabridge and Wine can need
//! more than 1024: XLN Audio's plugins failed to start with "Too many open
//! files". DAWs such as REAPER and Ardour raise the soft limit to the hard
//! one at start, and so does Carlita; everything it starts (the plugin
//! scanner, Carla's plugin bridges, yabridge, Wine) inherits it.

/// Raises the soft open file limit to the hard limit. Returns the limit in
/// effect afterwards.
///
/// Only makes system calls, so it is safe to call between `fork` and `exec`.
pub fn raise_open_file_limit() -> Option<u64> {
    // SAFETY: getrlimit/setrlimit only read and write the struct passed.
    unsafe {
        let mut lim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return None;
        }
        if lim.rlim_cur < lim.rlim_max {
            let raised = libc::rlimit {
                rlim_cur: lim.rlim_max,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                lim = raised;
            }
        }
        Some(lim.rlim_cur as u64)
    }
}

/// Marks every file descriptor above stderr close-on-exec, so a program
/// about to be started does not inherit descriptors the libraries in this
/// process (JACK, ALSA, X11...) opened without that flag. Each one counts
/// against the started program's open file limit, and is passed on to
/// whatever it starts in turn (yabridge, Wine).
///
/// Only makes system calls, so it is safe to call between `fork` and `exec`.
/// Descriptors already close-on-exec (such as the one Rust's process
/// spawning reports `exec` errors through) are unaffected.
pub fn close_inherited_fds_on_exec() {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    // SAFETY: close_range and fcntl only change descriptor flags.
    unsafe {
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC) == 0 {
            return;
        }
        // Kernels before 5.11: go through the descriptors one by one, up
        // to the limit they could have been opened under.
        let mut lim = std::mem::zeroed::<libc::rlimit>();
        let max = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
            (lim.rlim_cur as u64).min(1 << 16) as libc::c_int
        } else {
            1024
        };
        for fd in 3..max {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn raised_limit_is_the_hard_limit() {
        let now = raise_open_file_limit().expect("limit");
        let mut lim = unsafe { std::mem::zeroed::<libc::rlimit>() };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
        assert_eq!(now, lim.rlim_cur as u64);
        assert_eq!(lim.rlim_cur, lim.rlim_max);
    }

    #[test]
    fn started_programs_do_not_inherit_descriptors() {
        // A descriptor without close-on-exec, as a C library might open.
        let file = std::fs::File::open("/dev/null").unwrap();
        let fd = file.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
        let check = format!("test -e /proc/self/fd/{fd}");
        let inherited = Command::new("sh").arg("-c").arg(&check).status().unwrap();
        assert!(inherited.success(), "the test descriptor should leak");

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&check);
        unsafe {
            cmd.pre_exec(|| {
                close_inherited_fds_on_exec();
                Ok(())
            });
        }
        assert!(!cmd.status().unwrap().success());
        drop(file);
    }
}
