# cc-link

Makes a Claude Code session on another machine appear as an ordinary local peer, so `ListAgents`
shows it and `SendMessage` reaches it, with no change to Claude Code itself.

**A link is a live grant of one session's permissions to a session on another machine, in both
directions.** Whatever one side's user can ask their Claude to run, the far session will run. On
Linux the messaging layer authenticates nothing, so SSH is the only thing standing between the far
machine and your session. Do not attach to a host you would not hand a shell to.

## How it works

One cc-link runs on each machine, joined by a single SSH stdio channel — no listening port, no
forwarded socket. Each side publishes a mirror of the far session into its own registry, under its
own pid, and relays messages between that mirror's socket and the real session's. The mirrored name
is prefixed with the host (`p4~claude-code-9e`) because it is the only address that survives the
crossing.

The link ends when either exported session ends, when the SSH connection drops, or on `detach`.
Nothing reconnects by itself.

## Install

The binary must exist on both machines and the local end must be able to reach the far one over SSH
without a prompt (`BatchMode=yes` is always set; an ssh agent or a key is on you).

`make deb` builds `cc-link_<version>_amd64.deb` and `cc-link_<version>_arm64.deb` in one container
pass, against an older glibc than the machines they install on.

`makepkg -si` from the repository root builds and installs the working tree on Arch. It takes the
version from `Cargo.toml`, so there is nothing to bump in two places.

## Use from a session

Register the MCP server in the scope where linking is actually wanted — every session in that scope
starts a cc-link process, so user-global is rarely the right place:

```json
{
  "mcpServers": {
    "cc-link": { "command": "cc-link", "args": ["mcp"] }
  }
}
```

The server starts with the session, so registering it does nothing for the session you register it
from — the tools appear in the next one. That is not a fault to chase.

Then `list_remote_sessions` to see what a host offers, `attach` to mirror one, `detach` to end it.
Attaching prompts every time, by design. Both take an optional `remote_bin` for a host where cc-link
is not on the login PATH — which is not the interactive PATH, so check with
`ssh <host> 'echo $PATH'` before assuming.

Diagnostics go to the journal: `journalctl -t cc-link`, or `-f` while a link is up. The far end logs
to the journal on its own machine, which is where its problems belong. A host with no journal gets
stderr instead.

## Use from a shell

```
cc-link list p4
cc-link connect p4 --session claude-code-9e
cc-link down [p4]
```

`connect` runs in the foreground for as long as the link lasts, and `down` is how you end it — it
signals the relay, which withdraws its mirror before exiting. Never kill a relay by hand when `down`
will do it. A relay that dies anyway leaves nothing behind for long: its mirror carries a dead pid
in the local domain, which is exactly what Claude Code's own sweeper collects.
