//! The relay itself: symmetric on both machines, only the dialling differs.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::frame;
use crate::mux::{self, Control, Intent, Mux, Stream};
use crate::registry::{self, LocalIdentity, Paths, Record};

/// How often a mirrored record's activity timestamp is moved forward. Listings drop a peer a day
/// stale, so this leaves an order of magnitude of headroom.
const HEARTBEAT: Duration = Duration::from_secs(3600);

/// How long to let the registry settle before reading a record that just changed.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);

/// How long the far end waits for a client to read a session list before giving up on it.
const LIST_DRAIN: Duration = Duration::from_secs(10);

/// One end of a link, once both sides have agreed to it.
pub struct Link {
    paths: Paths,
    identity: LocalIdentity,
    /// Session on this machine the far end reaches.
    export: Record,
    mux: Mux,
}

/// Result of a handshake: what the far end exports, and how far its clock is from ours.
pub struct Agreement {
    /// Name the far end knows itself by, which prefixes the mirrored session's name.
    pub peer_host: String,
    /// Record of the session the far end exports.
    pub remote_export: Record,
    /// Milliseconds between the two clocks.
    pub clock_offset_ms: i64,
    /// Control stream the link runs on.
    pub control: Stream,
}

/// Ask the far end to mirror one of its sessions, and offer ours in return.
pub async fn client_handshake(mux: &Mux, export: &Record, session: &str) -> Result<Agreement> {
    let mut control = mux
        .open()
        .await
        .context("opening the control stream")?;
    let sent_at = registry::now_ms();
    mux::send(
        &mut control,
        &hello(
            Intent::Attach {
                session: session.to_owned(),
            },
            Some(
                export
                    .0
                    .clone(),
            ),
        )?,
    )
    .await?;
    match mux::recv(&mut control).await? {
        Control::Ready {
            export,
            clock_ms,
            host,
        } => {
            let offset = clock_offset(sent_at, clock_ms);
            Ok(Agreement {
                peer_host: host,
                remote_export: Record(export),
                clock_offset_ms: offset,
                control,
            })
        }
        Control::Refused { reason } => bail!("the far end refused the link: {reason}"),
        other => bail!("unexpected control message during handshake: {other:?}"),
    }
}

/// Enumerate the sessions the far end could export.
pub async fn client_list(mux: &Mux) -> Result<Vec<Value>> {
    let mut control = mux
        .open()
        .await
        .context("opening the control stream")?;
    mux::send(&mut control, &hello(Intent::List, None)?).await?;
    match mux::recv(&mut control).await? {
        Control::Sessions { sessions } => Ok(sessions),
        Control::Refused { reason } => bail!("the far end refused: {reason}"),
        other => bail!("unexpected control message: {other:?}"),
    }
}

/// Answer a client's handshake.
///
/// The session exported here is the one the client named. Falling back to whatever session happens
/// to be running would hand an arbitrary user's permissions across the boundary.
pub async fn server_handshake(mux: &mut Mux, paths: &Paths) -> Result<Option<(Agreement, Record)>> {
    let mut control = mux
        .accept()
        .await
        .ok_or_else(|| anyhow!("the client closed the link before saying anything"))?;
    let Control::Hello {
        protocol,
        platform,
        pid_domain,
        home,
        host,
        clock_ms,
        intent,
        export,
    } = mux::recv(&mut control).await?
    else {
        bail!("the client opened with something other than a greeting");
    };

    if let Err(reason) = acceptable(protocol, &platform, &pid_domain, &home) {
        mux::send(
            &mut control,
            &Control::Refused {
                reason: reason.clone(),
            },
        )
        .await?;
        bail!(reason);
    }

    match intent {
        Intent::List => {
            let sessions = registry::list_live(paths)?
                .into_iter()
                .filter(|r| !r.is_stub())
                .map(|r| r.0)
                .collect();
            mux::send(&mut control, &Control::Sessions { sessions }).await?;
            // Exiting here would drop the multiplexer with the reply still in it: the frame is
            // written to the stream, not to the pipe, and nothing else would ever flush it. Wait
            // for the client to read and close.
            let _ = tokio::time::timeout(LIST_DRAIN, drain(&mut control)).await;
            Ok(None)
        }
        Intent::Attach { session } => {
            let local = match registry::resolve_exportable_session(paths, Some(&session)) {
                Ok(record) => record,
                Err(e) => {
                    let reason = e.to_string();
                    mux::send(
                        &mut control,
                        &Control::Refused {
                            reason: reason.clone(),
                        },
                    )
                    .await?;
                    bail!(reason);
                }
            };
            let remote_export =
                export.ok_or_else(|| anyhow!("the client offered no session to mirror"))?;
            let sent_at = registry::now_ms();
            mux::send(
                &mut control,
                &Control::Ready {
                    export: local
                        .0
                        .clone(),
                    clock_ms: registry::now_ms(),
                    host: local_host(),
                },
            )
            .await?;
            Ok(Some((
                Agreement {
                    peer_host: host,
                    remote_export: Record(remote_export),
                    clock_offset_ms: clock_offset(sent_at, clock_ms),
                    control,
                },
                local,
            )))
        }
    }
}

