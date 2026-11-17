# ADR 0005 — Rust mailer, tray consumer and headless agents in project folders

Date: 2026-11-17
Status: Proposed. The owner approved the design on 2026-11-16; this text is under review.
Spec: [`docs/specs/rust-agent-mail/spec.md`](../specs/rust-agent-mail/spec.md).

## Context

ADR 0004 made `agent-mail dispatch` a separate poller. It answers typed queries, wakes the owning
session, and otherwise runs a read-only swarm responder. Four gaps are left:

- **Mail is handled only while someone runs `dispatch` by hand.** The tray starts the dashboard and
  the receiver, not the dispatcher.
- **Every MCP process is its own network client.** It runs discovery, probes `/capabilities`, POSTs
  to peers and flushes its own outbox. A message queued by a session that has ended waits until
  some other process happens to flush.
- **The envelope carries no project tag.** A receiver can't tell which workspace a message is about,
  and `policy.json` maps only one folder per peer.
- **A responder can't do work.** Its tools are limited to Read, Grep and Glob, with no MCP.

The owner wants each PC to run an always-on, lightweight service that accepts mail. The tray app
should then hand the mail to a headless agent, running in the folder that the message's project tag
selects. That agent replies to the sender and can reach sessions on the same PC. Everything is to be
built in Rust, and the MCP server becomes a native CLI.

## Decision

1. **Three binaries with one database.**
   - `agent-maild` is the mailer, started at user logon. It is the only process that uses the
     network:
     - it receives on TCP 47832;
     - it answers and sends discovery on UDP 47831;
     - it probes Tailscale and `hosts.json`;
     - it drains the outbox.
   - `AgentMailTray.exe` runs the loopback dashboard and consumes the inbox.
   - `agent-mail.exe` is the CLI. `agent-mail mcp` is the stdio MCP server that `mcp.json` launches.
   - They share `~/.agent-mail/mail.db` (SQLite, WAL) on local disk, never in a synced folder
     (ADR 0002).
   - A named pipe only pokes and reports status. The fallback is polling `mail.db`.
2. **The mailer is the only producer on the wire.** The CLI, MCP and the tray write outbox rows, and
   the mailer delivers them. When the mailer is down, a send returns `queued` and goes out once the
   mailer starts. The tray has no network code.
3. **Wire compatibility.** The Rust code speaks `agent-mail/1` and 1.1 byte-for-byte:
   - fingerprints over the stored PEM text;
   - signatures over sorted-key canonical JSON;
   - receipt digests over the message serialized in zod schema order.

   Envelope 1.2 adds one optional signed field, `project`. The mailer advertises it at
   `/capabilities` and drops it for a peer that does not list 1.2, reporting `envelope_downgraded`,
   the same as the 1.1 path in ADR 0003.
4. **The tray is the consumer.** This supersedes ADR 0004 decision 1, which set up a separate
   `dispatch` process.
   - The consumer runs inside the tray, still apart from the receiver, so a slow or crashed consumer
     never costs a delivery or a receipt.
   - It keeps ADR 0004's decision order: gate, typed answer, wake the live owner, then a headless
     run in place of the read-only responder.
5. **The project tag selects a folder from policy, never a path.**
   - `policy.workspaces` maps a tag to `{path, tier}`.
   - An exact key match wins. Otherwise `default_workspace` is used (for example the owner's synced
     root), and without it the message is stored and skipped as `no_workspace`.
   - Tag text is never joined into a path.
