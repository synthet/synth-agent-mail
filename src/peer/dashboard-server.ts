import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import express, { type Request } from "express";
import { dashboardHtml } from "../dashboard.js";
import { faviconIco, faviconSvg } from "../favicon.js";
import { isLoopback } from "../config.js";
import { snapshot } from "./dashboard-snapshot.js";
import { fetchPeerDashboards } from "./dashboard-peers.js";
import { loadIdentity } from "./identity.js";
import { PeerStore, type Contact } from "./store.js";

export interface PeerDashboardOptions {
  home: string;
  port: number;
  bind: string;
}

export interface RunningPeerDashboard {
  url: string;
  port: number;
  close: () => Promise<void>;
}


export interface BuildInfo {
  /** package.json version of the running build. */
  app: string;
  /** Short commit of the checkout when the server started, or null outside a git checkout. */
  commit: string | null;
  /** When this module was compiled (mtime of the running file), or null if unknown. */
  built: string | null;
}

function readCommit(root: string): string | null {
  try {
    let gitDir = join(root, ".git");
    if (statSync(gitDir).isFile()) {
      const pointer = readFileSync(gitDir, "utf8").trim().replace(/^gitdir:\s*/, "");
      gitDir = pointer.match(/^([A-Za-z]:|\/)/) ? pointer : join(root, pointer);
    }
    const head = readFileSync(join(gitDir, "HEAD"), "utf8").trim();
    let sha = head;
    if (head.startsWith("ref: ")) {
      const ref = head.slice(5);
      const common = existsSync(join(gitDir, "commondir"))
        ? join(gitDir, readFileSync(join(gitDir, "commondir"), "utf8").trim()) : gitDir;
      const loose = [join(gitDir, ref), join(common, ref)].find((file) => existsSync(file));
      if (loose) sha = readFileSync(loose, "utf8").trim();
      else {
        const packed = existsSync(join(common, "packed-refs")) ? readFileSync(join(common, "packed-refs"), "utf8") : "";
        sha = packed.split(/\r?\n/).find((line) => line.endsWith(` ${ref}`))?.split(" ")[0] ?? "";
      }
    }
    return /^[0-9a-f]{40}$/.test(sha) ? sha.slice(0, 7) : null;
  } catch {
    return null;
  }
}

/** Version of the running dashboard build, read once at startup without spawning git. */
export function buildInfo(): BuildInfo {
  const self = fileURLToPath(import.meta.url);
  const root = join(dirname(self), "..", "..");
  let app = "unknown";
  try { app = String((JSON.parse(readFileSync(join(root, "package.json"), "utf8")) as { version?: string }).version ?? "unknown"); }
  catch { /* keep "unknown" */ }
  let built: string | null = null;
  try { built = statSync(self).mtime.toISOString(); } catch { /* unknown */ }
  return { app, commit: readCommit(root), built };
}

export async function startPeerDashboard(options: PeerDashboardOptions): Promise<RunningPeerDashboard> {
  if (!isLoopback(options.bind)) {
    throw new Error("The dashboard listens on loopback only.");
  }
  const version = buildInfo();
  const app = express();
  app.disable("x-powered-by");
  app.get("/", (_req, res) => res.redirect("/dashboard"));
  app.get("/favicon.svg", (_req, res) => {
    res.set("Cache-Control", "public, max-age=86400").type("image/svg+xml").send(faviconSvg);
  });
  app.get("/favicon.ico", (_req, res) => {
    res.set("Cache-Control", "public, max-age=86400").type("image/x-icon").send(faviconIco());
  });

  const server = createServer(app);
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port, options.bind, () => { server.off("error", reject); resolve(); });
  });
  const actualPort = (server.address() as { port: number }).port;
  function loopbackHost(req: Request): boolean {
    const host = (req.header("host") ?? "").toLowerCase();
    return host === `127.0.0.1:${actualPort}` || host === `localhost:${actualPort}` || host === `[::1]:${actualPort}`;
  }
  app.get("/dashboard", (req, res) => {
    if (!loopbackHost(req)) {
      res.status(403).type("text/plain").send("Forbidden host");
      return;
    }
    res
      .set("Cache-Control", "no-store")
      .set("Content-Security-Policy", "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'")
      .set("X-Content-Type-Options", "nosniff")
      .type("html")
      .send(dashboardHtml);
  });
  app.get("/api/dashboard", (req, res) => {
    if (!loopbackHost(req)) {
      res.status(403).json({ error: "Forbidden host" });
      return;
    }
    try {
      res.set("Cache-Control", "no-store").json({ ...snapshot(options.home), version });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      res.set("Cache-Control", "no-store").json({
        mode: "peer",
        version,
        generated_at: new Date().toISOString(),
        agent: { id: "peer" },
        mailbox: { url: "peer", candidates: ["peer"], connected: false, fatal: message },
        wake: { mode: "off", client: "peer" },
        agents: { agents: [] },
        agents_error: message,
        remote: null,
        remote_error: message,
        local: null,
        recent: [],
        outbox: [],
        log: message,
      });
    }
  });
  // Peers on each dashboard: this server asks every pinned peer for its metadata over the peer port.
  // The browser never talks to another PC, and the answers carry no message text.
  app.get("/api/peers", async (req, res) => {
    if (!loopbackHost(req)) {
      res.status(403).json({ error: "Forbidden host" });
      return;
    }
    try {
      const contacts = new PeerStore(options.home).list<Contact>("contacts");
      res.set("Cache-Control", "no-store").json({ peers: await fetchPeerDashboards(loadIdentity(options.home), contacts) });
    } catch (error) {
      res.set("Cache-Control", "no-store").json({ peers: [], error: error instanceof Error ? error.message : String(error) });
    }
  });
  app.get("/health", (_req, res) => {
    res.json({ ok: true, service: "peer-dashboard" });
  });

  return {
    url: `http://127.0.0.1:${actualPort}/dashboard`,
    port: actualPort,
    close: () => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}
