# Mailbox operations

## One daemon per computer

```powershell
node dist/cli.js init --name Desktop
node dist/cli.js daemon
```

The daemon binds the peer receiver and a loopback control port. It writes `daemon.json` (mode 0600) with the control token. While that file answers `/health`, `agent-mail send` posts through the daemon and the daemon still uses `agent-mail/1` for delivery.

Register a session before publishing journal mail:

```powershell
node dist/cli.js register --session chat-1 --agent-name Builder --harness cursor
node dist/cli.js publish --session chat-1 --audience group --scope team --text "blocker: waiting on the schema"
node dist/cli.js digest --session chat-1
```

Pin a peer the same way as before (`trust --fingerprint`), then grant a scope and sync:

```powershell
node dist/cli.js sync <fingerprint> --endpoint http://192.0.2.20:47832
```

Only granted scopes are offered. An unpinned caller gets HTTP 403 and nothing is stored.

## Harness templates

From a project directory, not from the user profile:

```powershell
node dist/cli.js harness install --target D:\Projects\example --harness cursor
node dist/cli.js harness inspect --target D:\Projects\example
node dist/cli.js harness uninstall --target D:\Projects\example
```

`claude`, `codex`, `cursor`, and `agy` are accepted. Install refuses the user profile root and user-global MCP files (`~\.cursor\mcp.json`, `~\.claude.json`, `~\.codex\config.toml`, `~\.gemini`). Existing hook files that this installer did not create are left in place. Hook commands run `digest`, which prints a short untrusted summary and exits 0 when the journal is unavailable.

These templates were not executed inside Claude, Codex, Cursor, or Antigravity during this change. Cursor cloud agents were not checked.

## Dispatch

`agent-mail dispatch` answers typed queries, wakes the session that owns new peer mail, or runs a read-only responder ([ADR 0004](architecture/0004-dispatch-mailbox-decides-swarm-executes.md)). It does nothing until `policy.json` has `"dispatch": { "enabled": true, ... }` and lists the peer by fingerprint. The `mail-dispatch` skill documents the policy fields and the ledger lines.

Check the policy, then try one poll:

```powershell
node dist/cli.js policy check
node dist/cli.js dispatch --once
```

Run it as a scheduled task at logon, as yourself. Wakes and responder runs use your logged-in Claude, Codex or Cursor account, so the task must not run as another user or as SYSTEM:

```powershell
schtasks /Create /TN "agent-mail dispatch" /SC ONLOGON /TR "node <repo>\dist\cli.js dispatch"
```

Use a path without spaces, or wrap it in escaped quotes. The listener that owns the peer port is a separate process; dispatch only reads the stored inbox.

Prerequisites:

- A Claude wake needs the target session started with `claude --name <destination>`; `claude agents --json` must show exactly one session with that name.
- A Codex wake uses `codex queue --resume`, which needs Codex CLI 0.166 or later. On older builds the wake fails and a toast is shown instead.
- The responder needs `swarm_command` set to a built `synthet-agent-swarm` (its `run --request -` command). Enabling it sends that peer's message text to the responder harness's provider.

To stop it, set `"enabled": false` in the `dispatch` section. That takes effect on the next poll, without restarting the task. To remove it, run `schtasks /Delete /TN "agent-mail dispatch"`. Only one instance runs, through `dispatch.lock`; a lock left by a dead process is replaced.

## Rust cutover (per PC)

This is the T-10 step of [ADR 0005](architecture/0005-rust-mailer-tray-headless.md). It retires the TypeScript stack on one PC after the Rust tools have taken over its hooks and MCP. Each PC does its own cutover. Nobody changes another PC, and `dispatch.enabled` stays as the owner set it.

What is deployed now differs from the ADR's first plan. The Rust stack runs beside the TS one with its own home (`%USERPROFILE%\.agent-mail-rs`) and its own identity. The mailer is on TCP 47842 and the tray on 8796. Peers have pinned the Rust fingerprints at `:47842`. So the cutover moves no ports and changes no keys: it only stops and disables the TS entries. The ADR's "only one process can own 47832" applied to the plan where Rust took over 47832.