/// Refuse a link that should not exist rather than build one that misbehaves later.
fn acceptable(protocol: u32, platform: &str, pid_domain: &str, home: &str) -> Result<(), String> {
    if protocol != mux::CONTROL_PROTOCOL {
        return Err(format!(
            "control protocol {protocol} against {}",
            mux::CONTROL_PROTOCOL
        ));
    }
    if platform != "linux" {
        return Err(format!(
            "cc-link relays between Linux hosts only; the other end reports {platform}, where a mirrored session's messages would be dropped for failing authentication"
        ));
    }
    let local_domain = registry::local_pid_domain().map_err(|e| e.to_string())?;
    let local_home = std::env::var("HOME").unwrap_or_default();
    if mux::same_machine(
        &local_domain,
        std::path::Path::new(&local_home),
        pid_domain,
        home,
    ) {
        return Err(
            "both ends are the same machine and account, so they share one registry".into(),
        );
    }
    Ok(())
}

/// Read a stream until the other end closes it.
async fn drain(stream: &mut Stream) -> Result<()> {
    let mut sink = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(stream, &mut sink).await?;
    Ok(())
}

/// Name this machine knows itself by. Only ever a display prefix.
fn local_host() -> String {
    nix::unistd::gethostname()
        .map(|h| {
            h.to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|_| "peer".into())
}

fn hello(intent: Intent, export: Option<Value>) -> Result<Control> {
    Ok(Control::Hello {
        protocol: mux::CONTROL_PROTOCOL,
        platform: "linux".into(),
        pid_domain: registry::local_pid_domain()?,
        home: std::env::var("HOME").unwrap_or_default(),
        host: local_host(),
        clock_ms: registry::now_ms(),
        intent,
        export,
    })
}

/// Difference between the two clocks, measured across one round trip.
fn clock_offset(sent_at_ms: u64, remote_ms: u64) -> i64 {
    let midpoint = (sent_at_ms + registry::now_ms()) / 2;
    midpoint as i64 - remote_ms as i64
}

impl Link {
    /// Publish the mirror and start relaying.
    ///
    /// The record appears only now: publishing earlier advertises a peer that cannot be reached.
    pub async fn establish(
        paths: Paths,
        host: String,
        export: Record,
        agreement: Agreement,
        mux: Mux,
    ) -> Result<(Self, Stream)> {
        let pid = std::process::id();
        let identity = LocalIdentity {
            pid,
            proc_start: registry::proc_start(pid)?,
            pid_domain: registry::local_pid_domain()?,
            socket_path: paths.socket_path(pid),
            host,
            clock_offset_ms: agreement.clock_offset_ms,
        };
        // The exported session is itself a live local record, so it is the template: the mirror
        // then has whatever shape this build of Claude Code writes, with no field list to keep.
        let record = registry::synth_record(&export, &agreement.remote_export, &identity)?;
        let key = registry::synth_key(&registry::read_key(&paths, &export)?, &identity)?;
        registry::publish(&paths, &identity, &record, &key)?;
        info!(
            socket = %identity.socket_path.display(),
            name = record["name"].as_str(),
            "mirroring remote session"
        );
        Ok((
            Self {
                paths,
                identity,
                export,
                mux,
            },
            agreement.control,
        ))
    }

    /// Relay until either side ends the link or the exported session goes away.
    pub async fn run(mut self, control: Stream) -> Result<()> {
        let listener = bind(
            &self
                .identity
                .socket_path,
        )?;
        let (control_read, mut control_write) = tokio::io::split(control);
        let mut control_read = BufReader::new(control_read);

        let (changes_tx, mut changes) = mpsc::channel::<()>(8);
        let watcher = watch_registry(&self.paths, changes_tx)?;
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat
            .tick()
            .await;

        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        let reason = loop {
            let mut control_line = String::new();
            tokio::select! {
                accepted = listener.accept() => {
                    let (conn, _) = accepted.context("accepting on the mirror socket")?;
                    let mux_stream = self.mux.open().await?;
                    let socket = self.identity.socket_path.clone();
                    tokio::spawn(async move {
                        if let Err(e) = pump(conn, mux_stream, socket).await {
                            warn!(error = %e, "a relayed connection ended badly");
                        }
                    });
                }
                inbound = self.mux.accept() => {
                    match inbound {
                        Some(stream) => {
                            let target = registry::validate_target(&self.paths, &self.export)?;
                            let socket = target
                                .socket_path()
                                .ok_or_else(|| anyhow!("the exported session has no socket"))?;
                            let ours = self.identity.socket_path.clone();
                            tokio::spawn(async move {
                                if let Err(e) = deliver(stream, socket, ours).await {
                                    warn!(error = %e, "a relayed connection ended badly");
                                }
                            });
                        }
                        None => break "the link closed".to_string(),
                    }
                }
                read = control_read.read_line(&mut control_line) => {
                    // A closed control stream is how the other end says it is done, whether it
                    // closed cleanly or its process went away; it is the normal exit, not a fault.
                    match read {
                        Ok(0) => break "the far end closed the control stream".to_string(),
                        Ok(_) => {}
                        Err(e) => break format!("the control stream ended: {e}"),
                    }
                    match serde_json::from_str::<Control>(&control_line)? {
                        Control::Update { record } => self.mirror_update(&record)?,
                        Control::Bye { reason } => break reason,
                        other => warn!(?other, "unexpected control message"),
                    }
                }
                Some(()) = changes.recv() => {
                    tokio::time::sleep(WATCH_DEBOUNCE).await;
                    match registry::validate_target(&self.paths, &self.export) {
                        Ok(current) => {
                            if let Err(e) = mux::send(&mut control_write, &Control::Update { record: current.0 }).await {
                                break format!("the control stream ended: {e}");
                            }
                        }
                        Err(e) => break format!("the exported session is gone: {e}"),
                    }
                }
                _ = heartbeat.tick() => {
                    registry::touch_activity(&self.paths, &self.identity)?;
                }
                _ = sigint.recv() => break "interrupted".to_string(),
                _ = sigterm.recv() => break "terminated".to_string(),
            }
        };

        drop(watcher);
        // Local artifacts go first: shutdown is measured in milliseconds before the kill lands, and
        // a mirror left advertised is a peer someone will send to.
        registry::unpublish(&self.paths, &self.identity)?;
        let _ = mux::send(
            &mut control_write,
            &Control::Bye {
                reason: reason.clone(),
            },
        )
        .await;
        info!(reason, "link ended");
        Ok(())
    }

    /// Rewrite the mirrored record when the far session's own changes.
    fn mirror_update(&self, remote: &Value) -> Result<()> {
        let record = registry::synth_record(&self.export, &Record(remote.clone()), &self.identity)?;
        registry::write_record(&self.paths, &self.identity, &record)
    }
}

/// Bind the mirror's socket, refusing to clobber a live session's.
///
/// A path left behind by a dead process is unlinked, but only once it refuses a connection: pids
/// are reused, and the socket at that path may belong to a session that is very much alive.
fn bind(path: &PathBuf) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .with_context(|| format!("setting mode on {}", dir.display()))?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => return Ok(listener),
        Err(e) if e.kind() != std::io::ErrorKind::AddrInUse => {
            return Err(e).with_context(|| format!("binding {}", path.display()))
        }
        Err(_) => {}
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => bail!("{} is already served by a live session", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path)
                .with_context(|| format!("removing the stale socket {}", path.display()))?;
            UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("probing {}", path.display())),
    }
}

