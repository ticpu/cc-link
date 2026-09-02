//! Bridge Claude Code cross-session messaging between two machines over SSH.
//!
//! A link is a live grant of one session's permissions to a session on another machine, in both
//! directions, with no authentication beyond SSH.

mod frame;
mod link;
mod mcp;
mod mux;
mod registry;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;

use tokio::io::join;
use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::registry::{Paths, Record};

#[derive(Parser)]
#[command(
    version,
    about = "Relay Claude Code cross-session messaging to another machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Relay to a session on another machine. Runs until the link ends.
    Connect {
        /// Host to reach over SSH.
        host: String,
        /// Session to mirror from that host, by name, pid or identifier.
        #[arg(long)]
        session: String,
        /// Session on this machine to export. Defaults to the only live one.
        #[arg(long)]
        local_session: Option<String>,
        /// Path to cc-link on the far end.
        #[arg(long, default_value = "cc-link")]
        remote_bin: String,
        /// Extra ssh options, as accepted by ssh -o.
        #[arg(short = 'o')]
        ssh_option: Vec<String>,
        /// Run this instead of ssh. For tests that need both ends in one process tree.
        #[arg(long)]
        transport_command: Option<String>,
    },
    /// List the sessions a host could export.
    List {
        /// Host to reach over SSH.
        host: String,
        /// Path to cc-link on the far end.
        #[arg(long, default_value = "cc-link")]
        remote_bin: String,
        /// Extra ssh options, as accepted by ssh -o.
        #[arg(short = 'o')]
        ssh_option: Vec<String>,
        /// Run this instead of ssh.
        #[arg(long)]
        transport_command: Option<String>,
    },
    /// The far end of a link. Speaks the protocol on stdio and is not run by hand.
    Serve,
    /// End a link, withdrawing the mirrored session.
    Down {
        /// Only links to this host.
        host: Option<String>,
        /// Only the link mirroring this remote session, prefixed or not.
        #[arg(long)]
        session: Option<String>,
        /// Only the link exporting this local session.
        #[arg(long)]
        local_session: Option<String>,
    },
    /// Control plane: an MCP server that attaches and detaches links for the session that spawned
    /// it.
    Mcp,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging();

    match cli.command {
        Command::Connect {
            host,
            session,
            local_session,
            remote_bin,
            ssh_option,
            transport_command,
        } => {
            connect(
                host,
                session,
                local_session,
                remote_bin,
                ssh_option,
                transport_command,
            )
            .await
        }
        Command::List {
            host,
            remote_bin,
            ssh_option,
            transport_command,
        } => list(host, remote_bin, ssh_option, transport_command).await,
        Command::Serve => serve().await,
        Command::Down {
            host,
            session,
            local_session,
        } => down(host, session, local_session),
        Command::Mcp => mcp::run().await,
    }
}

/// Send diagnostics to the journal, or to stderr where there is no journal.
///
/// Nothing may ever write to stdout: it carries the MCP protocol under `mcp` and the multiplexer
/// under `serve`. A stray print corrupts the stream, and under `serve` it corrupts it exactly the
/// way a chatty login shell does, which sends the reader hunting the wrong bug.
///
/// Structured fields are the reason this is a journal and not a file — an event carrying a field
/// list stays queryable by value rather than by substring.
fn init_logging() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match tracing_journald::layer() {
        Ok(journal) => {
            tracing_subscriber::registry()
                .with(filter())
                .with(journal.with_syslog_identifier("cc-link".into()))
                .init();
        }
        // A container or a host without systemd has no journal socket, and the far end of a link
        // is whatever machine the user has.
        Err(e) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::io::stderr)
                // Under `serve` this stderr is a pipe back to the client, and under `mcp` it is a
                // log the harness collects; colour codes in either are noise a reader has to strip.
                .with_ansi(std::io::stderr().is_terminal())
                .init();
            warn!(error = %e, "no journal to log to; using stderr");
        }
    }
}

/// Start the far end and hold the link open.
async fn connect(
    host: String,
    session: String,
    local_session: Option<String>,
    remote_bin: String,
    ssh_option: Vec<String>,
    transport_command: Option<String>,
) -> Result<()> {
    let paths = Paths::from_env()?;
    let export = registry::resolve_exportable_session(&paths, local_session.as_deref())?;
    let mut child = spawn_transport(&host, &remote_bin, &ssh_option, transport_command)?;
    let io = child_io(&mut child)?;
    let mux = mux::start(io, true).await?;
    let agreement = link::client_handshake(&mux, &export, &session).await?;
    let (link, control) = link::Link::establish(paths, host, export, agreement, mux).await?;
    let outcome = link
        .run(control)
        .await;
    let _ = child.start_kill();
    outcome
}