6. **Permission tiers per folder.** This supersedes ADR 0004 decision 5, under which the responder
   can't act.
   - The effective tier is min(folder tier, the peer's `max_tier`).
   - `read`: Read, Grep and Glob. The tray collects the reply.
   - `reply`: adds the run-bound agent-mail tools (reply, list sessions, publish to a local session).
   - `edit`: adds Edit and Write in the folder. It exists only for Claude at first.
   - No tier includes Bash. Claude's Bash tool runs commands as the user and is not confined to
     the working directory. A command tier would need its own decision.
   - Every tier gets generated deny rules for reading or editing the agent-mail home
     (`identity.json`, `policy.json`, `mail.db`).
   - The default is `read`. `edit` must be set on both the folder and the peer.
7. **The tray starts headless agents.** This supersedes ADR 0004's rule that swarm is the only
   place that starts a harness.
   - Runners are tried in the `dispatch.runners` order:
     1. Claude, as `claude -p` with tier-derived `--tools`/`--allowedTools`,
        `--strict-mcp-config`, a budget cap and the folder as cwd;
     2. `swarm:<harness>`, the existing `run --request -` contract, read tier only, accepted only
        when `outcome == "ok"`;
     3. `api`, the Anthropic Messages API with a key from the environment or a protected store,
        text only.
   - A runner that is not installed, not logged in or rate-limited falls through to the next. A
     timeout kills the process tree.
8. **Replies are bound to the run.**
   - Each run gets a row in `runs`, a random token and a run folder holding `request.md` (the remote
     text inside `<untrusted-remote-message>`) and `reply.md`.
   - The reply is taken from the first of these that exists:
     1. a run-bound `agent-mail reply` or MCP `reply_to_message` call;
     2. a non-empty `reply.md`;
     3. the final stdout `result`.
   - The recipient, thread and `in_reply_to` come from the stored message, never from the agent.
     There is one reply per run.
9. **Outbound checks move into the send path.** `check_outbound` (secret patterns, size, allowed
   peers) runs inside the Rust send code for every send, replies included. Today only the PreToolUse
   hook checks, and `reply_to_message` is not covered by it.

## Kept from ADR 0003 and ADR 0004

- Dispatch is off by default, per peer, and configured only from `policy.json`, which only the owner
  may edit.
- Remote text never configures the folder, tier, runner, model or recipient.
- Loop guards:
  - `hop < 6`;
  - no automatic reply to an `auto_reply` message;
  - one automatic reply per message;
  - every automatic reply carries `auto_reply=true` and `hop + 1`.
- Rate and budget caps, and the `<untrusted-remote-message>` wrapper.
- `ledger.jsonl` records one line per decision, with bodies only as sha256.
- Messages stay unacknowledged, so the owner still sees them.
- Wakes carry only the metadata notice, and a live interactive session is never resumed.
- Enabling a run for a peer sends that peer's text to the runner's provider. The owner opts into
  this per peer. No message text goes to Jev or TypeSafe.

## Not doing (now)

- Porting journal events and sync (`agent-mail/2`), tasks, A2A and blobs. The TS journal stays as
  it is until a later decision.
- Edit tier for Codex, Cursor or ACP harnesses. Swarm's respond mode is read-only.
- A tray loopback send API. The CLI and MCP write to `mail.db` directly.
- A Windows service running before logon. Per-user logon tasks keep the keys and folders under the
  owner's account.

## Consequences

- Mail is handled whenever the tray runs and is delivered whenever the PC is on. A session that has
  ended no longer leaves mail stranded in the outbox.
- The trust boundary is still the agent-mail processes plus `policy.json`, and `policy.json` now
  also grants write access to folders. Keep it owner-writable only and deny agent edits to it.
- An `edit` folder lets a paired peer's message cause file changes in that folder, within the budget.
  That is the owner's opt-in per folder and per peer. T-8 must show with a test that a headless
  edit-tier run cannot write outside the folder or touch the agent-mail home. Until that test passes,
  `edit` is not enabled for any folder.
- The hooks already installed in sessions call the TS `hook inject` against the JSON store. Until
  T-10 ports them, the Rust binaries run on a separate `--home`. The per-PC cutover is T-10 plus
  `migrate`.
- On a PC, the TS and Rust stacks must not both run at once, because only one process can own TCP
  47832. A TS peer on another PC interoperates over 1.1.
- Rollback: stop `agent-maild` and the Rust tray, then start the TS tray. `migrate` leaves the JSON
  store and `journal.db` untouched.

## Sequencing

Tickets T-0 to T-9 in [`docs/specs/rust-agent-mail/tasks.md`](../specs/rust-agent-mail/tasks.md):
1. this ADR;
2. `mail-core` with golden vectors;
3. the mailer receive side;
4. the outbox and the CLI;
5. MCP;
6. the tray;
7. the consumer at read tier;
8. reply tier;
9. edit tier and the other runners;
10. docs and retiring the TS stack.
