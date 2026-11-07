//! Locking the remote's screen through logind. The user's graphical session
//! gets a Lock signal, which the desktop's idle daemon answers with its own
//! lock screen: on Omarchy, hypridle runs its `lock_cmd`.

use crate::audio::command;

pub fn lock() {
    let Some(session) = display_session() else {
        tracing::warn!("no graphical session to lock");
        return;
    };
    match command("loginctl")
        .args(["lock-session", &session])
        .stdout(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => tracing::info!(session, "screen locked"),
        Ok(status) => tracing::warn!(session, %status, "loginctl could not lock the screen"),
        Err(e) => tracing::warn!(error = %e, "cannot run loginctl"),
    }
}

/// Locks the screen when dropped while armed, on every way out of the
/// session.
#[derive(Default)]
pub struct LockOnEnd {
    pub armed: bool,
}

impl Drop for LockOnEnd {
    fn drop(&mut self) {
        if self.armed {
            lock();
        }
    }
}

/// The user's graphical session, as logind names it in the user's
/// `Display` property.
fn display_session() -> Option<String> {
    let uid =
        std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").ok()?).to_string();
    let out = command("loginctl")
        .args(["show-user", &uid, "-p", "Display", "--value"])
        .stdout(std::process::Stdio::piped())
        .output()
        .ok()?;
    parse_session(&String::from_utf8_lossy(&out.stdout))
}

fn parse_session(output: &str) -> Option<String> {
    let id = output.trim();
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_session_is_a_plain_session_id() {
        assert_eq!(parse_session("2\n").as_deref(), Some("2"));
        assert_eq!(parse_session("c3\n").as_deref(), Some("c3"));
        assert_eq!(parse_session("\n"), None);
        assert_eq!(parse_session("2; rm -rf /"), None);
    }

    #[test]
    fn an_unarmed_guard_does_not_lock() {
        drop(LockOnEnd::default());
    }
}
