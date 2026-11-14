//! Process-tree containment and kill (contract r2 §9.3 "Kill", §9.4).
//!
//! - Windows: each shell is assigned to its own Job Object created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; harness-host holds the only handle,
//!   so dropping it (kill, or harness-host dying) terminates the whole tree.
//! - Unix: the shell is a session leader (portable-pty `setsid`), so its
//!   process group id is its pid: `SIGHUP` the group, `SIGKILL` it after the
//!   grace period.

/// Owns whatever keeps a session's process tree contained.
pub(crate) struct TreeGuard {
    #[cfg(windows)]
    job: Option<win32job::Job>,
    #[cfg(unix)]
    pgid: Option<u32>,
}

impl TreeGuard {
    #[cfg(windows)]
    pub(crate) fn contain(child: &(dyn portable_pty::Child + Send + Sync)) -> Self {
        let job = child.as_raw_handle().and_then(|handle| {
            let mut info = win32job::ExtendedLimitInfo::new();
            info.limit_kill_on_job_close();
            let job = win32job::Job::create_with_limit_info(&info)
                .map_err(|e| tracing::warn!(error = %e, "job object create failed"))
                .ok()?;
            // 句柄只在本进程中作为整数传给 AssignProcessToJobObject，所有权仍归 portable-pty 的 child。
            #[allow(clippy::as_conversions)]
            let raw = handle as isize;
            job.assign_process(raw)
                .map_err(|e| tracing::warn!(error = %e, "assign shell to job failed"))
                .ok()?;
            Some(job)
        });
        Self { job }
    }

    #[cfg(unix)]
    pub(crate) fn contain(child: &(dyn portable_pty::Child + Send + Sync)) -> Self {
        Self {
            pgid: child.process_id(),
        }
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn contain(_child: &(dyn portable_pty::Child + Send + Sync)) -> Self {
        Self {}
    }

    /// First, polite stage. Windows: closing the job kills the tree at once.
    pub(crate) fn hang_up(&mut self) {
        #[cfg(windows)]
        {
            drop(self.job.take());
        }
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                signal_group(pgid, nix::sys::signal::Signal::SIGHUP);
            }
        }
    }

    /// Final stage for a tree that ignored the hang-up.
    pub(crate) fn force(&mut self) {
        #[cfg(windows)]
        {
            drop(self.job.take());
        }
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid.take() {
                signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
fn signal_group(pgid: u32, signal: nix::sys::signal::Signal) {
    let Ok(raw) = i32::try_from(pgid) else { return };
    if let Err(error) = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), signal) {
        tracing::debug!(pgid, %error, ?signal, "killpg failed (group already gone?)");
    }
}
