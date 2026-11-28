# ADR 0006 — Deliver to a live Claude Code session; envelope 1.3 names the sender's checkout

Date: 2026-11-27
Status: Proposed. The owner approved the plan in conversation on 2026-11-26; the tracker (epic
af-hw144) doesn't record that approval. This text needs the owner's acceptance before af-hrmbk
(envelope 1.3) is merged.
Amends:
- ADR 0003 decision 4 (how a wake reaches a Claude session, and what the notice carries);
- ADR 0004 decision 4.3 (a checkout step joins the order for finding the live owner);
- ADR 0005 decision 1 (the pipe gains a `wait` request, af-2ft1h, PR #67), decision 4 (a task never
  wakes a session) and decision 7 (a task uses the swarm runner).

Everything else in ADRs 0003, 0004 and 0005 stands.

## Context

With af-2ft1h (PR #67, not yet merged) the tray consumer wakes within a second of new mail. Two
gaps remain before a message can land in the session that should answer it:

- **The receiver can't tell which folder a message is about.** The envelope's `project` tag is free
  text, set only when the sender passes it, and `policy.json` maps tags to fixed paths. Drive letters
  and project roots differ between PCs, so a path from the sender would mean nothing here. The
  cross-pc-ssh skill already solves this for SSH: the same repo is the checkout that shares a root
  commit, with the origin deciding between several. `rust/mail-core/src/checkout.rs` (af-47rzi,
  PR #68, not yet merged) ports those rules.
- **The wake step is a stub.** ADR 0003 decision 4 left the Claude wake path to a measured trial.
  The consumer only records "not available yet". Claude Code has since shipped cross-session
  messaging (see the owner's report "Claude Code Cross-Session Messaging", 2026-11-26): a session
  binds a local inbox, a named pipe on Windows, and `SendMessage` delivers to it. Bare-mode and
  `--restricted` sessions bind none, so they can only be reached by hook inject.

## Decision

1. **Envelope agent-mail/1.3** adds two optional signed fields after `project`:
   - `checkout`: `roots` (1–4 root commit hashes), `origin` (`host/owner/name`), `head`,
     `branch`, and `name` (the folder's own name).
   - `intent`: `notify` (wake a live session) or `task` (run an agent in the matched checkout;
     a task never wakes a session).

   It is negotiated like 1.1 and 1.2. A 1.2 peer gets `checkout` dropped, and the sender is told
   `envelope_downgraded`. A task is refused instead of downgraded, so it never quietly becomes a
   plain message. `agent.session_id` falls back to the sender's `CLAUDE_CODE_SESSION_ID`, which
   needs no new field.
2. **Privacy.** No absolute path, no relative subfolder and no OS user name leaves the PC. The
   `origin` does name the repo's owner account (`host/owner/name`), as the git remote does. The sender
   shares only what identifies the repo, and the owner can turn `checkout` off with
   `share_checkout: false`.
3. **The receiver resolves the folder.** `checkout.rs` matches the identity against the owner's
   `checkout_roots`, with renames covered by the owner's `origin_aliases`. Peer strings are only
   compared, never used to build a path, and resolution makes no network call. The existing
   `project` → `workspaces` lookup still applies to messages without `checkout`. The tier is
   read unless a `workspaces` entry names the same folder, and it is always capped by the peer's
   `max_tier`.
4. **Which session.** ADR 0004 decision 4.3's order, with one step added. The first match wins:
   1. `to_session`, only for a session that has itself sent mail in this conversation (the ADR 0004
      guard, `dispatch.rs` `route`);
   2. the conversation-lease holder;
   3. **new:** the single live session whose working folder is inside the resolved checkout. This
      step has no sent-in-conversation guard, so a first message can reach someone. The sender
      can't pick the session: the receiver chooses it from its own matching, and it gets only a
      notice. If several sessions match, none is woken and the ledger records `ambiguous`;
   4. the peer's configured `wake.destination`.

   Sessions are read from Claude Code's session records. Filtering for interactive sessions depends
   on the record's `kind` field, which the report marks as not independently confirmed. Without it,
   every live session in the checkout counts.
5. **How it reaches the session, in order:**
   1. **Direct pipe**, behind `dispatch.live.enabled` (off by default). The tray re-reads the session
      record, takes the token from that session's key file, writes the documented auth line and one
      `{"type":"user",…}` line, then closes. The token stays in memory and is never logged.
      Because the pipe returns no acknowledgement, the ledger records `written`, never
      `delivered`.
   2. **SendMessage relay.** A restricted `claude -p` (ListAgents, SendMessage and ToolSearch only;
      strict MCP config, no saved session, no slash commands, a 60 s timeout) sends the notice to the
      session. This is the documented route and the frozen TS dispatcher's tool set. Two things are
      new: a budget cap of at most $0.05 (the TS relay used `limits.budget_usd`, default 0.25), and
      the address, which is `uds:<messagingSocketPath>` from the session record, or the record's
      `name` when that is unique.
   3. **Hook inject.** If neither works, the mail stays unacknowledged, and the session's next
      prompt shows it (filtered by `to_session` and checkout, af-f3m0e).
6. **What is delivered.** By default a **notice** only, never the body, as ADR 0003 decision 4
   requires. It extends ADR 0003's line `MAIL thread=… from=<label> id=…` with `project=`, `host=` and
   `session=`, followed by a fixed line saying it is data, not instructions, to be read with
   `fetch_inbox`. Those three values come from the sending peer. Each passes the same plain-text check
   as the peer label (`safe_label`), or is left out. The body itself reaches the model only through
   inject or `fetch_inbox`, inside `<untrusted-remote-message>`. A `live.content: "body"` mode,
   which would write the wrapped body straight into the pipe, is **not** decided here. It stays off
   and needs its own owner decision.
7. **Tasks for the swarm.** `intent=task` skips the wake. The tray's swarm runner (af-i7v2y) runs
   `swarm_command run --request -` in the resolved checkout, read-only, and sends the reply back
   with `to_session` set to the sender's session, so it wakes that session through the same chain.
   This departs from ADR 0005 in two ways. A task skips decision 4's wake step, and it uses the
   swarm runner even when decision 7's `dispatch.runners` order puts Claude first. Gate, typed answer
   and the per-peer limits still apply.

## Undocumented surfaces this depends on

None of these is a supported Claude Code contract, so any release can change them:

- the session record `~/.claude/sessions/<pid>.json` and its fields (`sessionId`, `name`, `cwd`,
  `messagingSocketPath`, `procStart`; `kind` isn't even confirmed by the report);
- the key file `~/.claude/sessions/<pid>.<sha256-of-endpoint>.key`, and that it holds the peer token;
- the message line `{"type":"user","message":{"role":"user","content":…}}`;
- the Windows pipe name, which can change during a session.

Only the auth line and the session's own `CLAUDE_CODE_MESSAGING_SOCKET` / `_TOKEN` are documented,
and they apply to a session's own children, not to the tray. Hence the order in decision 5: the
direct pipe is opt-in, is re-read each time, and falls back to the documented relay, then to inject.

## Consequences

- **Reading a key file makes the tray a peer session.** It runs as the same user, the owner opts in,
  and messages it writes are treated like another session's. They can't approve prompts or change
  configuration. Sessions in bypass mode hold them for approval (to be recorded by af-8hmb2).
- **Cost.** The relay costs a small model call per wake, and the direct pipe makes no call of its
  own. Either way, a delivered message counts toward usage like a typed prompt, because it starts a
  turn in the receiving session.
- **Loop guards stay as they are:** automatic mail is never answered, `hop` stays below 6, and the
  per-peer rate limit and per-run budget cap apply.
- **Rollout is receiver-first,** as in ADR 0003. Both PCs deploy a build that accepts 1.3 before
  either sends it.