| | TypeScript (retired here) | Rust (stays) |
|---|---|---|
| Mailer | `node dist\cli.js listen` on 47832, started by a scheduled task whose name differs per PC (`\AgentMailListener` on ZEPHYR, `\agent-mail-listen` on STRIX), so find it by its action (step 1) | `agent-maild.exe` on 47842, task `\AgentMail\agent-maild-<id>` |
| Dispatch | `node dist\cli.js dispatch`, if a task runs it | the tray's consumer |
| Tray | `dist\AgentMailTray.exe` (`tray-host.js`, 8788), `Agent Mailbox.lnk` in Startup | `agent-mail-tray.exe` on 8796, task `\AgentMail\agent-mail-tray-<id>` |
| Hooks | `node …\dist\cli.js hook inject\|gate\|ledger`, `digest` | `agent-mail.exe --home <h> hook inject\|gate\|ledger`, `digest` |
| MCP `agent-mail` | `node …\dist\cli.js mcp` | `agent-mail.exe --home <h> mcp` |

Before you start: T-7 and T-10 must be merged. Build the three Rust binaries from that commit; a PC without a toolchain gets them from one that has one, with `SHA256SUMS` checked.

The new `mail.db` schema (v4) is one-way. An older `agent-mail`, `agent-maild` or `agent-mail-tray` refuses a v4 file ("schema version 4 is newer than this build"). That includes the MCP server that every open session runs. So upgrade all three binaries together:

1. Close every Claude Code and Codex session on the PC. Each one holds an `agent-mail.exe mcp` process.
2. Stop both `\AgentMail\` tasks: `Stop-ScheduledTask -TaskPath \AgentMail\ -TaskName <name>`.
3. Copy `mail.db`, `mail.db-wal` and `mail.db-shm` from the home into a backup folder.
4. Replace the three `.exe` files in `%LOCALAPPDATA%\Programs\agent-mail-rs`.
5. Start both tasks again.

`mail.db` moves to v4 the first time a new binary opens it. To roll back, put the old binaries and the backed-up `mail.db*` back in place.

```powershell
$bin = Join-Path $env:LOCALAPPDATA 'Programs\agent-mail-rs'; $cli = Join-Path $bin 'agent-mail.exe'
$h = Join-Path $env:USERPROFILE '.agent-mail-rs'; $ts = Join-Path $env:USERPROFILE '.agent-mail'
```

1. **Inventory (read-only).**
   - Record who owns 8788, 8796, 47832 and 47842, and which scheduled tasks and Startup entries start them.
   - Find the TS tasks by their action, not their name. Record the names for steps 7 and 8:
     ```powershell
     Get-ScheduledTask | Where-Object { ($_.Actions | ForEach-Object { "$($_.Execute) $($_.Arguments)" }) -match 'dist\\cli\.js.*(listen|dispatch)' } | Select-Object TaskPath, TaskName, State
     ```
   - Record every hook or MCP entry that runs `dist\cli.js`. Check `~\.claude\settings*.json`, project `.claude\settings*.json`, `~\.codex\hooks.json`, `~\.codex\config.toml`, `~\.cursor\mcp.json`, the Gemini and Antigravity MCP settings under `~\.gemini\`, and `claude mcp list`.
   - Check who still sends to the TS listener: look at the sender and date of the newest files in `$ts\inbox`. Every peer that still delivers to `:47832` must reach this PC's Rust mailer before step 7. Otherwise its mail queues on the peer.
2. **Import once.** `& $cli --home $h migrate --from $ts` copies contacts, inbox, acks, outbox, sent, and now also `journal.db` sessions and leases. It never changes `$ts`, and running it again imports nothing new. Then check `& $cli --home $h peers`. Contacts imported from TS carry the peers' TS fingerprints and `:47832` endpoints. Once every peer has cut over, the owner untrusts them.

   `migrate` doesn't touch `policy.json`. The Rust home needs its own, with `allowed_peers` naming the Rust peers. A copy of the TS file lists the TS fingerprints, so it doesn't work. If `& $cli --home $h policy check` fails, the owner writes it: start from `& $cli --home $h policy init` and add the peers.
3. **Seen state.** The inject hook shows each session the unacked mail it hasn't been shown, five messages per prompt, and imported mail counts as unacked. Both stacks keep seen state in `sessions\<sha256(session)>.seen.json` with the same `peer:<sender>:<id>` keys. So copy the TS files across, keeping any file the Rust home already has, and sessions don't see the backlog again:
   ```powershell
   New-Item -ItemType Directory -Force "$h\sessions" | Out-Null
   Get-ChildItem "$ts\sessions\*.seen.json" | Where-Object { -not (Test-Path "$h\sessions\$($_.Name)") } | Copy-Item -Destination "$h\sessions\"
   ```
   A session that never ran under TS still sees the unacked backlog once. Ack only what has really been handled.
4. **Hooks.** Replace each TS hook command that step 1 found with the Rust one. If the PC had no agent-mail hooks, adding them changes what every session sees, so that's the owner's choice; skip this step unless they ask. Keep the same events, and add to any hooks that already exist:
   - `"<bin>\agent-mail.exe" --home "<h>" hook inject --event SessionStart|UserPromptSubmit|Stop --harness claude|codex`
   - `… hook gate` (PreToolUse) and `… hook ledger` (PostToolUse)
   - `… digest --session <harness>`

   For a project, `& $cli --home $h harness install --target <project> --harness claude` writes `.claude\agent-mailbox.hooks.json`, which you then merge into `.claude\settings.json`; `codex` writes `.codex\hooks.json` directly. The template holds only the `inject` and `digest` commands, so add `hook gate` and `hook ledger` by hand. TS entries already merged into a `settings.json` stay there until you replace them.
5. **MCP.** Point `agent-mail` at the Rust binary and remove any second `agent-mail-rs` entry. For Claude Code: `claude mcp remove -s user agent-mail; claude mcp add -s user agent-mail -- $cli --home $h mcp --harness claude-code --agent-name claude`. For Codex, edit `[mcp_servers.agent-mail]` in `~\.codex\config.toml`. Do the same for every other entry step 1 found (Cursor, Gemini, Antigravity). Restart the sessions so they pick it up.
6. **Check before stopping TS.** In a fresh session, `whoami` must show the Rust fingerprint, and a test message from a peer must arrive in the Rust inbox. If hooks are wired, the message must also show up through the Rust `hook inject`. No peer may still depend on `:47832` (step 1). Don't stop TS until all of this passes.
7. **Stop and disable TS.**
   - Quit the TS tray: `dist\AgentMailTray.exe --quit`.
   - Move `Agent Mailbox.lnk` out of the Startup folder into a backup folder; don't delete it.
   - For each TS task from step 1 (the listener, and dispatch if there is one), run `Disable-ScheduledTask -TaskPath <path> -TaskName <name>`, then `Stop-ScheduledTask`. Stopping a task can leave its `node` running, so check the process that owns 47832. If its command line is `dist\cli.js listen`, stop that PID. Do the same for a `dist\cli.js dispatch` process.
   - Leave `$ts` in place.
8. **Receipts.** Nothing answers on 47832 or 8788. `http://127.0.0.1:47842/capabilities` and `http://127.0.0.1:8796/dashboard` answer. `claude mcp get agent-mail` shows `agent-mail.exe`. After a logon, only the Rust tasks start.

