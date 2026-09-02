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

```
make deb
```

builds `cc-link_<version>_amd64.deb` and `cc-link_<version>_arm64.deb` in one container pass.

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

Then `list_remote_sessions` to see what a host offers, `attach` to mirror one, `detach` to end it.
Attaching prompts every time, by design.

## Use from a shell

```
cc-link list p4
cc-link connect p4 --session claude-code-9e
cc-link down [p4]
```

`connect` runs in the foreground for as long as the link lasts. `down` is for mirrors left behind
when no session is running; a mirror whose relay has died is collected by Claude Code's own sweeper
without help.
