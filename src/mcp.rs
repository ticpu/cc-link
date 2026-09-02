//! The control plane: an MCP server that opens and closes links for the session that spawned it.
//!
//! The server is a supervisor and nothing else. It holds one relay child per linked host — that
//! child's pid is what backs the mirror in the registry — and does no work at all until a tool is
//! called, because a configured server is started with every session in its scope whether or not a
//! link is ever wanted.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use serde::Deserialize;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::registry::{self, Paths, Record};

/// How long to wait for a new link to publish its mirror before calling the attach a failure.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(20);

/// How often to look for the mirror while waiting.
const ATTACH_POLL: Duration = Duration::from_millis(200);

/// Host to reach.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HostArg {
    /// SSH destination, as ssh would take it.
    pub host: String,
    /// Path to cc-link on that host, when it is not on the login PATH.
    pub remote_bin: Option<String>,
}

/// Host and the session on it to mirror.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachArg {
    /// SSH destination, as ssh would take it.
    pub host: String,
    /// Session on that host, by name, pid or identifier.
    pub session: String,
    /// Path to cc-link on that host, when it is not on the login PATH.
    pub remote_bin: Option<String>,
}

/// Supervisor holding the links this session has opened.
#[derive(Clone)]
pub struct ControlPlane {
    tool_router: ToolRouter<Self>,
    links: Arc<Mutex<HashMap<String, Child>>>,
    paths: Paths,
    /// Session that spawned this server, and the one every link exports.
    session: Record,
}

#[tool_router(router = tool_router)]
impl ControlPlane {
    /// Build a supervisor for the session that spawned this process.
    pub fn new(paths: Paths, session: Record) -> Self {
        Self {
            tool_router: Self::tool_router(),
            links: Arc::new(Mutex::new(HashMap::new())),
            paths,
            session,
        }
    }

    /// Show which sessions on a host could be mirrored. Reads only; opens no link.
    #[tool(
        name = "list_remote_sessions",
        description = "List the Claude Code sessions running on an SSH host, so one can be picked to attach to. Opens no link and grants nothing.",
        annotations(title = "List remote sessions", read_only_hint = true)
    )]
    pub async fn list_remote_sessions(
        &self,
        Parameters(HostArg { host, remote_bin }): Parameters<HostArg>,
    ) -> Result<String, ErrorData> {
        let sessions = remote_sessions(&host, remote_bin.as_deref())
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;
        serde_json::to_string_pretty(&sessions)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    /// Mirror a session from another machine into this one's peer list.
    #[tool(
        name = "attach",
        description = "Attach this session to a Claude Code session on another machine. This is a live grant of session permissions in both directions: the remote session can drive this one and this one can drive it, for as long as the link lasts. It ends when either session ends or on detach.",
        annotations(title = "Attach to a remote session")
    )]
    pub async fn attach(
        &self,
        Parameters(AttachArg {
            host,
            session,
            remote_bin,
        }): Parameters<AttachArg>,
    ) -> Result<String, ErrorData> {
        self.open(&host, &session, remote_bin.as_deref())
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))
    }

    /// End a link.
    #[tool(
        name = "detach",
        description = "End the link to a host, removing the mirrored session from this machine's peer list and withdrawing the grant.",
        annotations(title = "Detach from a remote session")
    )]
    pub async fn detach(
        &self,
        Parameters(HostArg { host, .. }): Parameters<HostArg>,
    ) -> Result<String, ErrorData> {
        self.close(&host)
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))
    }
}

