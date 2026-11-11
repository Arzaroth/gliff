//! Locking the remote's screen. The lock runs from the compositor, in the
//! graphical session's environment: Omarchy locks through its shell, which
//! does not answer logind, and other desktops through logind's Lock signal,
//! which their idle daemon answers (hypridle runs its `lock_cmd`).

const LOCK: &str = "if command -v omarchy-system-lock >/dev/null; then omarchy-system-lock; else loginctl lock-session auto; fi";

pub fn lock(instance: &hypr_ipc::Instance) {
    match instance.exec(LOCK) {
        Ok(()) => tracing::info!("screen lock started"),
        Err(e) => tracing::warn!(error = %e, "the compositor did not start the screen lock"),
    }
}

/// Locks the screen when dropped while armed, on every way out of the
/// session.
pub struct LockOnEnd {
    pub armed: bool,
    instance: hypr_ipc::Instance,
}

impl LockOnEnd {
    pub fn new(instance: hypr_ipc::Instance) -> Self {
        Self {
            armed: false,
            instance,
        }
    }
}

impl Drop for LockOnEnd {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.instance);
        }
    }
}
