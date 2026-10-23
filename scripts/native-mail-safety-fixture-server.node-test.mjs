import assert from "node:assert/strict";
import { createServer } from "node:net";
import test from "node:test";
import tls from "node:tls";

import {
  parseArgs,
  startNativeMailSafetyFixture,
} from "./native-mail-safety-fixture-server.mjs";

async function makeRejectedTlsContact(port) {
  await new Promise((resolve, reject) => {
    const socket = tls.connect({
      host: "127.0.0.1",
      port,
      servername: "localhost",
      rejectUnauthorized: false,
    });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error("TLS contact test timed out"));
    }, 2_000);
    socket.once("secureConnect", () => {
      clearTimeout(timer);
      socket.destroy();
      reject(new Error("fixture unexpectedly completed TLS"));
    });
    socket.once("error", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

async function listen(server, port = 0) {
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => {
      server.off("error", reject);
      resolve();
    });
  });
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  return address.port;
}

function close(server) {
  return new Promise((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
}

test("parses a loopback-only native fixture configuration", () => {
  assert.deepEqual(parseArgs([]), {
    host: "127.0.0.1",
    port: 48_731,
    tlsProbePort: 48_732,
    evidence: undefined,
  });
  assert.deepEqual(
    parseArgs([
      "--port",
      "49000",
      "--tls-probe-port",
      "49001",
      "--evidence",
      "/tmp/native.json",
    ]),
    {
      host: "127.0.0.1",
      port: 49_000,
      tlsProbePort: 49_001,
      evidence: "/tmp/native.json",
    },
  );
  assert.throws(() => parseArgs(["--host", "0.0.0.0"]), /must be 127\.0\.0\.1/);
  assert.throws(() => parseArgs(["--port", "80"]), /1024 to 65535/);
  assert.throws(() => parseArgs(["--tls-probe-port", "80"]), /1024 to 65535/);
  assert.throws(
    () => parseArgs(["--port", "49000", "--tls-probe-port", "49000"]),
    /must be different/,
  );
  assert.throws(() => parseArgs(["--unknown"]), /unknown argument/);
});

test("records only fixture-resource requests and supports reset", async () => {
  const fixture = await startNativeMailSafetyFixture();
  try {
    const health = await fetch(`${fixture.origin}/health`);
    assert.equal(health.status, 200);
    assert.equal(fixture.snapshot().totalRequests, 0);
    assert.equal(fixture.snapshot().tlsContactAttempts, 0);
    assert.equal(fixture.snapshot().unexpectedHttpTlsContactAttempts, 0);
    assert.equal(fixture.snapshot().totalContacts, 0);
    assert.notEqual(fixture.port, fixture.tlsProbePort);

    const image = await fetch(`${fixture.origin}/remote-image.svg`);
    assert.equal(image.status, 200);
    assert.match(image.headers.get("content-type") ?? "", /image\/svg\+xml/);
    await image.arrayBuffer();

    const style = await fetch(`${fixture.origin}/remote-style.css`);
    assert.equal(style.status, 200);
    assert.match(await style.text(), /remote-font\.woff2/);

    const script = await fetch(`${fixture.origin}/hostile-script.js`);
    assert.equal(script.status, 200);
    assert.match(await script.text(), /script-executed/);

    const report = await fetch(`${fixture.origin}/requests`).then((response) =>
      response.json(),
    );
    assert.equal(report.totalRequests, 3);
    assert.equal(report.tlsContactAttempts, 0);
    assert.equal(report.unexpectedHttpTlsContactAttempts, 0);
    assert.equal(report.totalContacts, 3);
    assert.deepEqual(report.counts, {
      "/remote-image.svg": 1,
      "/remote-style.css": 1,
      "/hostile-script.js": 1,
    });
    assert.equal(report.responseObservations.length, 3);
    assert.ok(
      report.responseObservations.every(
        (observation) =>
          observation.listener === "http" &&
          observation.finished &&
          !observation.closedBeforeFinish &&
          observation.status === 200,
      ),
    );

    const reset = await fetch(`${fixture.origin}/reset`, { method: "POST" });
    assert.equal(reset.status, 204);
    assert.equal(fixture.snapshot().totalRequests, 0);
    assert.equal(fixture.snapshot().tlsContactAttempts, 0);
    assert.equal(fixture.snapshot().unexpectedHttpTlsContactAttempts, 0);
    assert.equal(fixture.snapshot().totalContacts, 0);
    assert.deepEqual(fixture.snapshot().responseObservations, []);
    assert.deepEqual(fixture.snapshot().transportEvents, []);
  } finally {
    await fixture.close();
  }
});

test("isolates expected TLS contacts from the HTTP fixture", async () => {
  const fixture = await startNativeMailSafetyFixture();
  try {
    await makeRejectedTlsContact(fixture.tlsProbePort);

    const tlsOnly = fixture.snapshot();
    assert.equal(tlsOnly.totalRequests, 0);
    assert.deepEqual(tlsOnly.counts, {});
    assert.deepEqual(tlsOnly.requests, []);
    assert.equal(tlsOnly.tlsContactAttempts, 1);
    assert.equal(tlsOnly.unexpectedHttpTlsContactAttempts, 0);
    assert.equal(tlsOnly.totalContacts, 1);
    assert.deepEqual(tlsOnly.transportEvents, [
      {
        sequence: 1,
        at: tlsOnly.transportEvents[0].at,
        kind: "tls-client-hello",
        listener: "tls-probe",
      },
    ]);

    const image = await fetch(`${fixture.origin}/remote-image.svg`);
    assert.equal(image.status, 200);
    const withHttp = fixture.snapshot();
    assert.equal(withHttp.totalRequests, 1);
    assert.equal(withHttp.counts["/remote-image.svg"], 1);
    assert.equal(withHttp.tlsContactAttempts, 1);
    assert.equal(withHttp.unexpectedHttpTlsContactAttempts, 0);
    assert.equal(withHttp.totalContacts, 2);

    await makeRejectedTlsContact(fixture.port);
    const wrongListener = fixture.snapshot();
    assert.equal(wrongListener.tlsContactAttempts, 1);
    assert.equal(wrongListener.unexpectedHttpTlsContactAttempts, 1);
    assert.equal(wrongListener.totalContacts, 3);
    assert.equal(wrongListener.transportEvents.at(-1)?.listener, "http");

    fixture.reset();
    const reset = fixture.snapshot();
    assert.equal(reset.totalRequests, 0);
    assert.equal(reset.tlsContactAttempts, 0);
    assert.equal(reset.unexpectedHttpTlsContactAttempts, 0);
    assert.equal(reset.totalContacts, 0);
  } finally {
    await fixture.close();
  }
});

test("fails closed when the isolated TLS port is not owned and releases both listeners", async () => {
  const httpReservation = createServer();
  const tlsReservation = createServer();
  const httpPort = await listen(httpReservation);
  const tlsProbePort = await listen(tlsReservation);
  await close(httpReservation);
  try {
    await assert.rejects(
      startNativeMailSafetyFixture({ port: httpPort, tlsProbePort }),
      (error) => error?.code === "EADDRINUSE",
    );
    const httpProof = createServer();
    await listen(httpProof, httpPort);
    await close(httpProof);
  } finally {
    await close(tlsReservation);
  }

  const fixture = await startNativeMailSafetyFixture();
  const ownedHttpPort = fixture.port;
  const ownedTlsProbePort = fixture.tlsProbePort;
  await fixture.close();
  const httpProof = createServer();
  const tlsProof = createServer();
  try {
    await listen(httpProof, ownedHttpPort);
    await listen(tlsProof, ownedTlsProbePort);
  } finally {
    await close(httpProof);
    await close(tlsProof);
  }
});

test("records redirect and final resource as separate privacy contacts", async () => {
  const fixture = await startNativeMailSafetyFixture();
  try {
    const redirect = await fetch(`${fixture.origin}/redirect-image`, {
      redirect: "manual",
    });
    assert.equal(redirect.status, 302);
    assert.equal(
      redirect.headers.get("location"),
      "/remote-image.svg?via=redirect",
    );
    assert.equal(redirect.headers.get("connection"), "close");
    assert.equal(redirect.headers.get("content-length"), "12");
    assert.equal(redirect.headers.get("transfer-encoding"), null);
    assert.equal(await redirect.text(), "redirecting\n");

    const target = new URL(redirect.headers.get("location"), fixture.origin);
    const response = await fetch(target);
    assert.equal(response.status, 200);
    await response.arrayBuffer();
    await new Promise((resolve) => setImmediate(resolve));

    const snapshot = fixture.snapshot();
    assert.equal(snapshot.totalRequests, 2);
    assert.deepEqual(
      snapshot.requests.map(({ path, query, referer }) => ({
        path,
        query,
        referer,
      })),
      [
        { path: "/redirect-image", query: "", referer: null },
        {
          path: "/remote-image.svg",
          query: "?via=redirect",
          referer: null,
        },
      ],
    );
    assert.deepEqual(
      snapshot.responseObservations.map(
        ({ path, query, status, finished, closedBeforeFinish }) => ({
          path,
          query,
          status,
          finished,
          closedBeforeFinish,
        }),
      ),
      [
        {
          path: "/redirect-image",
          query: "",
          status: 302,
          finished: true,
          closedBeforeFinish: false,
        },
        {
          path: "/remote-image.svg",
          query: "?via=redirect",
          status: 200,
          finished: true,
          closedBeforeFinish: false,
        },
      ],
    );
  } finally {
    await fixture.close();
  }
});

test("redirect completion remains intact during an isolated TLS failure", async () => {
  const fixture = await startNativeMailSafetyFixture();
  try {
    const [response] = await Promise.all([
      fetch(`${fixture.origin}/redirect-image`),
      makeRejectedTlsContact(fixture.tlsProbePort),
    ]);
    assert.equal(response.status, 200);
    assert.match(await response.text(), /Remote image loaded after consent/);
    await new Promise((resolve) => setImmediate(resolve));

    const snapshot = fixture.snapshot();
    assert.equal(snapshot.totalRequests, 2);
    assert.equal(snapshot.tlsContactAttempts, 1);
    assert.equal(snapshot.unexpectedHttpTlsContactAttempts, 0);
    assert.equal(snapshot.totalContacts, 3);
    assert.deepEqual(
      snapshot.requests.map(({ listener, path, query }) => ({
        listener,
        path,
        query,
      })),
      [
        { listener: "http", path: "/redirect-image", query: "" },
        {
          listener: "http",
          path: "/remote-image.svg",
          query: "?via=redirect",
        },
      ],
    );
    assert.ok(
      snapshot.responseObservations.every(
        (observation) =>
          observation.finished && !observation.closedBeforeFinish,
      ),
    );
  } finally {
    await fixture.close();
  }
});
