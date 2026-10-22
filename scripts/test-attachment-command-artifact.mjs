import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { stopFixture } from "./fixture-process-lifecycle.mjs";

function run(command, args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", env });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0 && !signal) resolve();
      else reject(new Error(`${command} ${args.join(" ")} failed`));
    });
  });
}

async function waitForPort(portFile, server) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (server.exitCode !== null)
      throw new Error("fictional IMAP fixture exited");
    try {
      const port = Number((await readFile(portFile, "utf8")).trim());
      if (Number.isInteger(port) && port > 0 && port < 65536) return port;
    } catch {
      // The owned fixture has not written its loopback port yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("timed out waiting for fictional TLS IMAP fixture");
}

async function startNoSmtpProbe() {
  let connectionCount = 0;
  const sockets = new Set();
  const listener = createServer((socket) => {
    connectionCount += 1;
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    socket.destroy();
  });
  listener.listen(0, "127.0.0.1");
  await once(listener, "listening");
  const address = listener.address();
  if (!address || typeof address === "string") {
    throw new Error("fictional no-SMTP probe did not bind a loopback port");
  }
  return {
    port: address.port,
    connectionCount: () => connectionCount,
    async close() {
      for (const socket of sockets) socket.destroy();
      if (!listener.listening) return;
      const closed = once(listener, "close");
      listener.close();
      await closed;
    },
  };
}

const root = await mkdtemp(
  join(tmpdir(), "dakia-attachment-command-artifact-"),
);
const nonce = randomUUID();
const profile = join(root, "profile");
const downloads = join(root, "Downloads");
const tls = join(root, "tls");
const portFile = join(root, "imap-port");
const artifact = join(root, "tauri-attachment-command.json");
const frontendResults = join(root, "reader-results.json");
let server;
let smtpProbe;

try {
  await Promise.all([mkdir(profile), mkdir(downloads), mkdir(tls)]);
  await Promise.all([
    writeFile(
      join(root, ".dakia-native-mail-safety-root"),
      "dakia-native-mail-safety-fixture-v1\n",
      { mode: 0o600 },
    ),
    writeFile(
      join(profile, ".dakia-native-mail-safety-profile"),
      "dakia-native-mail-safety-fixture-v1\n",
      { mode: 0o600 },
    ),
    writeFile(join(root, ".dakia-attachment-command-artifact"), `${nonce}\n`, {
      mode: 0o600,
    }),
  ]);

  await run(
    "openssl",
    [
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-days",
      "1",
      "-subj",
      "/CN=Dakia attachment artifact CA",
      "-addext",
      "basicConstraints=critical,CA:true",
      "-addext",
      "keyUsage=critical,keyCertSign,cRLSign",
      "-keyout",
      join(tls, "ca.key"),
      "-out",
      join(tls, "ca.crt"),
    ],
    process.env,
  );
  await run(
    "openssl",
    [
      "req",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-subj",
      "/CN=localhost",
      "-keyout",
      join(tls, "server.key"),
      "-out",
      join(tls, "server.csr"),
    ],
    process.env,
  );
  await writeFile(
    join(tls, "server.ext"),
    [
      "[server]",
      "basicConstraints=critical,CA:false",
      "keyUsage=critical,digitalSignature,keyEncipherment",
      "extendedKeyUsage=serverAuth",
      "subjectAltName=DNS:localhost,IP:127.0.0.1",
      "",
    ].join("\n"),
  );
  await run(
    "openssl",
    [
      "x509",
      "-req",
      "-in",
      join(tls, "server.csr"),
      "-CA",
      join(tls, "ca.crt"),
      "-CAkey",
      join(tls, "ca.key"),
      "-CAcreateserial",
      "-days",
      "1",
      "-extfile",
      join(tls, "server.ext"),
      "-extensions",
      "server",
      "-out",
      join(tls, "server.crt"),
    ],
    process.env,
  );
  await run(
    "openssl",
    [
      "x509",
      "-in",
      join(tls, "ca.crt"),
      "-outform",
      "DER",
      "-out",
      join(tls, "ca.der"),
    ],
    process.env,
  );

  server = spawn(
    "node",
    [
      "scripts/attachment-command-fixture-server.mjs",
      "--cert",
      join(tls, "server.crt"),
      "--key",
      join(tls, "server.key"),
      "--port-file",
      portFile,
    ],
    { stdio: "inherit", env: process.env },
  );
  const port = await waitForPort(portFile, server);
  smtpProbe = await startNoSmtpProbe();
  const env = {
    ...process.env,
    DAKIA_ATTACHMENT_COMMAND_ARTIFACT_ROOT: root,
    DAKIA_ATTACHMENT_COMMAND_ARTIFACT_NONCE: nonce,
    DAKIA_ATTACHMENT_COMMAND_TAURI_ARTIFACT: artifact,
    DAKIA_NATIVE_MAIL_SAFETY_FIXTURE: "1",
    DAKIA_NATIVE_MAIL_SAFETY_ROOT: root,
    DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR: profile,
    DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR: downloads,
    DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST: "127.0.0.1",
    DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT: String(port),
    DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME: "localhost",
    DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST: "127.0.0.1",
    DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT: String(smtpProbe.port),
    DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME: "localhost",
    DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH: join(tls, "ca.der"),
    DAKIA_NATIVE_MAIL_SAFETY_PASSWORD: "synthetic-only-password",
  };
  delete env.DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT;

  await run(
    "cargo",
    [
      "test",
      "-p",
      "dakia-desktop",
      "--lib",
      "--locked",
      "--features",
      "native-mail-safety-fixture",
      "attachment_command_artifact_tests::provider_mime_to_tauri_attachment_commands_exports_reader_artifact",
      "--",
      "--exact",
    ],
    env,
  );
  if (smtpProbe.connectionCount() !== 0) {
    throw new Error("attachment command artifact unexpectedly contacted SMTP");
  }
  const actual = JSON.parse(await readFile(artifact, "utf8"));
  if (actual.nonce !== nonce || actual.bulk?.error?.savedCount !== 1) {
    throw new Error(
      "Tauri attachment artifact has the wrong nonce or partial outcome",
    );
  }

  await run(
    "npx",
    [
      "vitest",
      "run",
      "apps/desktop/src/components/attachmentCommandArtifact.test.tsx",
      "--maxWorkers=1",
      "--reporter=json",
      "--outputFile",
      frontendResults,
    ],
    {
      ...env,
      VITE_DAKIA_ATTACHMENT_COMMAND_ARTIFACT_NONCE: nonce,
      VITE_DAKIA_ATTACHMENT_COMMAND_TAURI_ARTIFACT: Buffer.from(
        JSON.stringify(actual),
      ).toString("base64"),
    },
  );
  const frontend = JSON.parse(await readFile(frontendResults, "utf8"));
  if (
    frontend.numTotalTests !== 1 ||
    frontend.numPassedTests !== 1 ||
    frontend.numPendingTests !== 0
  ) {
    throw new Error(
      "Reader attachment artifact test did not run exactly once and pass",
    );
  }
  if (smtpProbe.connectionCount() !== 0) {
    throw new Error("attachment command artifact unexpectedly contacted SMTP");
  }
} finally {
  await stopFixture(server);
  await smtpProbe?.close();
  if (process.env.DAKIA_KEEP_ATTACHMENT_COMMAND_ARTIFACTS === "1") {
    process.stderr.write(`Retained attachment-command artifacts: ${root}\n`);
  } else {
    await rm(root, { recursive: true, force: true });
  }
}
