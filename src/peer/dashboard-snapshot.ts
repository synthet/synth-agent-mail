import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import Database from "better-sqlite3";
import { fingerprint, validatePublic, type PublicIdentity } from "./identity.js";
import type { Contact } from "./store.js";
import type { PeerMessage } from "./wire.js";
function loadPublic(home: string): PublicIdentity {
  const raw = JSON.parse(readFileSync(join(home, "identity.json"), "utf8")) as Record<string, unknown>;
  return validatePublic({ name: raw.name, signing_key: raw.signing_key, encryption_key: raw.encryption_key });
}

/** Read the peer store without creating directories, so the dashboard never writes to it. */
function readArea<T>(home: string, area: string): T[] {
  let names: string[];
  try { names = readdirSync(join(home, area)); }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return []; throw error; }
  return names.filter((name) => name.endsWith(".json")).flatMap((name) => {
    try { return [JSON.parse(readFileSync(join(home, area, name), "utf8")) as T]; }
    catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return []; throw error; }
  });
}

function readAcked(home: string, id: string): boolean {
  const file = createHash("sha256").update(id).digest("hex") + ".json";
  try { readFileSync(join(home, "acked", file)); return true; }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return false; throw error; }
}

interface JournalRead {
  agents: { id: string }[];
  messages: { created_at: number; sender: string; text: string; kind: string; thread_id: string }[];
  messageCount: number;
  tasks: Record<string, number>;
}

/**
 * Read journal.db without creating files next to it.
 * The journal runs in WAL mode, and even a read-only SQLite connection to a WAL database needs
 * the -shm file, so read a private copy of the database and its WAL instead.
 */
function readJournal(home: string): JournalRead | null {
  const path = join(home, "journal.db");
  if (!existsSync(path)) return null;
  const copyDir = mkdtempSync(join(tmpdir(), "agent-mail-journal-"));
  const copy = join(copyDir, "journal.db");
  copyFileSync(path, copy);
  if (existsSync(`${path}-wal`)) copyFileSync(`${path}-wal`, `${copy}-wal`);
  let db: Database.Database;
  try { db = new Database(copy, { readonly: true, fileMustExist: true }); }
  catch (error) { rmSync(copyDir, { recursive: true, force: true }); throw error; }
  try {
    db.pragma("query_only = ON");
    const sessions = db.prepare(`SELECT a.alias AS alias, COALESCE(s.harness, '') AS harness
      FROM sessions s JOIN agents a ON a.agent_id = s.agent_id
      ORDER BY s.last_seen DESC`).all() as { alias: string; harness: string }[];
    const agents = sessions.length
      ? sessions.map((row) => ({ id: row.harness ? `${row.alias} · ${row.harness}` : row.alias }))
      : db.prepare("SELECT alias AS id FROM agents ORDER BY alias").all() as { id: string }[];
    const messages = db.prepare(`SELECT m.created_at, m.body AS text, m.audience_kind AS kind, m.thread_id,
        COALESCE(a.alias, m.sender_agent, substr(m.sender_host, 1, 12)) AS sender
      FROM messages m LEFT JOIN agents a ON a.agent_id = m.sender_agent
      WHERE m.deleted = 0 ORDER BY m.created_at DESC LIMIT 40`).all() as JournalRead["messages"];
    const messageCount = (db.prepare("SELECT COUNT(*) AS n FROM messages WHERE deleted = 0").get() as { n: number }).n;
    const tasks: Record<string, number> = {};
    for (const row of db.prepare("SELECT state, COUNT(*) AS n FROM tasks GROUP BY state").all() as { state: string; n: number }[]) {
      tasks[row.state] = row.n;
    }
    return { agents, messages, messageCount, tasks };
  } finally {
    db.close();
    rmSync(copyDir, { recursive: true, force: true });
  }
}

/**
 * What a pinned peer may see of this PC: counts, names, times and ids. Built field by field from an
 * allowlist, so no message text, log line or error detail can leak into it.
 */
export function metadataSnapshot(home: string) {
  const view = snapshot(home) as ReturnType<typeof snapshot> & { remote: { journal?: { messages: number; tasks: Record<string, number> } } };
  const count = (value: unknown) => typeof value === "number" && Number.isFinite(value) ? value : 0;
  return {
    protocol: "agent-mail/1 dashboard-meta",
    name: String(view.agent.id),
    generated_at: view.generated_at,
    journal: view.journal === true,
    counts: {
      unread: count(view.remote.inbox.unread),
      stored: count(view.remote.inbox.stored),
      sent: count(view.remote.tasks.delivered),
      queued: count(view.local.pending),
      journal_messages: count(view.remote.journal?.messages),
    },
    tasks: Object.fromEntries(Object.entries(view.remote.journal?.tasks ?? {}).map(([state, n]) => [String(state).slice(0, 40), count(n)])),
    agents: view.agents.agents.slice(0, 50).map((agent) => ({ id: String(agent.id).slice(0, 120), source: String(agent.source).slice(0, 20) })),
    recent: view.recent.slice(0, 20).map((row) => ({
      at: String(row.message.created_at),
      source: String(row.source),
      status: String(row.status),
      type: String(row.message.type).slice(0, 40),
      peer: String(row.message.sender).slice(0, 120),
      conversation_id: String(row.message.conversation_id ?? "").slice(0, 80),
    })),
  };
}
export type DashboardMeta = ReturnType<typeof metadataSnapshot>;