impl ControlPlane {
    /// Start a relay and wait for it to publish its mirror.
    ///
    /// The wait is what makes a failed attach visible: the child reports ssh's own error on stderr
    /// and exits, and there is no mirror to find.
    async fn open(&self, host: &str, session: &str, remote_bin: Option<&str>) -> Result<String> {
        let mut links = self
            .links
            .lock()
            .await;
        if links.contains_key(host) {
            return Err(anyhow!("already linked to {host}; detach first"));
        }
        let pid = self
            .session
            .pid()
            .ok_or_else(|| anyhow!("the session that spawned cc-link has no pid"))?;
        let exe = std::env::current_exe().context("locating cc-link")?;
        let mut command = Command::new(exe);
        command
            .arg("connect")
            .arg(host)
            .arg("--session")
            .arg(session)
            .arg("--local-session")
            .arg(pid.to_string());
        if let Some(remote_bin) = remote_bin {
            command
                .arg("--remote-bin")
                .arg(remote_bin);
        }
        let mut child = command
            .kill_on_drop(true)
            .spawn()
            .context("starting the relay")?;

        let child_pid = child
            .id()
            .ok_or_else(|| anyhow!("the relay did not start"))?;
        let deadline = tokio::time::Instant::now() + ATTACH_TIMEOUT;
        loop {
            if let Some(mirror) = self.mirror(child_pid)? {
                let name = mirror
                    .name()
                    .unwrap_or_default();
                links.insert(host.to_owned(), child);
                info!(host, name, "link open");
                return Ok(format!(
                    "Linked to {host}. The session is now a peer named {name}; it can drive this session and this session can drive it until you detach."
                ));
            }
            if let Some(status) = child.try_wait()? {
                return Err(anyhow!(
                    "the relay exited with {status}; its error is in this server's stderr"
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                return Err(anyhow!(
                    "the relay did not publish a mirror within {}s",
                    ATTACH_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(ATTACH_POLL).await;
        }
    }

    /// The mirror a relay child published, if it has.
    fn mirror(&self, relay_pid: u32) -> Result<Option<Record>> {
        Ok(registry::list_live(&self.paths)?
            .into_iter()
            .find(|r| r.is_stub() && r.pid() == Some(relay_pid)))
    }

    /// Ask a relay to end its link and wait for it to go.
    async fn close(&self, host: &str) -> Result<String> {
        let mut links = self
            .links
            .lock()
            .await;
        let mut child = links
            .remove(host)
            .ok_or_else(|| anyhow!("no link to {host}"))?;
        end(&mut child).await;
        Ok(format!("Link to {host} closed."))
    }

    /// End every link. Called when the session goes away.
    pub async fn close_all(&self) {
        let mut links = self
            .links
            .lock()
            .await;
        for (host, mut child) in links.drain() {
            end(&mut child).await;
            info!(host, "link closed");
        }
    }
}

/// Signal a relay and wait for it, so its mirror is gone before we answer.
async fn end(child: &mut Child) {
    if let Some(pid) = child.id() {
        if let Err(e) = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        ) {
            warn!(pid, error = %e, "could not signal the relay");
        }
    }
    if let Err(e) = child
        .wait()
        .await
    {
        warn!(error = %e, "could not reap the relay");
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ControlPlane {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(
            "Attaching links this session to a session on another machine over SSH. It is a live, \
             two-way grant of session permissions with no authentication beyond SSH, so treat a \
             host you attach to as one you would give a shell to.",
        )
    }
}

/// Ask a host what it could export, without opening a link.
async fn remote_sessions(host: &str, remote_bin: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let exe = std::env::current_exe().context("locating cc-link")?;
    let mut command = Command::new(exe);
    command
        .arg("list")
        .arg(host);
    if let Some(remote_bin) = remote_bin {
        command
            .arg("--remote-bin")
            .arg(remote_bin);
    }
    let output = command
        .output()
        .await
        .context("starting cc-link list")?;
    if !output
        .status
        .success()
    {
        return Err(anyhow!(
            "{}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .to_owned()
        ));
    }
    serde_json::from_slice(&output.stdout).context("parsing the remote session list")
}

/// The session this server was spawned by, which is its own parent.
fn parent_session(paths: &Paths) -> Result<Record> {
    let parent = nix::unistd::getppid().as_raw() as u32;
    registry::list_live(paths)?
        .into_iter()
        .find(|r| r.pid() == Some(parent) && !r.is_stub())
        .ok_or_else(|| {
            anyhow!("cc-link mcp is meant to be spawned by a Claude Code session; pid {parent} is not one")
        })
}

/// Run the control plane on stdio until the session ends.
pub async fn run() -> Result<()> {
    let paths = Paths::from_env()?;
    let control = ControlPlane::new(paths.clone(), parent_session(&paths)?);
    let service = control
        .clone()
        .serve(rmcp::transport::stdio())
        .await
        .context("starting the MCP server")?;

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = service.waiting() => {}
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
    control
        .close_all()
        .await;
    Ok(())
}
