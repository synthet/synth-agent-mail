import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { createServer, request } from "node:http";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { answerDashboardRequest, dashboardRequest, fetchPeerDashboard, NonceCache, DASHBOARD_PATH } from "../dist/peer/dashboard-peers.js";
import { startPeerDashboard } from "../dist/peer/dashboard-server.js";
import { fingerprint, initIdentity, publicIdentity, signed } from "../dist/peer/identity.js";
import { Peer } from "../dist/peer/peer.js";
import { signMessage } from "../dist/peer/wire.js";

const SECRET = "SECRET-BODY-must-never-leave-this-pc";

/** Two homes, Alpha and Beta. Beta listens on a free port; pairing is up to the test. */
async function setup(t) {
  const dir = mkdtempSync(join(tmpdir(), "dash-peers-"));
  const a = new Peer(initIdentity(join(dir, "a"), "Alpha"), join(dir, "a"));
  const b = new Peer(initIdentity(join(dir, "b"), "Beta"), join(dir, "b"));
  const listener = await b.listen({ port: 0, bind: "127.0.0.1", discovery: false });
  const endpoint = `http://127.0.0.1:${listener.port}`;
  t.after(async () => { await listener.close(); rmSync(dir, { recursive: true, force: true }); });
  // Beta holds one message with text that must stay on Beta.
  const message = signMessage(a.identity, {
    id: randomUUID(), conversation_id: randomUUID(), sender: fingerprint(a.identity), recipient: fingerprint(b.identity),
    text: SECRET, created_at: Date.now() - 1000, origin: { hostname: "alpha", platform: "test", arch: "x64", addresses: [] },
  });
  b.store.put("inbox", message.sender + ":" + message.id, message);
  return { dir, a, b, endpoint };
}
const pinBoth = ({ a, b, endpoint }) => {
  a.store.trust(publicIdentity(b.identity), fingerprint(b.identity), endpoint);
  b.store.trust(publicIdentity(a.identity), fingerprint(a.identity));
};
const post = (endpoint, body) => fetch(endpoint + DASHBOARD_PATH, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });

test("a pinned peer gets sealed metadata, and no message text", async (t) => {
  const env = await setup(t);
  pinBoth(env);
  const result = await fetchPeerDashboard(env.a.identity, env.a.store.contact("Beta"));
  assert.equal(result.status, "ok");
  assert.equal(result.meta.name, "Beta");
  assert.equal(result.meta.counts.unread, 1);
  assert.equal(result.meta.recent.length, 1);
  assert.equal(result.meta.recent[0].peer, "Alpha");
  assert.ok(!JSON.stringify(result).includes(SECRET), "metadata must not carry message text");
  // On the wire the answer is ciphertext too.
  const { packet } = dashboardRequest(env.a.identity, env.b.identity);
  const raw = await (await post(env.endpoint, packet)).text();
  assert.ok(!raw.includes(SECRET) && !raw.includes("Alpha"));
});

test("a peer that hasn't pinned the asker refuses", async (t) => {
  const env = await setup(t);
  env.a.store.trust(publicIdentity(env.b.identity), fingerprint(env.b.identity), env.endpoint);
  assert.equal((await fetchPeerDashboard(env.a.identity, env.a.store.contact("Beta"))).status, "refused");
});

test("replayed, expired, misaddressed and forged requests are refused", async (t) => {
  const env = await setup(t);
  pinBoth(env);
  const { packet } = dashboardRequest(env.a.identity, env.b.identity);
  assert.equal((await post(env.endpoint, packet)).status, 200);
  assert.equal((await post(env.endpoint, packet)).status, 403, "replay");

  const trusted = (id) => env.b.store.get("contacts", id)?.identity;
  const forge = (fields) => {
    const payload = JSON.stringify({ protocol: "agent-mail/1 dashboard-request", sender: fingerprint(env.a.identity), recipient: fingerprint(env.b.identity), timestamp: Date.now(), nonce: randomUUID(), ...fields });
    return { payload, signature: signed(env.a.identity, payload) };
  };
  const answer = (packet) => answerDashboardRequest(env.b.identity, env.b.store.dir, trusted, packet, new NonceCache());
  assert.throws(() => answer(forge({ timestamp: Date.now() - 120_000 })), /Expired/);
  assert.throws(() => answer(forge({ recipient: "0".repeat(64) })), /Wrong recipient/);
  const tampered = forge({});
  assert.throws(() => answer({ ...tampered, payload: tampered.payload.replace("dashboard-request", "dashboard-request ") }), /Malformed|Untrusted/);
  assert.throws(() => answer({ ...forge({}), signature: Buffer.alloc(64).toString("base64") }), /Untrusted/);
  assert.throws(() => answer("not a packet"), /Malformed/);
});

test("an older peer build answers 404 and shows as unsupported; a dead address as offline", async (t) => {
  const env = await setup(t);
  const old = createServer((_req, res) => res.writeHead(404).end());
  await new Promise((resolve) => old.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => old.close(resolve)));
  const contact = { identity: publicIdentity(env.b.identity), endpoint: `http://127.0.0.1:${old.address().port}` };
  assert.equal((await fetchPeerDashboard(env.a.identity, contact)).status, "unsupported");
  assert.equal((await fetchPeerDashboard(env.a.identity, { ...contact, endpoint: "http://127.0.0.1:1" }, 500)).status, "offline");
  assert.equal((await fetchPeerDashboard(env.a.identity, { identity: contact.identity })).status, "no-endpoint");
});

test("/api/peers on the local dashboard lists pinned peers; it stays loopback-only", async (t) => {
  const env = await setup(t);
  pinBoth(env);
  const dashboard = await startPeerDashboard({ home: env.a.store.dir, port: 0, bind: "127.0.0.1" });
  t.after(() => dashboard.close());
  const url = dashboard.url.replace(/\/dashboard$/, "/api/peers");
  const body = await (await fetch(url)).json();
  assert.equal(body.peers.length, 1);
  assert.equal(body.peers[0].status, "ok");
  assert.ok(!JSON.stringify(body).includes(SECRET));
  // fetch() ignores a Host override, so use http.request to simulate a DNS-rebinding page.
  const status = await new Promise((resolve, reject) => {
    const req = request(url, { headers: { host: "evil.example:80" } }, (res) => { res.resume(); resolve(res.statusCode); });
    req.on("error", reject);
    req.end();
  });
  assert.equal(status, 403);
  await assert.rejects(startPeerDashboard({ home: env.a.store.dir, port: 0, bind: "0.0.0.0" }), /loopback only/);
});
