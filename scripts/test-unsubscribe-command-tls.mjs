import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

import { stopFixture } from "./fixture-process-lifecycle.mjs";

const OPENSSL_TIMEOUT_MS = 30_000;
const CARGO_TIMEOUT_MS = 120_000;

export function waitForCommandChild(
  child,
  command,
  args,
  { timeoutMs = CARGO_TIMEOUT_MS } = {},
) {
  return new Promise((resolve, reject) => {
    let settled = false;
    let timedOut = false;
    let timer;
    const settle = (callback, value) => {
      if (settled) return;
      settled = true;
      if (timer) clearTimeout(timer);
      callback(value);
    };
    // `run` calls this immediately after spawn. Keep the production timeout
    // inclusive of child startup rather than waiting for a test-only signal.
    timer = setTimeout(() => {
      timedOut = true;
      void stopFixture(child).then(
        () =>
          settle(
            reject,
            new Error(
              `${command} ${args.join(" ")} timed out after ${timeoutMs}ms`,
            ),
          ),
        (cleanupError) => settle(reject, cleanupError),
      );
    }, timeoutMs);
    child.once("error", (spawnError) => settle(reject, spawnError));
    child.once("exit", (code, signal) => {
      if (timedOut) {
        settle(
          reject,
          new Error(
            `${command} ${args.join(" ")} timed out after ${timeoutMs}ms`,
          ),
        );
      } else if (code === 0 && !signal) {
        settle(resolve);
      } else {
        settle(
          reject,
          new Error(
            `${command} ${args.join(" ")} failed with ${code ?? signal}`,
          ),
        );
      }
    });
  });
}

export function run(
  command,
  args,
  env,
  cwd,
  { timeoutMs = CARGO_TIMEOUT_MS } = {},
) {
  const child = spawn(command, args, { stdio: "inherit", env, cwd });
  return waitForCommandChild(child, command, args, { timeoutMs });
}

async function openssl(args) {
  await run("openssl", args, process.env, process.cwd(), {
    timeoutMs: OPENSSL_TIMEOUT_MS,
  });
}

async function waitForPorts(path, server) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (server.exitCode !== null)
      throw new Error("fictional unsubscribe TLS server exited");
    try {
      const ports = JSON.parse(await readFile(path, "utf8"));
      if (Number.isInteger(ports.good) && Number.isInteger(ports.wrongHost))
        return ports;
    } catch {
      // The owned server has not published both listeners yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("timed out waiting for fictional unsubscribe TLS listeners");
}

async function certificate({ key, csr, cert, caCert, caKey, hostname }) {
  await openssl([
    "req",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-subj",
    `/CN=${hostname}`,
    "-keyout",
    key,
    "-out",
    csr,
  ]);
  const extension = `${cert}.ext`;
  await writeFile(
    extension,
    [
      "[server]",
      "basicConstraints=critical,CA:false",
      "keyUsage=critical,digitalSignature,keyEncipherment",
      "extendedKeyUsage=serverAuth",
      `subjectAltName=DNS:${hostname}`,
      "",
    ].join("\n"),
  );
  await openssl([
    "x509",
    "-req",
    "-in",
    csr,
    "-CA",
    caCert,
    "-CAkey",
    caKey,
    "-CAcreateserial",
    "-days",
    "1",
    "-extfile",
    extension,
    "-extensions",
    "server",
    "-out",
    cert,
  ]);
}

