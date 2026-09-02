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
use tokio::io::join;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::registry::Paths;

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
    /// End a link, withdrawing the mirrored session. Also clears mirrors left by a relay that is
    /// no longer running.
    Down {
        /// Only mirrors of this host.
        host: Option<String>,
    },
    /// Control plane: an MCP server that attaches and detaches links for the session that spawned
    /// it.
    Mcp,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // stdout carries the protocol on both the serve and mcp paths, so diagnostics never go there.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

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
        Command::Down { host } => down(host),
        Command::Mcp => mcp::run().await,
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

/// Remove mirrors whose relay is gone, or end the ones still running.
fn down(host: Option<String>) -> Result<()> {
    let paths = Paths::from_env()?;
    let mut matched = 0;
    for record in registry::list_live(&paths)? {
        if !record.is_stub() {
            continue;
        }
        let recorded_host = record
            .0
            .get(registry::STUB_HOST)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        if let Some(host) = &host {
            if &recorded_host != host {
                continue;
            }
        }
        let pid = record
            .pid()
            .context("mirror has no pid")?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .with_context(|| format!("signalling the relay for {recorded_host} (pid {pid})"))?;
        info!(host = recorded_host, pid, "asked a relay to end its link");
        matched += 1;
    }
    if matched == 0 {
        bail!("no mirrored session matches");
    }
    Ok(())
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