/** Full local view for this PC's own dashboard. Contains message text: never send it to another PC. */
export function snapshot(home: string) {
  const self = loadPublic(home);
  const selfFingerprint = fingerprint(self);
  const contacts = readArea<Contact>(home, "contacts");
  const names = new Map<string, string>([[selfFingerprint, self.name]]);
  for (const contact of contacts) names.set(fingerprint(contact.identity), contact.identity.name);
  const nameOf = (id: string) => names.get(id) ?? id.slice(0, 12);

  const inbox = readArea<PeerMessage>(home, "inbox").sort((a, b) => b.created_at - a.created_at);
  const sent = readArea<PeerMessage>(home, "sent").sort((a, b) => b.created_at - a.created_at);
  const outbox = readArea<PeerMessage>(home, "outbox").sort((a, b) => b.created_at - a.created_at);
  const unread = inbox.filter((message) => !readAcked(home, message.sender + ":" + message.id)).length;

  const recent = [...inbox.map((message) => ({ message, status: readAcked(home, message.sender + ":" + message.id) ? "acked" : "new", direction: "inbox" })),
    ...sent.map((message) => ({ message, status: "delivered", direction: "sent" }))]
    .sort((a, b) => b.message.created_at - a.message.created_at)
    .slice(0, 40);

  const peerLog = [...inbox.map((message) => ({ message, direction: "from" })), ...sent.map((message) => ({ message, direction: "to" })), ...outbox.map((message) => ({ message, direction: "queued" }))]
    .map(({ message, direction }) => ({
      at: message.created_at,
      line: `${new Date(message.created_at).toISOString()} peer ${direction} ${nameOf(direction === "to" || direction === "queued" ? message.recipient : message.sender)} ${message.text}`,
    }));
  const joinLog = (lines: { at: number; line: string }[]) => lines.sort((a, b) => a.at - b.at).slice(-40).map((entry) => entry.line).join("\n");
  const log = joinLog(peerLog);

  const view = {
    mode: "peer" as const,
    journal: false,
    generated_at: new Date().toISOString(),
    agent: { id: self.name },
    mailbox: { url: "peer", candidates: ["peer"], connected: true, fatal: null },
    wake: { mode: "off", client: "peer" },
    agents: { agents: [{ id: self.name, source: "peer" }, ...contacts.map((contact) => ({ id: contact.identity.name, source: "peer" }))] },
    agents_error: null,
    remote: { inbox: { unread, stored: inbox.length }, tasks: { delivered: sent.length } },
    remote_error: null,
    local: { pending: outbox.length },
    recent: recent.map(({ message, status, direction }) => ({
      status,
      source: "peer",
      message: {
        created_at: new Date(message.created_at).toISOString(),
        sender: direction === "sent" ? nameOf(message.recipient) : nameOf(message.sender),
        type: direction,
        body: { text: message.text },
        conversation_id: message.conversation_id,
      },
    })),
    outbox: outbox.slice(0, 40).map((message) => ({
      created_at: new Date(message.created_at).toISOString(),
      recipient: nameOf(message.recipient),
      type: "peer",
      status: "queued",
      attempts: 0,
      last_error: "",
    })),
    log,
  };
  const journal = readJournal(home);
  if (!journal) return view;
  // Merge the journal (same-PC mailbox) into the peer view instead of replacing it (#16), so a stale
  // journal.db can't hide peer mail. Every row says which store it came from.
  const journalRecent = journal.messages.map((message) => ({
    status: "new",
    source: "journal",
    message: {
      created_at: new Date(message.created_at).toISOString(),
      sender: message.sender,
      type: message.kind,
      body: { text: message.text },
      conversation_id: message.thread_id,
    },
  }));
  const seenAgents = new Set(view.agents.agents.map((agent) => agent.id));
  const journalAgents = journal.agents.filter((agent) => !seenAgents.has(agent.id)).map((agent) => ({ id: agent.id, source: "journal" }));
  return {
    ...view,
    journal: true,
    agents: { agents: [...view.agents.agents, ...journalAgents] },
    remote: { ...view.remote, journal: { messages: journal.messageCount, tasks: journal.tasks } },
    recent: [...view.recent, ...journalRecent]
      .sort((a, b) => Date.parse(b.message.created_at) - Date.parse(a.message.created_at))
      .slice(0, 40),
    log: joinLog([...peerLog, ...journal.messages.map((message) => ({
      at: message.created_at,
      line: `${new Date(message.created_at).toISOString()} journal ${message.kind} ${message.sender} ${message.text}`,
    }))]),
  };
}
