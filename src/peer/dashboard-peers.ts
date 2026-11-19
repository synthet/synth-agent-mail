import { randomUUID } from "node:crypto";
import { z } from "zod";
import { metadataSnapshot, type DashboardMeta } from "./dashboard-snapshot.js";
import { fingerprint, signed, verified, type Identity, type PublicIdentity } from "./identity.js";
import type { Contact } from "./store.js";
import { openData, sealData } from "./wire.js";

/**
 * Peers on each dashboard. A PC's dashboard server asks each pinned peer for metadata at
 * POST /dashboard/meta on the peer port. The request is signed by the asker's Ed25519 key and
 * names the recipient, a timestamp and a fresh nonce. The answer is sealed to the asker's X25519
 * key and signed by the responder. Only pinned peers get an answer, and it never carries message text.
 */
export const DASHBOARD_REQUEST = "agent-mail/1 dashboard-request";
export const DASHBOARD_META = "agent-mail/1 dashboard-meta";
export const DASHBOARD_PATH = "/dashboard/meta";
const MAX_SKEW_MS = 60_000;

const packetSchema = z.object({ payload: z.string().max(4_000), signature: z.string().max(128) }).strict();
const requestSchema = z.object({
  protocol: z.literal(DASHBOARD_REQUEST),
  sender: z.string().regex(/^[a-f0-9]{64}$/),
  recipient: z.string().regex(/^[a-f0-9]{64}$/),
  timestamp: z.number().int(),
  nonce: z.string().uuid(),
}).strict();

export function dashboardRequest(identity: Identity, recipient: PublicIdentity, nonce = randomUUID()) {
  const payload = JSON.stringify({ protocol: DASHBOARD_REQUEST, sender: fingerprint(identity), recipient: fingerprint(recipient), timestamp: Date.now(), nonce });
  return { nonce, packet: { payload, signature: signed(identity, payload) } };
}

/** Nonces seen in the last few minutes, so a captured request can't be replayed. */
export class NonceCache {
  private readonly seen = new Map<string, number>();
  constructor(private readonly ttlMs = 2 * MAX_SKEW_MS + 10_000, private readonly max = 10_000) {}
  /** True the first time a nonce is offered within the TTL. */
  claim(nonce: string, now = Date.now()): boolean {
    for (const [key, expiry] of this.seen) { if (expiry > now) break; this.seen.delete(key); }
    if (this.seen.has(nonce) || this.seen.size >= this.max) return false;
    this.seen.set(nonce, now + this.ttlMs);
    return true;
  }
}

export class DashboardRequestError extends Error {
  constructor(readonly status: 400 | 403, message: string) { super(message); }
}

/** Validate a peer's request and return the sealed metadata answer, or throw a DashboardRequestError. */
export function answerDashboardRequest(
  identity: Identity,
  home: string,
  trusted: (fingerprint: string) => PublicIdentity | undefined,
  input: unknown,
  nonces: NonceCache,
  meta: (home: string) => DashboardMeta = metadataSnapshot,
) {
  let request: z.infer<typeof requestSchema>;
  let packet: z.infer<typeof packetSchema>;
  try {
    packet = packetSchema.parse(input);
    request = requestSchema.parse(JSON.parse(packet.payload));
  } catch {
    throw new DashboardRequestError(400, "Malformed dashboard request");
  }
  const contact = trusted(request.sender);
  if (!contact || fingerprint(contact) !== request.sender || !verified(contact, packet.payload, packet.signature)) {
    throw new DashboardRequestError(403, "Untrusted sender or invalid signature");
  }
  if (request.recipient !== fingerprint(identity)) throw new DashboardRequestError(403, "Wrong recipient");
  if (Math.abs(Date.now() - request.timestamp) > MAX_SKEW_MS) throw new DashboardRequestError(403, "Expired request");
  if (!nonces.claim(request.nonce)) throw new DashboardRequestError(403, "Replayed request");
  return sealData(identity, contact, DASHBOARD_META, request.nonce, meta(home));
}

export type PeerDashboard = {
  name: string;
  fingerprint: string;
  endpoint: string | null;
  /** ok; unsupported = the peer's build has no dashboard endpoint; refused = it doesn't trust this PC. */
  status: "ok" | "unsupported" | "refused" | "offline" | "no-endpoint" | "invalid";
  meta?: DashboardMeta;
};

export async function fetchPeerDashboard(identity: Identity, contact: Contact, timeoutMs = 2500): Promise<PeerDashboard> {
  const base = { name: contact.identity.name, fingerprint: fingerprint(contact.identity).slice(0, 12), endpoint: contact.endpoint ?? null };
  if (!contact.endpoint) return { ...base, status: "no-endpoint" };
  const { nonce, packet } = dashboardRequest(identity, contact.identity);
  let response: Response;
  try {
    response = await fetch(new URL(DASHBOARD_PATH, contact.endpoint), {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(packet), signal: AbortSignal.timeout(timeoutMs),
    });
  } catch {
    return { ...base, status: "offline" };
  }
  if (response.status === 404) return { ...base, status: "unsupported" };
  if (response.status === 400 || response.status === 403) return { ...base, status: "refused" };
  if (!response.ok) return { ...base, status: "invalid" };
  try {
    const meta = openData(identity, contact.identity, DASHBOARD_META, nonce, await response.json()) as DashboardMeta;
    if (!meta || meta.protocol !== DASHBOARD_META) return { ...base, status: "invalid" };
    return { ...base, status: "ok", meta };
  } catch {
    return { ...base, status: "invalid" };
  }
}

export function fetchPeerDashboards(identity: Identity, contacts: Contact[], timeoutMs?: number): Promise<PeerDashboard[]> {
  return Promise.all(contacts.map((contact) => fetchPeerDashboard(identity, contact, timeoutMs)));
}