export async function main() {
  const candidate = process.cwd();
  const root = await mkdtemp(join(tmpdir(), "dakia-unsubscribe-command-tls-"));
  const nonce = randomUUID();
  const profile = join(root, "profile");
  const downloads = join(root, "Downloads");
  const tls = join(root, "tls");
  const portFile = join(root, "ports.json");
  const receipt = join(root, "https-receipt.json");
  const artifact = join(root, "tauri-unsubscribe-command.json");
  let server;

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
      writeFile(join(root, ".dakia-unsubscribe-command-tls"), `${nonce}\n`, {
        mode: 0o600,
      }),
    ]);
    const caKey = join(tls, "ca.key");
    const caCert = join(tls, "ca.crt");
    const caDer = join(tls, "ca.der");
    const untrustedKey = join(tls, "untrusted.key");
    const untrustedCert = join(tls, "untrusted.crt");
    const untrustedDer = join(tls, "untrusted.der");
    await openssl([
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-days",
      "1",
      "-subj",
      "/CN=Dakia unsubscribe fixture CA",
      "-addext",
      "basicConstraints=critical,CA:true",
      "-addext",
      "keyUsage=critical,keyCertSign,cRLSign",
      "-keyout",
      caKey,
      "-out",
      caCert,
    ]);
    await openssl(["x509", "-in", caCert, "-outform", "DER", "-out", caDer]);
    await openssl([
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-days",
      "1",
      "-subj",
      "/CN=Untrusted unsubscribe fixture CA",
      "-addext",
      "basicConstraints=critical,CA:true",
      "-addext",
      "keyUsage=critical,keyCertSign,cRLSign",
      "-keyout",
      untrustedKey,
      "-out",
      untrustedCert,
    ]);
    await openssl([
      "x509",
      "-in",
      untrustedCert,
      "-outform",
      "DER",
      "-out",
      untrustedDer,
    ]);
    const goodCert = join(tls, "good.crt");
    const goodKey = join(tls, "good.key");
    const wrongCert = join(tls, "wrong.crt");
    const wrongKey = join(tls, "wrong.key");
    await certificate({
      key: goodKey,
      csr: join(tls, "good.csr"),
      cert: goodCert,
      caCert,
      caKey,
      hostname: "unsubscribe-fixture.invalid",
    });
    await certificate({
      key: wrongKey,
      csr: join(tls, "wrong.csr"),
      cert: wrongCert,
      caCert,
      caKey,
      hostname: "wrong-unsubscribe-fixture.invalid",
    });
    server = spawn(
      "node",
      [
        "scripts/unsubscribe-command-tls-fixture-server.mjs",
        "--good-cert",
        goodCert,
        "--good-key",
        goodKey,
        "--wrong-cert",
        wrongCert,
        "--wrong-key",
        wrongKey,
        "--port-file",
        portFile,
        "--receipt",
        receipt,
      ],
      { stdio: "inherit", cwd: candidate, env: process.env },
    );
    const ports = await waitForPorts(portFile, server);
    const base = {
      ...process.env,
      DAKIA_NATIVE_MAIL_SAFETY_FIXTURE: "1",
      DAKIA_NATIVE_MAIL_SAFETY_ROOT: root,
      DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR: profile,
      DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR: downloads,
      DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST: "127.0.0.1",
      DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT: String(ports.good),
      DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME: "localhost",
      DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST: "127.0.0.1",
      DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT: String(ports.good),
      DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME: "localhost",
      DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH: caDer,
      DAKIA_NATIVE_MAIL_SAFETY_PASSWORD: "synthetic-only-password",
      DAKIA_UNSUBSCRIBE_COMMAND_TLS_ROOT: root,
      DAKIA_UNSUBSCRIBE_COMMAND_TLS_NONCE: nonce,
      DAKIA_UNSUBSCRIBE_COMMAND_TLS_ARTIFACT: artifact,
    };
    const invoke = async (name, port, rootDer) =>
      run(
        "cargo",
        [
          "test",
          "-p",
          "dakia-desktop",
          "--lib",
          "--locked",
          "--features",
          "native-mail-safety-fixture",
          `unsubscribe_command_tls_tests::${name}`,
          "--",
          "--exact",
          "--ignored",
        ],
        {
          ...base,
          DAKIA_UNSUBSCRIBE_COMMAND_TLS_PORT: String(port),
          DAKIA_UNSUBSCRIBE_COMMAND_TLS_CA_DER_PATH: rootDer,
        },
        candidate,
      );
    await invoke(
      "production_tauri_unsubscribe_posts_once_and_stale_or_wrong_account_never_reach_tls",
      ports.good,
      caDer,
    );
    await invoke(
      "production_tauri_unsubscribe_rejects_untrusted_tls_without_http_post",
      ports.good,
      untrustedDer,
    );
    await invoke(
      "production_tauri_unsubscribe_rejects_wrong_tls_hostname_without_http_post",
      ports.wrongHost,
      caDer,
    );
    await stopFixture(server);
    server = undefined;

    const evidence = JSON.parse(await readFile(artifact, "utf8"));
    const wire = JSON.parse(await readFile(receipt, "utf8"));
    if (
      !evidence.completed ||
      !evidence.staleRejected ||
      !evidence.wrongAccountRejected
    ) {
      throw new Error(
        `incomplete unsubscribe command evidence: ${JSON.stringify(evidence)}`,
      );
    }
    if (wire.requests.length !== 1)
      throw new Error(`expected one HTTP POST, got ${wire.requests.length}`);
    const [post] = wire.requests;
    if (
      post.server !== "good" ||
      post.method !== "POST" ||
      post.path !== "/one-click" ||
      post.contentType !== "application/x-www-form-urlencoded" ||
      post.body !== "List-Unsubscribe=One-Click"
    ) {
      throw new Error(`unexpected RFC8058 request: ${JSON.stringify(post)}`);
    }
    for (const header of ["authorization", "cookie", "origin", "referer"]) {
      if (post[header] !== null)
        throw new Error(`unsubscribe POST leaked ${header}`);
    }
  } finally {
    await stopFixture(server);
    if (process.env.DAKIA_KEEP_UNSUBSCRIBE_COMMAND_TLS_ARTIFACTS === "1") {
      process.stderr.write(
        `Retained unsubscribe command TLS artifacts: ${root}\n`,
      );
    } else {
      await rm(root, { recursive: true, force: true });
    }
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  await main();
}
