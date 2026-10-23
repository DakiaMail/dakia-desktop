#!/usr/bin/env node

import { createServer } from "node:http";
import { writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const defaultHost = "127.0.0.1";
const defaultPort = 48_731;
const defaultTlsProbePort = 48_732;

function validPort(port) {
  return Number.isInteger(port) && port >= 1_024 && port <= 65_535;
}

export function parseArgs(argv) {
  const options = {
    host: defaultHost,
    port: defaultPort,
    tlsProbePort: defaultTlsProbePort,
    evidence: undefined,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--host") options.host = argv[++index];
    else if (value === "--port") options.port = Number(argv[++index]);
    else if (value === "--tls-probe-port")
      options.tlsProbePort = Number(argv[++index]);
    else if (value === "--evidence") options.evidence = argv[++index];
    else throw new Error(`unknown argument: ${value}`);
  }
  if (options.host !== defaultHost)
    throw new Error(`--host must be ${defaultHost}`);
  if (!validPort(options.port))
    throw new Error("--port must be an integer from 1024 to 65535");
  if (!validPort(options.tlsProbePort))
    throw new Error("--tls-probe-port must be an integer from 1024 to 65535");
  if (options.port === options.tlsProbePort)
    throw new Error("--port and --tls-probe-port must be different");
  if (argv.includes("--evidence") && !options.evidence)
    throw new Error("--evidence requires a file path");
  return options;
}

function responseHeaders(contentType) {
  return {
    "cache-control": "no-store, max-age=0",
    "content-type": contentType,
    "cross-origin-resource-policy": "cross-origin",
    "access-control-allow-origin": "*",
    "x-content-type-options": "nosniff",
  };
}

function json(response, status, value) {
  response.writeHead(
    status,
    responseHeaders("application/json; charset=utf-8"),
  );
  response.end(`${JSON.stringify(value, null, 2)}\n`);
}

function snapshot(state) {
  const counts = {};
  for (const event of state.requests)
    counts[event.path] = (counts[event.path] ?? 0) + 1;
  return {
    schemaVersion: 3,
    startedAt: state.startedAt,
    capturedAt: new Date().toISOString(),
    listeners: structuredClone(state.listeners),
    totalRequests: state.requests.length,
    tlsContactAttempts: state.tlsContactAttempts,
    unexpectedHttpTlsContactAttempts: state.unexpectedHttpTlsContactAttempts,
    totalContacts:
      state.requests.length +
      state.tlsContactAttempts +
      state.unexpectedHttpTlsContactAttempts,
    counts,
    requests: structuredClone(state.requests),
    responseObservations: structuredClone(state.responseObservations),
    transportEvents: structuredClone(state.transportEvents),
  };
}

function resetState(state) {
  state.requests.length = 0;
  state.responseObservations.length = 0;
  state.transportEvents.length = 0;
  state.tlsContactAttempts = 0;
  state.unexpectedHttpTlsContactAttempts = 0;
  state.startedAt = new Date().toISOString();
}

function recordTlsClientError(state, error, socket, listener) {
  // A TLS ClientHello uses handshake content type 0x16. The HTTP fixture
  // cannot complete TLS, but the connection itself is still a privacy
  // contact. Count it once per socket without retaining or logging any raw
  // handshake bytes.
  if (
    Buffer.isBuffer(error.rawPacket) &&
    error.rawPacket[0] === 0x16 &&
    !state.tlsContactSockets.has(socket)
  ) {
    state.tlsContactSockets.add(socket);
    if (listener === "tls-probe") state.tlsContactAttempts += 1;
    else state.unexpectedHttpTlsContactAttempts += 1;
    state.transportEvents.push({
      sequence: state.transportEvents.length + 1,
      at: new Date().toISOString(),
      kind: "tls-client-hello",
      listener,
    });
  }
  socket.destroy();
}

function record(state, request, response, url, listener) {
  const recorded = {
    sequence: state.requests.length + 1,
    at: new Date().toISOString(),
    listener,
    method: request.method ?? "GET",
    path: url.pathname,
    query: url.search,
    destination: request.headers["sec-fetch-dest"] ?? null,
    referer: request.headers.referer ?? null,
    userAgent: request.headers["user-agent"] ?? null,
  };
  state.requests.push(recorded);
  const observation = {
    requestSequence: recorded.sequence,
    listener,
    path: url.pathname,
    query: url.search,
    startedAt: recorded.at,
    status: null,
    finishedAt: null,
    closedAt: null,
    finished: false,
    closedBeforeFinish: false,
  };
  state.responseObservations.push(observation);
  response.once("finish", () => {
    observation.status = response.statusCode;
    observation.finished = true;
    observation.finishedAt = new Date().toISOString();
  });
  response.once("close", () => {
    observation.closedAt = new Date().toISOString();
    observation.closedBeforeFinish = !observation.finished;
  });
}

function routeFixture(request, response, url, state) {
  if (url.pathname === "/health") {
    json(response, 200, { ok: true });
    return;
  }
  if (url.pathname === "/requests") {
    json(response, 200, snapshot(state));
    return;
  }
  if (url.pathname === "/reset" && request.method === "POST") {
    resetState(state);
    response.writeHead(204, responseHeaders("text/plain; charset=utf-8"));
    response.end();
    return;
  }

  record(state, request, response, url, "http");
  if (url.pathname === "/remote-image.svg") {
    response.writeHead(200, responseHeaders("image/svg+xml; charset=utf-8"));
    response.end(
      "<svg xmlns='http://www.w3.org/2000/svg' width='520' height='74' viewBox='0 0 520 74'>" +
        "<rect width='520' height='74' rx='12' fill='#d7f5e8'/>" +
        "<text x='20' y='44' font-family='sans-serif' font-size='20' fill='#164a37'>Remote image loaded after consent</text>" +
        "</svg>",
    );
    return;
  }
  if (url.pathname === "/srcset-image.svg") {
    response.writeHead(200, responseHeaders("image/svg+xml; charset=utf-8"));
    response.end(
      "<svg xmlns='http://www.w3.org/2000/svg' width='40' height='40'><circle cx='20' cy='20' r='18' fill='#367b64'/></svg>",
    );
    return;
  }
  if (url.pathname === "/svg-external.svg") {
    response.writeHead(200, responseHeaders("image/svg+xml; charset=utf-8"));
    response.end(
      "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'><rect width='1' height='1' fill='#367b64'/></svg>",
    );
    return;
  }
  if (url.pathname === "/background.svg") {
    response.writeHead(200, responseHeaders("image/svg+xml; charset=utf-8"));
    response.end(
      "<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8' fill='#eef8f4'/></svg>",
    );
    return;
  }
  if (url.pathname === "/remote-style.css") {
    response.writeHead(200, responseHeaders("text/css; charset=utf-8"));
    response.end(
      "@font-face{font-family:DakiaMailSafetyRemote;src:url('/remote-font.woff2') format('woff2')}" +
        ".remote-style-marker{font-family:DakiaMailSafetyRemote,sans-serif;background-image:url('/background.svg');color:#164a37}",
    );
    return;
  }
  if (url.pathname === "/remote-font.woff2") {
    // The byte sequence is intentionally tiny. A request is the relevant
    // privacy signal; font decoding success is outside this acceptance gate.
    response.writeHead(200, responseHeaders("font/woff2"));
    response.end(Buffer.from("wOF2dakia-mail-safety-fixture", "utf8"));
    return;
  }
  if (url.pathname === "/redirect-image") {
    const body = Buffer.from("redirecting\n", "utf8");
    response.writeHead(302, {
      ...responseHeaders("text/plain; charset=utf-8"),
      // Make redirect framing explicit and start the final image on a fresh
      // HTTP socket. The deliberately invalid TLS probe has its own listener.
      connection: "close",
      "content-length": String(body.byteLength),
      location: "/remote-image.svg?via=redirect",
    });
    response.end(body);
    return;
  }
  if (url.pathname === "/hostile-script.js") {
    response.writeHead(200, responseHeaders("text/javascript; charset=utf-8"));
    response.end(
      "document.documentElement.dataset.dakiaMailSafetyScriptExecuted='true';fetch('/script-executed')",
    );
    return;
  }
  if (url.pathname === "/hostile-frame.html") {
    response.writeHead(200, responseHeaders("text/html; charset=utf-8"));
    response.end(
      "<!doctype html><script>fetch('/frame-executed')</script>frame",
    );
    return;
  }
  if (url.pathname === "/form-submit") {
    response.writeHead(204, responseHeaders("text/plain; charset=utf-8"));
    response.end();
    return;
  }
  response.writeHead(404, responseHeaders("text/plain; charset=utf-8"));
  response.end("not found\n");
}

export async function startNativeMailSafetyFixture(options = {}) {
  const host = options.host ?? defaultHost;
  const port = options.port ?? 0;
  const tlsProbePort = options.tlsProbePort ?? 0;
  if (host !== defaultHost)
    throw new Error(`fixture host must be ${defaultHost}`);
  if (!Number.isInteger(port) || port < 0 || port > 65_535)
    throw new Error("fixture port must be an integer from 0 to 65535");
  if (
    !Number.isInteger(tlsProbePort) ||
    tlsProbePort < 0 ||
    tlsProbePort > 65_535
  )
    throw new Error(
      "fixture TLS probe port must be an integer from 0 to 65535",
    );
  if (port !== 0 && port === tlsProbePort)
    throw new Error("fixture ports must be different");
  const state = {
    startedAt: new Date().toISOString(),
    listeners: null,
    requests: [],
    responseObservations: [],
    transportEvents: [],
    tlsContactAttempts: 0,
    unexpectedHttpTlsContactAttempts: 0,
    tlsContactSockets: new WeakSet(),
  };
  const server = createServer((request, response) => {
    const url = new URL(request.url ?? "/", `http://${host}`);
    routeFixture(request, response, url, state);
  });
  server.on("clientError", (error, socket) => {
    recordTlsClientError(state, error, socket, "http");
  });
  const tlsProbeServer = createServer((request, response) => {
    const url = new URL(request.url ?? "/", `http://${host}`);
    record(state, request, response, url, "tls-probe");
    response.writeHead(421, responseHeaders("text/plain; charset=utf-8"));
    response.end("TLS probe listener does not serve HTTP.\n");
  });
  tlsProbeServer.on("clientError", (error, socket) => {
    recordTlsClientError(state, error, socket, "tls-probe");
  });
  const listen = (ownedServer, ownedPort) =>
    new Promise((resolve, reject) => {
      ownedServer.once("error", reject);
      ownedServer.listen(ownedPort, host, () => {
        ownedServer.off("error", reject);
        resolve();
      });
    });
  const closeServer = (ownedServer) =>
    new Promise((resolve, reject) =>
      ownedServer.close((error) => (error ? reject(error) : resolve())),
    );
  await listen(server, port);
  try {
    await listen(tlsProbeServer, tlsProbePort);
  } catch (error) {
    await closeServer(server);
    throw error;
  }
  const address = server.address();
  const tlsProbeAddress = tlsProbeServer.address();
  if (!address || typeof address === "string")
    throw new Error("fixture server did not expose a TCP address");
  if (!tlsProbeAddress || typeof tlsProbeAddress === "string") {
    await Promise.all([closeServer(server), closeServer(tlsProbeServer)]);
    throw new Error("fixture TLS probe did not expose a TCP address");
  }
  state.listeners = {
    http: { host, port: address.port },
    tlsProbe: { host, port: tlsProbeAddress.port },
  };
  return {
    origin: `http://${host}:${address.port}`,
    port: address.port,
    tlsProbePort: tlsProbeAddress.port,
    snapshot: () => snapshot(state),
    reset: () => resetState(state),
    close: async () => {
      const results = await Promise.allSettled([
        closeServer(server),
        closeServer(tlsProbeServer),
      ]);
      const failure = results.find((result) => result.status === "rejected");
      if (failure) throw failure.reason;
    },
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const fixture = await startNativeMailSafetyFixture(options);
  process.stdout.write(
    `${JSON.stringify({ event: "ready", origin: fixture.origin, tlsProbePort: fixture.tlsProbePort, pid: process.pid })}\n`,
  );
  let closing = false;
  const close = async (signal) => {
    if (closing) return;
    closing = true;
    const evidence = fixture.snapshot();
    if (options.evidence)
      await writeFile(
        options.evidence,
        `${JSON.stringify(evidence, null, 2)}\n`,
      );
    await fixture.close();
    process.stdout.write(
      `${JSON.stringify({ event: "closed", signal, evidence })}\n`,
    );
  };
  for (const signal of ["SIGINT", "SIGTERM"])
    process.once(signal, () => {
      void close(signal).catch((error) => {
        process.stderr.write(`${error.stack ?? error}\n`);
        process.exitCode = 1;
      });
    });
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1])
  main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`);
    process.exitCode = 1;
  });