/// Print what a host could export.
async fn list(
    host: String,
    remote_bin: String,
    ssh_option: Vec<String>,
    transport_command: Option<String>,
) -> Result<()> {
    let mut child = spawn_transport(&host, &remote_bin, &ssh_option, transport_command)?;
    let io = child_io(&mut child)?;
    let mux = mux::start(io, true).await?;
    let sessions = link::client_list(&mux).await?;
    println!("{}", serde_json::to_string_pretty(&sessions)?);
    let _ = child.start_kill();
    Ok(())
}

/// The end SSH starts.
async fn serve() -> Result<()> {
    let paths = Paths::from_env()?;
    let io = join(tokio::io::stdin(), tokio::io::stdout());
    let mut mux = mux::start(io, false).await?;
    let Some((agreement, export)) = link::server_handshake(&mut mux, &paths).await? else {
        return Ok(());
    };
    let host = agreement
        .peer_host
        .clone();
    let (link, control) = link::Link::establish(paths, host, export, agreement, mux).await?;
    link.run(control)
        .await
}

/// End links, one at a time.
///
/// A machine can hold several links to one host — one per session that opened one — and they are
/// only told apart by what each exports. Ending more than the caller meant to would take away a
/// peer another session is still talking to, so several matches with no selector is a refusal, not
/// a broadcast.
fn down(
    host: Option<String>,
    session: Option<String>,
    local_session: Option<String>,
) -> Result<()> {
    let paths = Paths::from_env()?;
    let matches: Vec<Mirror> = registry::list_live(&paths)?
        .into_iter()
        .filter(Record::is_stub)
        .map(Mirror::from)
        .filter(|m| {
            host.as_ref()
                .is_none_or(|h| &m.host == h)
        })
        .filter(|m| {
            session
                .as_ref()
                .is_none_or(|s| m.matches_mirror(s))
        })
        .filter(|m| {
            local_session
                .as_ref()
                .is_none_or(|s| &m.exports == s)
        })
        .collect();

    match matches.as_slice() {
        [] => bail!("no link matches"),
        [one] => {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(one.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            )
            .with_context(|| format!("signalling the relay for {} (pid {})", one.name, one.pid))?;
            info!(
                name = one.name,
                pid = one.pid,
                "asked a relay to end its link"
            );
            Ok(())
        }
        several => {
            let listed: Vec<String> = several
                .iter()
                .map(|m| format!("{} (exports {})", m.name, m.exports))
                .collect();
            bail!(
                "{} links match; name one with --session or --local-session: {}",
                several.len(),
                listed.join(", ")
            )
        }
    }
}

/// A published mirror, reduced to what picking one needs.
struct Mirror {
    pid: u32,
    name: String,
    host: String,
    exports: String,
}

impl Mirror {
    /// Whether a selector names this mirrored session, prefixed or not.
    fn matches_mirror(&self, selector: &str) -> bool {
        self.name == selector
            || self
                .name
                .strip_prefix(&format!("{}~", self.host))
                == Some(selector)
    }
}

impl From<Record> for Mirror {
    fn from(record: Record) -> Self {
        let field = |key: &str| {
            record
                .0
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            pid: record
                .pid()
                .unwrap_or_default(),
            name: record
                .name()
                .unwrap_or_default(),
            host: field(registry::STUB_HOST),
            exports: field(registry::STUB_EXPORTS),
        }
    }
}

/// Start whichever process carries the link.
fn spawn_transport(
    host: &str,
    remote_bin: &str,
    ssh_option: &[String],
    transport_command: Option<String>,
) -> Result<tokio::process::Child> {
    let mut command = match transport_command {
        Some(line) => mux::transport_command(&line),
        None => mux::ssh_command(host, remote_bin, ssh_option),
    };
    command
        .spawn()
        .context("starting the far end")
}

/// The child's pipes, as one duplex channel.
fn child_io(
    child: &mut tokio::process::Child,
) -> Result<tokio::io::Join<tokio::process::ChildStdout, tokio::process::ChildStdin>> {
    let stdout = child
        .stdout
        .take()
        .context("the far end has no stdout")?;
    let stdin = child
        .stdin
        .take()
        .context("the far end has no stdin")?;
    Ok(join(stdout, stdin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror() -> Mirror {
        Mirror {
            pid: 42,
            name: "p4~claude-code-9b".into(),
            host: "p4".into(),
            exports: "cc-link-bridge".into(),
        }
    }

    #[test]
    fn a_mirrored_session_answers_to_its_name_with_or_without_the_host() {
        assert!(mirror().matches_mirror("p4~claude-code-9b"));
        assert!(mirror().matches_mirror("claude-code-9b"));
        assert!(!mirror().matches_mirror("claude-code-9e"));
        assert!(!mirror().matches_mirror("p4~"));
    }
}
