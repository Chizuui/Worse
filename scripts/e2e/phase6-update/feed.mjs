// Loopback update feed (latest.json + artifacts) with switchable modes for the Phase 6 updater gate.
import http from "node:http";
import { RELEASE_NOTES, VERSION_DOWN, VERSION_N, VERSION_N1 } from "./constants.mjs";

export function startFeed(files) {
  const feed = { mode: "none", requests: [], bytesServed: {}, port: 0 };
  const server = http.createServer((request, response) => {
    const url = new URL(request.url, "http://127.0.0.1");
    feed.requests.push({ at: Date.now(), path: url.pathname, mode: feed.mode });
    if (url.pathname === "/latest.json") {
      const body = feed.manifest();
      if (body === null) {
        response.writeHead(204).end();
        return;
      }
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify(body));
      return;
    }
    const file = files[url.pathname];
    if (!file) {
      response.writeHead(404).end();
      return;
    }
    feed.bytesServed[url.pathname] = (feed.bytesServed[url.pathname] ?? 0) + file.length;
    response.writeHead(200, { "content-type": "application/octet-stream", "content-length": String(file.length) }).end(file);
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      feed.port = server.address().port;
      feed.server = server;
      resolve(feed);
    });
  });
}

export function manifestFor(feed, mode, signatures) {
  const base = `http://127.0.0.1:${feed.port}`;
  const entry = (signature, file) => ({ signature, url: `${base}${file}` });
  const doc = (version, platforms) => ({ version, notes: RELEASE_NOTES, pub_date: "2026-11-01T00:00:00Z", platforms });
  switch (mode) {
    case "none":
      return null;
    case "downgrade":
      return doc(VERSION_DOWN, { "windows-x86_64": entry(signatures.n1, "/n1-setup.exe") });
    case "same":
      return doc(VERSION_N, { "windows-x86_64": entry(signatures.n1, "/n1-setup.exe") });
    case "wrong-platform":
      return doc(VERSION_N1, { "darwin-aarch64": entry(signatures.n1, "/n1-setup.exe") });
    case "tampered":
      return doc(VERSION_N1, { "windows-x86_64": entry(signatures.n1, "/tampered-setup.exe") });
    case "foreign-key":
      return doc(VERSION_N1, { "windows-x86_64": entry(signatures.attacker, "/n1-setup.exe") });
    case "relabel":
      return doc("0.6.3", { "windows-x86_64": entry(signatures.n1, "/n1-setup.exe") });
    case "good":
      return doc(VERSION_N1, { "windows-x86_64": entry(signatures.n1, "/n1-setup.exe") });
    default:
      throw new Error(`unknown feed mode ${mode}`);
  }
}
