//! A session that runs one app on its private screen: the app starts on a
//! workspace of its own pinned to the session's headless output, with no
//! gaps or border, so its window fills the screen. The session follows the
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

pub struct App {
    instance: hypr_ipc::Instance,
    workspace: String,
    started: Instant,
    opened: bool,
    title: String,
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
        })
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