/// A local session connected to the mirror: carry it to the far end.
async fn pump(conn: UnixStream, stream: Stream, our_socket: PathBuf) -> Result<()> {
    let (conn_read, conn_write) = conn.into_split();
    let (stream_read, stream_write) = tokio::io::split(stream);
    let outbound = tokio::spawn(async move {
        let mut reader = conn_read;
        let mut writer = stream_write;
        tokio::io::copy(&mut reader, &mut writer).await?;
        writer
            .shutdown()
            .await
    });
    frame::relay_lines(stream_read, conn_write, &our_socket).await?;
    outbound.await??;
    Ok(())
}

/// The far end opened a connection: hand it to the exported session.
///
/// The session socket is dialled only once there is a message to write, because a connection that
/// stays silent is destroyed by the receiver.
async fn deliver(stream: Stream, session_socket: PathBuf, our_socket: PathBuf) -> Result<()> {
    let (stream_read, stream_write) = tokio::io::split(stream);
    let mut reader = BufReader::new(stream_read);
    let mut first = String::new();
    if reader
        .read_line(&mut first)
        .await?
        == 0
    {
        return Ok(());
    }
    let rewritten = frame::rewrite_reply_address(first.trim_end_matches('\n'), &our_socket)?;

    let conn = UnixStream::connect(&session_socket)
        .await
        .with_context(|| format!("connecting to {}", session_socket.display()))?;
    let (conn_read, mut conn_write) = conn.into_split();
    conn_write
        .write_all(rewritten.as_bytes())
        .await?;
    conn_write
        .write_all(b"\n")
        .await?;
    conn_write
        .flush()
        .await?;

    let inbound = tokio::spawn({
        let our_socket = our_socket.clone();
        async move { frame::relay_lines(reader, conn_write, &our_socket).await }
    });
    let mut reader = conn_read;
    let mut writer = stream_write;
    tokio::io::copy(&mut reader, &mut writer).await?;
    writer
        .shutdown()
        .await?;
    inbound.await??;
    Ok(())
}

/// Watch the registry directory.
///
/// Records are rewritten whole rather than edited, so a watch on one file goes deaf as soon as it
/// is replaced; the directory is what stays.
fn watch_registry(paths: &Paths, changes: mpsc::Sender<()>) -> Result<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(_) => {
                let _ = changes.try_send(());
            }
            Err(e) => warn!(error = %e, "registry watch failed"),
        })?;
    let dir = paths.sessions_dir();
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;
    Ok(watcher)
}