**Rollback:** re-enable and start the TS tasks recorded in step 1, put `Agent Mailbox.lnk` back, and point the hooks and MCP at `dist\cli.js` again. `migrate` changed nothing in `$ts`, so the TS stack resumes where it stopped. Mail received only by Rust meanwhile stays in `mail.db`.

## Backup

Copy, while the daemon is stopped:

- `identity.json` (private keys; keep it private)
- `journal.db`, `journal.db-wal`, `journal.db-shm`
- `blobs/`
- the JSON directories `contacts`, `inbox`, `acked`, `outbox`, `sent`
- `policy.json`, `ledger.jsonl` and `sessions/`. That folder holds the seen files, including the dispatcher's; without it, the next dispatch poll starts from now.

`node dist/cli.js` repair is not a separate command. Opening a `Journal` and calling `repair()` checks blob hashes.

## Rollback

1. Stop the daemon.
2. Delete `daemon.json` so `send` stops proxying.
3. Leave or delete `journal.db*` and `blobs/`. The JSON store is still the `agent-mail/1` mailbox.
4. Do not delete `identity.json` unless you intend to create a new host id.
5. Legacy `mailbox.db` is untouched and is not migrated.

Deleting a journal does not delete messages that a peer already stored. A later sync can deliver them again if the events are still on the peer; idempotent event ids keep them from applying twice on a journal that still has them.
