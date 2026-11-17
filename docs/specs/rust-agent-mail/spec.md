# Spec — Rust agent-mail: always-on mailer, tray consumer, headless agents

Status: approved by the owner (2026-11-16). Architecture: ADR 0005 (to be written, T-0).

## Problem

Every PC runs the TypeScript peer today:
- The C# tray starts `node dist/tray-host.js`, which serves the dashboard and the receiver.
- Each MCP process does its own discovery and posts straight to remote peers.

Gaps:
- Received mail is handled automatically only if `agent-mail dispatch` was started separately, and its responder is read-only.
- The envelope carries no project tag, so a receiver can't pick a workspace.
- There is no static host list for when multicast discovery fails.

## Goal

Message flow:

```
MCP/CLI → mail.db outbox → agent-maild (send) → LAN → agent-maild (receive) → mail.db inbox
        → tray consumer → headless agent in an allowed folder → reply → outbox → agent-maild → LAN → sender
```

The pieces:
- **`agent-maild`.** An always-on, lightweight mailer, started at user logon. It is the only process that touches the network: it receives, discovers peers and drains the outbox.
- **`AgentMailTray.exe`.** It owns the dashboard and consumes the inbox. It runs a headless agent (Claude first, then swarm harnesses, then an API key) in the folder that the message's `project` tag selects, or in the owner-configured default folder.
- **`agent-mail.exe`.** The CLI. Its `mcp` subcommand is what `mcp.json` launches over stdio.
- All three are written in Rust and stay wire-compatible with the TypeScript peers (`agent-mail/1`, `agent-mail/1.1`).

## Non-goals (this epic)

- Journal events and sync (`agent-mail/2`), task authority, A2A, blobs. These stay deferred.
- Resuming a live interactive session from a headless run.
- Edit tier for harnesses other than Claude.
- Model-based routing, and sending message text to Jev or TypeSafe.

## Acceptance criteria

- **AC-1.** WHEN `mail-core` signs, seals or receipts a message THEN the TypeScript `verifyMessage`, `unseal` and `verifyReceipt` accept it. WHEN the TypeScript side does the same THEN `mail-core` accepts it. Fingerprints and receipt digests are byte-identical on golden vectors.
- **AC-2.** WHEN a TypeScript peer posts to `agent-maild` `/message` THEN the message is stored in the `mail.db` inbox and the sender verifies the returned receipt. `/identity` and `/capabilities` answer in the TypeScript format.
- **AC-3.** WHEN `agent-maild` runs THEN it answers and sends multicast discovery packets in the `src/peer/discovery.ts` format, and it probes every `hosts.json` entry with `GET /identity`. A discovered or listed host stays untrusted until it is pinned.
- **AC-4.** WHEN the CLI or MCP sends THEN the message is written to the outbox and `agent-maild` delivers it, retrying with backoff. WHEN the mailer is not running THEN `send` reports `queued` (exit code 2), and the message is delivered after the mailer starts.
- **AC-5.** WHEN `check_outbound` rejects a text (secret patterns, `max_bytes`, `allowed_peers`, action-request patterns) THEN no outbox row is written. This holds for every send path, replies included.
- **AC-6.** WHEN the recipient advertises `agent-mail/1.2` THEN the signed `project` field is sent. Otherwise it is dropped and the result reports `envelope_downgraded`. The Rust `/capabilities` lists both 1.1 and 1.2.
- **AC-7.** `agent-mail mcp` serves the stdio MCP tools with the same names and parameters as `src/peer/mcp.ts`, plus an optional `project` on `send_message`. It never binds a network port.
- **AC-8.** `agent-mail migrate` imports the JSON `contacts`, `inbox`, `acked`, `outbox` and `sent` areas, and the journal's sessions and leases, into `mail.db` without deleting or changing the originals. Running it twice imports nothing new.
- **AC-9.** The tray serves the dashboard on loopback only and checks the Host header. It shows the mailer status, hosts, inbox and runs. Its only write is the kill switch, which changes `dispatch.enabled` and nothing else.
- **AC-10.** `agent-mail service install` and `agent-mail tray install` create logon tasks for the current user, not SYSTEM. `uninstall` removes them.
- **AC-11.** The consumer applies the gates from `src/fabric/dispatch.ts`, in order: peer listed, rate limit, hop < 6, no automatic reply to `auto_reply`, one automatic reply per message, then typed answer, then wake the live owner, then headless run. Every decision writes one ledger line that stores message bodies only as hashes.
- **AC-12.** WHEN `project` exactly matches a `policy.workspaces` key THEN that folder is the working directory. ELSE IF `default_workspace` is set THEN that folder is used. ELSE the message is skipped with `no_workspace`. Tag text is never used as a path.
- **AC-13.** The effective tier is min(folder tier, peer `max_tier`):
  - `read` gets Read, Grep and Glob.
  - `reply` adds the run-bound agent-mail tools.
  - `edit` adds Edit, Write and Bash, and only for Claude.

  Remote text never sets the folder, tier, runner, model or recipient.
- **AC-14.** A run's reply is taken from the first of these that exists: (a) a run-bound reply call, (b) a non-empty `reply.md`, (c) the final stdout `result`. It is sent with `auto_reply=true`, `hop+1`, and `in_reply_to` derived from the stored message.
- **AC-15.** WHEN `agent-mail reply` or MCP `reply_to_message` runs in run mode THEN it checks `AGENT_MAIL_RUN_TOKEN` against the run row and derives the recipient and thread from the stored message. A wrong token or a second reply is refused.
- **AC-16.** Runners are tried in `dispatch.runners` order, falling back on `not_installed`, `auth_required` and `rate_limited`. A timeout kills the whole process tree. Swarm results count only when `outcome == "ok"`.
- **AC-17.** At reply tier, the headless agent can list same-PC sessions and publish to them through `mail.db`. Wakes carry only a metadata notice. A live interactive session is never resumed.
- **AC-18.** ADR 0005 records what it supersedes in ADR 0004 (decisions 1 and 5, "swarm is the only spawner") and what it keeps.
- **AC-19.** The README, `docs/operations.md`, the `agent-mailbox` and `mail-dispatch` skills and the `config/*mcp.example.json` files describe the Rust binaries.
