# cc-link design rationale

## A link is a live cross-machine grant of session permissions

Attaching a session to a session on another machine lets each side drive the other: whatever one
session's user can ask their Claude to run, the far session will run, in both directions. On Linux
the messaging layer authenticates nothing, so the SSH connection is the only thing standing between
the far machine and the local session. The two ends are typically different accounts, often across
an organisational boundary, and the grant lasts as long as the link.

Nothing is bound outside the SSH pipe. The transport is a single stdio channel to a process started
by SSH — no listening port, no forwarded socket, no relaying helper. An earlier proof of the
mechanism used a forwarded port, and any local user on either machine could drive the far session
through it.

## The control plane is MCP; the data plane is the native socket

Messaging stays on the socket Claude Code already speaks, so a remote peer appears in listings and
replies land in conversations with nothing aware of the relay. Carrying messages over MCP instead
was rejected: MCP is request/response, so a remote session could only ever be polled, never push
into a conversation.

Attaching, detaching and surveying a remote host are MCP tools. A server the harness spawns lives
exactly as long as the session, which is the right lifetime for a grant, and it makes the session
being exported the server's own parent rather than something guessed by walking process ancestry.

A configured server is started with every session in its scope, whether or not a link is ever
opened, so an idle supervisor does nothing at all: no connection, no artifacts, no contact with a
remote host until it is asked for one.

Opening a link prompts every time. Surveying a remote host is marked read-only so it stays usable
while planning; attaching and detaching are not, because a prompt is the point at which the user
decides to hand their session to another machine.

## Both ends publish a record

The receiving side takes the sender's identity from the kernel, not from the message, so it sees the
relay process rather than the session behind it. A relay must therefore own a registry record for
its own process on each machine; a design where only the initiating side publishes cannot deliver in
the other direction.

## Mirrored records are overlaid onto a live local record

The registry is undocumented internals with no compatibility promise, and versions in the field
already disagree about which fields exist. A mirrored record is built by reading a live local record
as a template, overlaying the remote session's descriptive fields for keys the template also has,
and forcing the fields that describe identity — process, domain, socket, version, protocol floor.
Fields the local build does not know are dropped; fields the remote build lacks keep the template's
value. Nothing hardcodes a field list, and a machine with no live session to template from is
refused rather than served a guess.

The key file is the exception to the fall-through: every field in it is ours. A value inherited from
the template there would describe a different process.

## The mirrored session identifier is synthetic

Neither identifier can be reused: the remote one collides when two machines mirror each other, and
the template's duplicates a live local session. A value derived from the remote domain and the
remote identifier is unique on both machines and stable across reconnects, so a mirrored peer keeps
its identity when a link is rebuilt.

## The mirrored token is generated locally

The token in a key file exists to let a session recognise messages from a process it spawned itself.
Nothing else consults it on the platform this runs on, and the platform that does require it to
match is refused outright, so moving the remote session's real token across the link would place a
credential in a registry on a machine belonging to another account and buy nothing.

The consequence is that a link is silently unauthenticated: frames arrive and are delivered because
this platform requires no authentication, not because anything verified them. That is a property of
the transport being the trust boundary, and it is why the transport is the only thing the design
lets a user choose.

## Advertised capabilities are intersected

A sender never negotiates. It reads what a peer's record advertises, branches immediately, and a
capability that is claimed but not honoured sends it down a path that silently does nothing. A
mirrored record therefore advertises only what both the remote session and the relay can serve.
Intersecting costs a capability that both real sessions support whenever the relay does not, which
is correct: the relay is genuinely in the path.

The protocol floor is the local build's. A mirrored record claiming a lower one is dropped from
listings entirely.

## Reply addresses are rewritten in flight

A message carries the sender's socket path as the address to reply to, and the receiver accepts any
socket sitting in its own socket directory — a directory whose path is identical on both machines
for the same account. An unrewritten address is therefore accepted and points replies at a local
path that is either absent or belongs to an unrelated session. Every message is parsed and its reply
address replaced with the relay's own socket on the receiving side, which makes this a line-oriented
proxy rather than a byte pipe. Message identifiers, including those delivery receipts refer back to,
pass through untouched.

Rewriting can lengthen a line past the receiver's limit, whose response is to destroy the whole
connection. A line that would exceed it after rewriting is refused with an error naming the
condition, because a message dropped quietly here and a connection torn down there are otherwise
indistinguishable.

## The mirrored record carries the local process domain

A record whose domain is foreign is ignored by discovery but never collected, so it would accumulate
on disk with nothing able to remove it. A record in the local domain backed by the relay process is
collected by the harness's own sweeper the moment the relay dies. That is what makes crash cleanup
free, and why there is no lockfile and no stale-entry reaper here. Clean exit is driven by signals,
because destructors do not run on termination, and it removes the local artifacts before telling the
far side anything: shutdown is measured in milliseconds before the kill lands, so nothing
network-bound may sit ahead of the removal.

## A mirrored record is published only once the far session is confirmed

Publishing advertises a peer that someone will send to. The record and its key appear after the
handshake establishes that the remote session exists and is reachable, and are withdrawn as soon as
that stops being true — including when the exported session on either end goes away, because the
grant ended when the session did.

The activity timestamp is refreshed on a timer. Listings drop a peer whose timestamp has gone stale,
so a link that carries no traffic would otherwise disappear while still live. Timestamps arriving
from the far side are translated by the clock offset measured at handshake, so a machine whose clock
runs ahead cannot produce a record that appears to have been written in the future.

## Status is pushed, not snapshotted

A mirrored record left frozen reports one status for the life of the link, and users send work into
a session that is busy. Each side watches its own registry and pushes its exported session's status
and name across as they change.

## The exported session is never inferred by the far end

The side reached over SSH exports the session named in the handshake and refuses a name that does
not resolve to exactly one live session. Falling back to whatever session happens to be running
would hand an arbitrary user's permissions across the boundary without anyone choosing to.

## Chained and self-directed links are refused

A link whose exported session is itself a mirror makes the grant transitive while no listing on
either outer end can show the far machine, and killing the middle leaves both ends believing a link
is live while the heartbeat keeps the mirrors looking healthy. A grant nobody can see is one nobody
can audit. Mirrors are marked in their own record so this is decided on something the relay wrote,
not on a process name any process can set for itself.

A link to a host that resolves back to the same machine is refused for a related reason: both ends
would share one registry, template from each other's mirrors and mirror them back.

## The mirrored name is the only stable address

A peer's short reference is derived from its socket path, and the two ends of a link necessarily
hold different socket paths, so each computes a different reference for the same session. Only the
name survives the crossing, which is why it is prefixed with the remote host — a mirrored session
must not be mistakable for a local one — using a separator that cannot be a path component.
