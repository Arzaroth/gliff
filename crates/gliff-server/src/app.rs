//! A session that runs one app on its private screen: the app starts on a
//! workspace of its own pinned to the session's headless output, and its
//! first window opens fullscreen, above the bar the remote's shell puts on
//! every screen. The session follows the
//! app's title and ends when its last window closes; ending the session
//! first closes the app as its close button would.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// How long an app may take to open its first window.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Poll {
    Running,
    Title(String),
    Closed,
}

/// The processes of an app's windows and their descendants, which the
/// session sends the sound of. The app's poll keeps it current.
#[derive(Clone, Default)]
pub struct Processes(std::sync::Arc<std::sync::Mutex<Vec<u64>>>);

impl Processes {
    pub(crate) fn set(&self, roots: Vec<u64>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = roots;
    }

    /// Whether `pid` is one of the roots or descends from one.
    pub fn contains(&self, pid: u64) -> bool {
        let roots = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut pid = pid;
        for _ in 0..64 {
            if roots.contains(&pid) {
                return true;
            }
            match parent(pid) {
                Some(parent) if parent > 1 => pid = parent,
                _ => return false,
            }
        }
        false
    }
}

fn parent(pid: u64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

pub struct App {
    instance: hypr_ipc::Instance,
    workspace: String,
    started: Instant,
    opened: bool,
    title: String,
    processes: Processes,
}

impl App {
    pub fn launch(instance: &hypr_ipc::Instance, output: &str, command: &str) -> Result<Self> {
        let workspace = format!("gliff-app-{}", std::process::id());
        instance
            .bare_workspace_on(&workspace, output)
            .context("pin the app's workspace to the private screen")?;
        instance
            .exec_on_workspace(&workspace, command)
            .context("start the app")?;
        tracing::info!(command, workspace, "app started");
        Ok(Self {
            instance: instance.clone(),
            workspace,
            started: Instant::now(),
            opened: false,
            title: String::new(),
            processes: Processes::default(),
        })
    }

    pub fn processes(&self) -> Processes {
        self.processes.clone()
    }

    fn windows(&self) -> Result<Vec<hypr_ipc::Client>> {
        Ok(on_workspace(self.instance.clients()?, &self.workspace))
    }

    pub fn poll(&mut self) -> Poll {
        let windows = match self.windows() {
            Ok(windows) => windows,
            Err(e) => {
                tracing::debug!(error = %e, "cannot list the app's windows");
                return Poll::Running;
            }
        };
        let Some(first) = windows.first() else {
            if self.opened || self.started.elapsed() >= OPEN_TIMEOUT {
                tracing::info!(opened = self.opened, "app closed");
                return Poll::Closed;
            }
            return Poll::Running;
        };
        self.opened = true;
        self.processes.set(
            windows
                .iter()
                .filter_map(|w| u64::try_from(w.pid).ok())
                .collect(),
        );
        if first.title != self.title {
            self.title = first.title.clone();
            return Poll::Title(self.title.clone());
        }
        Poll::Running
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for window in self.windows().unwrap_or_default() {
            if let Err(e) = self.instance.close_window(&window.address) {
                tracing::warn!(error = %e, "cannot close the app's window");
            }
        }
    }
}

fn on_workspace(clients: Vec<hypr_ipc::Client>, workspace: &str) -> Vec<hypr_ipc::Client> {
    clients
        .into_iter()
        .filter(|c| c.workspace.name == workspace)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_apps_own_workspace_counts() {
        let clients: Vec<hypr_ipc::Client> = serde_json::from_str(
            r#"[
                {"address": "0x1", "title": "app", "workspace": {"id": -1337, "name": "gliff-app-7"}},
                {"address": "0x2", "title": "other", "workspace": {"id": 1, "name": "1"}},
                {"address": "0x3", "title": "else", "workspace": {"id": -1338, "name": "gliff-app-77"}}
            ]"#,
        )
        .unwrap();
        let ours = on_workspace(clients, "gliff-app-7");
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].address, "0x1");
    }

    #[test]
    fn processes_are_the_roots_and_their_descendants() {
        let me = u64::from(std::process::id());
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let processes = Processes::default();
        assert!(!processes.contains(me));
        processes.set(vec![me]);
        assert!(processes.contains(me));
        assert!(processes.contains(u64::from(child.id())));
        assert!(!processes.contains(parent(me).unwrap()));
        assert!(!processes.contains(1));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
