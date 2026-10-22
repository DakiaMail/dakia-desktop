#!/usr/bin/env node
// Executes the native fixture's exact SMTP heredoc against a loopback TLS peer.
// This is intentionally separate from native UI acceptance: it validates the
// fixture's executable setup path without launching Dakia or Docker.
import assert from "node:assert/strict";
import { execFile as execFileCallback, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { readFileSync } from "node:fs";
import {
  chmod,
  readFile,
  realpath,
  mkdtemp,
  mkdir,
  rename,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:tls";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { setImmediate as nextTurn } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execFile = promisify(execFileCallback);
const timeoutMs = 5_000;
const childExitTimeoutMs = 2_500;
const scriptRoot = dirname(fileURLToPath(import.meta.url));
const fixtureScript = join(scriptRoot, "native-provider-reader-acceptance.sh");
const workingDirectory = await realpath(
  await mkdtemp(join(tmpdir(), "dakia-native-fixture-node-test.")),
);
let activeServer;
const activeSockets = new Set();

function exactSmtpHeredoc(script) {
  const startMarker =
    'node --input-type=module - "$smtp_port" "$certificate_root/ca.crt" "$fixture_http_port" "$fixture_tls_probe_port" "$mailto_regressions" <<\'NODE\'\n';
  const start = script.indexOf(startMarker);
  assert.notEqual(
    start,
    -1,
    "fixture SMTP heredoc start marker must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf("\nNODE\n\npython3 - ", bodyStart);
  assert.notEqual(end, -1, "fixture SMTP heredoc end marker must remain exact");
  return script.slice(bodyStart, end);
}

function exactAcceptancePlanHeredoc(script) {
  const startMarker =
    'node --input-type=module - "$acceptance_plan" "$fixture_http_port" "$fixture_tls_probe_port" "$mailto_regressions" <<\'NODE\'\n';
  const start = script.indexOf(startMarker);
  assert.notEqual(
    start,
    -1,
    "native acceptance-plan heredoc start marker must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf("\nNODE\nlaunch_metadata=", bodyStart);
  assert.notEqual(
    end,
    -1,
    "native acceptance-plan heredoc end marker must remain exact",
  );
  return script.slice(bodyStart, end);
}

function exactTauriConfigBlock(script) {
  const startMarker = 'tauri_config="$run_root/tauri.json"\n';
  const start = script.indexOf(startMarker);
  assert.notEqual(
    start,
    -1,
    "fixture Tauri config block start marker must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf(
    '\ncargo_wrapper="$run_root/cargo-build-only"',
    bodyStart,
  );
  assert.notEqual(
    end,
    -1,
    "fixture Tauri config block end marker must remain exact",
  );
  return script.slice(start, end);
}

function exactBuildModeEvidenceBlock(script) {
  const startMarker =
    'build_mode_evidence="$run_root/native-build-mode.json"\n';
  const start = script.indexOf(startMarker);
  assert.notEqual(
    start,
    -1,
    "fixture build-mode block start marker must remain exact",
  );
  const end = script.indexOf(
    '\nif [[ "$packaged" == true ]]; then\n  (cd "$repo_root"',
    start + startMarker.length,
  );
  assert.notEqual(
    end,
    -1,
    "fixture build-mode block end marker must remain exact",
  );
  return script.slice(start, end);
}

function exactPostExitReleaseHeredoc(script) {
  const functionMarker = "post_exit_release_is_valid() {\n";
  const functionStart = script.indexOf(functionMarker);
  assert.notEqual(
    functionStart,
    -1,
    "post-exit release validator must remain present",
  );
  const startMarker =
    '  node --input-type=module - "$release_path" "$run_root" <<\'NODE\'\n';
  const start = script.indexOf(startMarker, functionStart);
  assert.notEqual(
    start,
    -1,
    "post-exit release validator start must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf("\nNODE\n}", bodyStart);
  assert.notEqual(end, -1, "post-exit release validator end must remain exact");
  return script.slice(bodyStart, end);
}

function exactPostExitHandoffHeredoc(script) {
  const startMarker =
    '  node --input-type=module - "$post_exit_handoff" "$run_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" "$app_sha256" "$cli_sha256" "$post_exit_release" <<\'NODE\'\n';
  const start = script.indexOf(startMarker);
  assert.notEqual(
    start,
    -1,
    "post-exit handoff writer start marker must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf(
    "\nNODE\n  printf 'Primary packaged acceptance exited cleanly.",
    bodyStart,
  );
  assert.notEqual(end, -1, "post-exit handoff writer end must remain exact");
  return script.slice(bodyStart, end);
}

function exactApprovedProcessStateHeredoc(script) {
  const functionMarker = "approved_identity_process_state() {\n";
  const functionStart = script.indexOf(functionMarker);
  assert.notEqual(
    functionStart,
    -1,
    "approved process-state probe must remain present",
  );
  const startMarker =
    "  node --input-type=module - \"$approved_identity_app/Contents/MacOS/dakia-desktop\" <<'NODE'\n";
  const start = script.indexOf(startMarker, functionStart);
  assert.notEqual(
    start,
    -1,
    "approved process-state probe start must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf("\nNODE\n}", bodyStart);
  assert.notEqual(
    end,
    -1,
    "approved process-state probe end must remain exact",
  );
  return script.slice(bodyStart, end);
}

function exactLaunchServicesRegistrationHeredoc(script) {
  const functionMarker =
    "register_approved_identity_with_launch_services() {\n";
  const functionStart = script.indexOf(functionMarker);
  assert.notEqual(
    functionStart,
    -1,
    "LaunchServices registration function must remain present",
  );
  const startMarker =
    '  node --input-type=module - "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" "$approved_identity_identifier" "$app_sha256" "$cli_sha256" "$run_root" <<\'NODE\'\n';
  const start = script.indexOf(startMarker, functionStart);
  assert.notEqual(
    start,
    -1,
    "LaunchServices registration heredoc start must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf("\nNODE\n}", bodyStart);
  assert.notEqual(
    end,
    -1,
    "LaunchServices registration heredoc end must remain exact",
  );
  return script.slice(bodyStart, end);
}

function exactNotificationPreflightWarningPredicate(script) {
  const startMarker = "notification_preflight_warning_is_exact() {\n";
  const start = script.indexOf(startMarker);
  assert.notEqual(
    start,
    -1,
    "notification-preflight warning predicate must remain present",
  );
  const end = script.indexOf(
    "\n}\n\nregister_approved_identity_with_launch_services()",
    start,
  );
  assert.notEqual(
    end,
    -1,
    "notification-preflight warning predicate end must remain exact",
  );
  return script.slice(start, end + 2);
}

async function notificationPreflightWarningIsAccepted(
  predicate,
  appLog,
  identifier,
) {
  try {
    await execFile(
      "bash",
      [
        "-c",
        `${predicate}\nnotification_preflight_warning_is_exact "$1" "$2"`,
        "notification-preflight-warning-test",
        appLog,
        identifier,
      ],
      { timeout: timeoutMs },
    );
    return true;
  } catch {
    return false;
  }
}

function exactApprovedIdentityHeredoc(script) {
  const startMarker =
    "  node --input-type=module - \"$@\" <<'APPROVED_IDENTITY_NODE'\n";
  const start = script.indexOf(startMarker);
  assert.notEqual(
    start,
    -1,
    "approved identity engine start marker must remain exact",
  );
  const bodyStart = start + startMarker.length;
  const end = script.indexOf("\nAPPROVED_IDENTITY_NODE\n}", bodyStart);
  assert.notEqual(
    end,
    -1,
    "approved identity engine end marker must remain exact",
  );
  return script.slice(bodyStart, end);
}

function assertExactFixtureScope(config, profileRoot) {
  const expectedScope = `${profileRoot}/translation-models/**`;
  const assetProtocol = config?.app?.security?.assetProtocol;
  assert.equal(assetProtocol?.enable, true);
  assert.deepEqual(assetProtocol?.scope, [expectedScope]);
  assert.equal(
    assetProtocol.scope.some((scope) => scope.includes("$APPLOCALDATA")),
    false,
  );
  assert.equal(
    assetProtocol.scope.some(
      (scope) => scope === `${profileRoot.replace(/\/profile$/, "")}/**`,
    ),
    false,
  );
}

async function generateCertificateAuthority(root) {
  const caKey = join(root, "ca.key");
  const caCertificate = join(root, "ca.crt");
  const serverKey = join(root, "server.key");
  const serverRequest = join(root, "server.csr");
  const serverCertificate = join(root, "server.crt");
  const extensions = join(root, "server.ext");
  await execFile("openssl", [
    "req",
    "-x509",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-days",
    "1",
    "-subj",
    "/CN=Dakia native fixture test CA",
    "-keyout",
    caKey,
    "-out",
    caCertificate,
  ]);
  await execFile("openssl", [
    "req",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-subj",
    "/CN=localhost",
    "-keyout",
    serverKey,
    "-out",
    serverRequest,
  ]);
  await writeFile(extensions, "subjectAltName=DNS:localhost,IP:127.0.0.1\n");
  await execFile("openssl", [
    "x509",
    "-req",
    "-in",
    serverRequest,
    "-CA",
    caCertificate,
    "-CAkey",
    caKey,
    "-CAcreateserial",
    "-days",
    "1",
    "-extfile",
    extensions,
    "-out",
    serverCertificate,
  ]);
  return { caCertificate, serverKey, serverCertificate };
}

function startSmtpServer(credentials) {
  const connections = [];
  const server = createServer(
    {
      key: readFileSync(credentials.serverKey),
      cert: readFileSync(credentials.serverCertificate),
    },
    (socket) => {
      activeSockets.add(socket);
      socket.once("close", () => activeSockets.delete(socket));
      const connection = { commands: [], messages: [], quitAt: undefined };
      connections.push(connection);
      let buffered = "";
      let dataMode = false;
      socket.setEncoding("utf8");
      socket.write("220 fictional SMTP ready\r\n");
      const handleLine = (line) => {
        connection.commands.push(line);
        if (/^EHLO /i.test(line)) socket.write("250 localhost\r\n");
        else if (/^AUTH PLAIN /i.test(line))
          socket.write("235 authenticated\r\n");
        else if (/^MAIL FROM:/i.test(line))
          socket.write("250 sender accepted\r\n");
        else if (/^RCPT TO:/i.test(line))
          socket.write("250 recipient accepted\r\n");
        else if (line === "DATA") {
          dataMode = true;
          socket.write("354 continue\r\n");
        } else if (line === "QUIT") {
          // Deliberately remain half-open. The fixture must release its side
          // of the completed SMTP connection without a server-initiated close.
          connection.quitAt = Date.now();
          socket.write("221 closing\r\n");
        } else {
          socket.write("500 unexpected command\r\n");
        }
      };
      socket.on("data", (chunk) => {
        buffered += chunk;
        for (;;) {
          if (dataMode) {
            const terminator = buffered.indexOf("\r\n.\r\n");
            if (terminator < 0) return;
            connection.messages.push(buffered.slice(0, terminator));
            buffered = buffered.slice(terminator + "\r\n.\r\n".length);
            dataMode = false;
            socket.write("250 queued\r\n");
            continue;
          }
          const ending = buffered.indexOf("\r\n");
          if (ending < 0) return;
          const line = buffered.slice(0, ending);
          buffered = buffered.slice(ending + 2);
          handleLine(line);
        }
      });
    },
  );
  return { server, connections };
}

async function withTimeout(promise, description, limitMs = timeoutMs) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`${description} timed out`)),
          limitMs,
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function waitForChildClose(child, description, limitMs = timeoutMs) {
  const [exitCode, signal] = await withTimeout(
    once(child, "close"),
    description,
    limitMs,
  );
  return { exitCode, signal };
}

async function assertChildCloseBarrier() {
  const grandchildPath = join(workingDirectory, "child-close-grandchild.mjs");
  const readyPath = join(workingDirectory, "child-close.ready");
  const releasePath = join(workingDirectory, "child-close.release");
  const markerPath = join(workingDirectory, "child-close.marker");
  await writeFile(
    grandchildPath,
    [
      'import { existsSync, writeFileSync } from "node:fs";',
      'writeFileSync(process.argv[2], "ready\\n");',
      "const releaseDeadline = Date.now() + 2_000;",
      "while (!existsSync(process.argv[3])) {",
      "  if (Date.now() >= releaseDeadline) {",
      '    process.stderr.write("child close-barrier release timed out\\n");',
      "    process.exit(71);",
      "  }",
      "  await new Promise((resolve) => setTimeout(resolve, 10));",
      "}",
      'writeFileSync(process.argv[4], "closed\\n");',
      'process.stdout.write("grandchild-closed\\n");',
      "",
    ].join("\n"),
  );

  let directExit;
  let runnerSettled = false;
  const runner = runLaunchServicesRegistration(
    [
      'import { spawn } from "node:child_process";',
      'import { existsSync } from "node:fs";',
      "const grandchild = spawn(process.execPath, process.argv.slice(2), {",
      '  stdio: ["ignore", "inherit", "inherit"],',
      "});",
      "grandchild.unref();",
      "const readyDeadline = Date.now() + 2_000;",
      "while (!existsSync(process.argv[3])) {",
      "  if (Date.now() >= readyDeadline) {",
      '    process.stderr.write("child close-barrier readiness timed out\\n");',
      "    process.exit(70);",
      "  }",
      "  await new Promise((resolve) => setTimeout(resolve, 10));",
      "}",
      "",
    ].join("\n"),
    [grandchildPath, readyPath, releasePath, markerPath],
    {},
    (child) => {
      directExit = once(child, "exit");
    },
  );
  void runner.then(
    () => {
      runnerSettled = true;
    },
    () => {
      runnerSettled = true;
    },
  );
  assert.ok(directExit, "the close-barrier test must observe the direct child");
  let result;
  try {
    const [directExitCode, directSignal] = await withTimeout(
      directExit,
      "direct child exit before inherited streams close",
    );
    assert.equal(directExitCode, 0);
    assert.equal(directSignal, null);
    assert.equal(await readFile(readyPath, "utf8"), "ready\n");
    await assert.rejects(() => readFile(markerPath, "utf8"), /ENOENT/);
    await nextTurn();
    assert.equal(
      runnerSettled,
      false,
      "the LaunchServices runner must not return at child exit while inherited streams remain open",
    );
  } finally {
    await writeFile(releasePath, "release\n");
    result = await runner;
  }
  assert.equal(result.exitCode, 0, result.stderr);
  assert.equal(result.signal, null, result.stderr);
  assert.equal(await readFile(markerPath, "utf8"), "closed\n");
  assert.equal(result.stdout, "grandchild-closed\n");
}

async function runExactHeredoc({ heredoc, port, caCertificate, manifestPath }) {
  const { spawn } = await import("node:child_process");
  const child = spawn(
    globalThis.process.execPath,
    [
      "--input-type=module",
      "-",
      String(port),
      caCertificate,
      "48731",
      "48732",
      manifestPath,
    ],
    {
      env: {
        PATH: globalThis.process.env.PATH ?? "",
        DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT: "",
      },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  child.stdin.end(heredoc);
  try {
    const [exitCode] = await withTimeout(
      once(child, "exit"),
      "fixture SMTP heredoc",
      childExitTimeoutMs,
    );
    return { exitCode, stdout, stderr, exitedAt: Date.now() };
  } catch (error) {
    child.kill();
    throw new Error(
      `${error.message}; stdout=${JSON.stringify(stdout)} stderr=${JSON.stringify(stderr)}`,
    );
  }
}

async function runExactPlanHeredoc({ heredoc, output, manifestPath }) {
  const { spawn } = await import("node:child_process");
  const child = spawn(
    globalThis.process.execPath,
    ["--input-type=module", "-", output, "48731", "48732", manifestPath],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  child.stdin.end(heredoc);
  const [exitCode] = await withTimeout(
    once(child, "exit"),
    "native acceptance-plan heredoc",
  );
  assert.equal(exitCode, 0, `native acceptance-plan heredoc failed: ${stderr}`);
}

async function runApprovedIdentityEngine(heredoc, args) {
  const { spawn } = await import("node:child_process");
  const child = spawn(
    globalThis.process.execPath,
    ["--input-type=module", "-", ...args],
    {
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  child.stdin.end(heredoc);
  const [exitCode] = await withTimeout(
    once(child, "exit"),
    "approved identity engine",
  );
  return { exitCode, stdout, stderr };
}

async function runLaunchServicesRegistration(
  heredoc,
  args,
  env = {},
  observeChild = () => {},
) {
  const child = spawn(
    globalThis.process.execPath,
    ["--input-type=module", "-", ...args],
    {
      env: { ...globalThis.process.env, ...env },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  observeChild(child);
  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const closed = waitForChildClose(
    child,
    "LaunchServices registration heredoc",
  );
  child.stdin.end(heredoc);
  const { exitCode, signal } = await closed;
  return { exitCode, signal, stdout, stderr };
}

async function createFakeBundle(path, identifier, desktopBytes, cliBytes) {
  await mkdir(join(path, "Contents", "MacOS"), { recursive: true });
  await mkdir(join(path, "Contents", "Resources"), { recursive: true });
  await writeFile(
    join(path, "Contents", "Info.plist"),
    [
      "<plist><dict>",
      `<key>CFBundleIdentifier</key><string>${identifier}</string>`,
      "<key>CFBundleName</key><string>Dakia Fictional Provider Reader</string>",
      "</dict></plist>",
    ].join(""),
  );
  await writeFile(
    join(path, "Contents", "MacOS", "dakia-desktop"),
    desktopBytes,
    { mode: 0o755 },
  );
  await writeFile(join(path, "Contents", "MacOS", "dakia"), cliBytes, {
    mode: 0o755,
  });
  await writeFile(
    join(path, "Contents", "Resources", "fixture.txt"),
    `resource:${desktopBytes}`,
  );
}

const digest = (value) => createHash("sha256").update(value).digest("hex");

async function createIdentityScenario(root, identifier, name) {
  const approvedRoot = join(root, `${name}-approved`);
  const runRoot = join(root, `${name}-run`);
  const approvedApp = join(approvedRoot, "Dakia Fictional Provider Reader.app");
  const candidateApp = join(runRoot, "Dakia Fictional Provider Reader.app");
  await mkdir(approvedRoot, { recursive: true, mode: 0o700 });
  await chmod(approvedRoot, 0o700);
  await mkdir(runRoot, { recursive: true, mode: 0o700 });
  await chmod(runRoot, 0o700);
  await writeFile(
    join(approvedRoot, ".dakia-native-mail-safety-root"),
    "dakia-native-mail-safety-fixture-v1\n",
  );
  await writeFile(
    join(runRoot, ".dakia-native-mail-safety-root"),
    "dakia-native-mail-safety-fixture-v1\n",
  );
  await createFakeBundle(
    approvedApp,
    identifier,
    "sealed-desktop",
    "sealed-cli",
  );
  await createFakeBundle(
    candidateApp,
    identifier,
    "candidate-desktop",
    "candidate-cli",
  );
  return {
    approvedRoot,
    approvedApp,
    recordPath: join(approvedRoot, "approved-native-identity.json"),
    journalPath: join(approvedRoot, ".approved-native-identity-swap.json"),
    runRoot,
    candidateApp,
    identifier,
    desktopSha256: digest("sealed-desktop"),
    cliSha256: digest("sealed-cli"),
  };
}

const bundlePlistPath = (scenario) =>
  join(scenario.approvedApp, "Contents", "Info.plist");

async function writeBinaryBundlePlist(
  scenario,
  identifier = scenario.identifier,
) {
  const program = String.raw`
import plistlib
import sys
with open(sys.argv[1], "wb") as output:
    plistlib.dump({
        "CFBundleIdentifier": sys.argv[2],
        "CFBundleName": "Dakia Fictional Provider Reader",
    }, output, fmt=plistlib.FMT_BINARY, sort_keys=False)
`;
  await execFile("/usr/bin/python3", [
    "-c",
    program,
    bundlePlistPath(scenario),
    identifier,
  ]);
}

function identityArgs(action, scenario) {
  return [
    action,
    scenario.approvedRoot,
    scenario.approvedApp,
    scenario.recordPath,
    scenario.journalPath,
    scenario.runRoot,
    scenario.candidateApp,
    scenario.identifier,
    scenario.desktopSha256,
    scenario.cliSha256,
  ];
}

try {
  await assertChildCloseBarrier();
  const [script, credentials, rustLibrary, nativeAcl] = await Promise.all([
    readFile(fixtureScript, "utf8"),
    generateCertificateAuthority(workingDirectory),
    readFile(
      join(scriptRoot, "..", "apps/desktop/src-tauri/src/lib.rs"),
      "utf8",
    ),
    readFile(
      join(
        scriptRoot,
        "..",
        "apps/desktop/src-tauri/src/native_acl_acceptance.rs",
      ),
      "utf8",
    ),
  ]);
  const heredoc = exactSmtpHeredoc(script);
  const launchServicesRegistrationHeredoc =
    exactLaunchServicesRegistrationHeredoc(script);
  const notificationPreflightWarningPredicate =
    exactNotificationPreflightWarningPredicate(script);
  const acceptancePlanHeredoc = exactAcceptancePlanHeredoc(script);
  const tauriConfigBlock = exactTauriConfigBlock(script);
  const buildModeEvidenceBlock = exactBuildModeEvidenceBlock(script);
  const postExitReleaseHeredoc = exactPostExitReleaseHeredoc(script);
  const postExitHandoffHeredoc = exactPostExitHandoffHeredoc(script);
  const approvedProcessStateHeredoc = exactApprovedProcessStateHeredoc(script);
  const approvedIdentityHeredoc = exactApprovedIdentityHeredoc(script);
  assert.doesNotMatch(
    script,
    /OTAMH2|2cc898a0691f|6b72b2de4f89|\/private\/var\/folders\//,
    "governed harness must not embed one Mac's temporary identity",
  );
  assert.match(
    script,
    /--bootstrap-approved-identity-record --from-run-root ROOT/,
  );
  assert.match(script, /--reuse-approved-identity RECORD/);
  assert.match(
    script,
    /--packaged --reuse-approved-identity RECORD/,
    "packaged mode must require an explicitly sealed reused identity",
  );
  assert.match(
    script,
    /node node_modules\/vite\/bin\/vite\.js build[\s\S]*--outDir "\$web_dist" --emptyOutDir/,
    "packaged mode must build its private static frontend",
  );
  assert.match(
    script,
    /npx tauri build --debug --target "\$cli_target" --bundles app --no-sign --ci[\s\S]*--features native-mail-safety-fixture/,
    "packaged mode must use the debug custom-protocol Tauri build",
  );
  assert.match(
    script,
    /codesign --verify --deep --strict --verbose=2 "\$app_root"[\s\S]*"\$app_root\/Contents\/MacOS\/dakia" --version/,
    "the exact signed bundled CLI must execute after final app signing",
  );
  const missingSignedCliExecution = script.replace(
    '[[ "$("$app_root/Contents/MacOS/dakia" --version)" == "dakia $expected_version" ]]',
    '[[ "$expected_version" == "$expected_version" ]]',
  );
  assert.notEqual(missingSignedCliExecution, script);
  assert.doesNotMatch(
    missingSignedCliExecution,
    /codesign --verify --deep --strict --verbose=2 "\$app_root"[\s\S]*"\$app_root\/Contents\/MacOS\/dakia" --version/,
    "the post-sign CLI oracle must fail against a version check that never executes the bundle",
  );
  assert.match(
    script,
    /packaged-classifier-inventory\.json[\s\S]*packaged classifier resources differ from the exact source inventory/,
    "packaged classifier resources must match the exact recursive source inventory",
  );
  assert.match(
    script,
    /if \[\[ "\$packaged" != true \]\]; then[\s\S]*npx tauri dev/,
    "the existing development-server build must remain behind the default mode",
  );
  for (const expected of [
    "export DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST=127.0.0.1",
    'export DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT="$smtp_port"',
    "export DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME=localhost",
  ]) {
    assert.match(
      script,
      new RegExp(expected.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")),
      `native runner must export ${expected}`,
    );
  }
  for (const variable of [
    "DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST",
    "DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT",
    "DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME",
  ]) {
    assert.match(
      rustLibrary,
      new RegExp(`required\\(\"${variable}\"\\)`),
      `desktop fixture must require ${variable}`,
    );
  }
  await assert.rejects(
    () => execFile(fixtureScript, ["--packaged"]),
    (error) =>
      error.code === 64 &&
      error.stderr.includes(
        "--packaged requires an approved identity record and cannot be combined with --prepare-only",
      ),
    "packaged mode without a sealed identity must fail before creating a fixture",
  );
  await assert.rejects(
    () => execFile(fixtureScript, ["--hold-approved-candidate-after-exit"]),
    (error) =>
      error.code === 64 &&
      error.stderr.includes(
        "--hold-approved-candidate-after-exit is valid only with --packaged and an approved identity record",
      ),
    "post-exit handoff must not run outside the exact packaged identity flow",
  );
  assert.match(
    script,
    /native-fictional-imap-proxy\.mjs --control-root %s --action hold --duration-ms 35000[\s\S]*held=true, holdDurationMs=35000, the exact lastCommandId, and a parseable future holdDeadlineAt with enough time remaining; use --action resume for early release/,
    "native guidance must use the nonce-owned bounded hold and require its exact acknowledgement before Sync",
  );
  const absentProbe = await execFile("node", [
    "--input-type=module",
    "-e",
    approvedProcessStateHeredoc,
    "fixture-process-probe",
    "/private/tmp/definitely-absent-dakia-fixture-executable",
  ]);
  assert.equal(absentProbe.stdout, "absent");
  const failingProcessProbe = approvedProcessStateHeredoc.replace(
    "'/bin/ps'",
    "'/private/tmp/definitely-missing-ps-command'",
  );
  assert.notEqual(failingProcessProbe, approvedProcessStateHeredoc);
  const failedProbe = await execFile("node", [
    "--input-type=module",
    "-e",
    failingProcessProbe,
    "fixture-process-probe",
    "/private/tmp/definitely-absent-dakia-fixture-executable",
  ]);
  assert.equal(failedProbe.stdout, "probe-error");
  assert.match(
    failedProbe.stderr,
    /could not enumerate approved-identity processes/,
  );
  assert.match(
    script,
    /elif \[\[ "\$approved_identity_process_probe" != 'absent' \]\]; then[\s\S]*Retaining the active transaction for explicit recovery[\s\S]*elif \{ \[\[ ! -e "\$approved_identity_journal" \]\]/,
    "cleanup must reach restoration only after a successful absent process probe",
  );
  const cleanupStart = script.indexOf("cleanup() {\n");
  const cleanupEnd = script.indexOf("\n}\ntrap cleanup EXIT", cleanupStart);
  assert(cleanupStart >= 0 && cleanupEnd > cleanupStart);
  const cleanupBody = script.slice(cleanupStart, cleanupEnd);
  const assertHoldBlocksRestore = (body) => {
    const holdGuard = body.indexOf(
      'if [[ "$approved_identity_post_exit_hold_active" == true ]]; then',
    );
    const restore = body.indexOf("approved_identity_state restore");
    assert(
      holdGuard >= 0 && restore > holdGuard,
      "active post-exit handoff must be checked before any restoration",
    );
  };
  assertHoldBlocksRestore(cleanupBody);
  const cleanupWithoutHoldGuard = cleanupBody.replaceAll(
    'if [[ "$approved_identity_post_exit_hold_active" == true ]]; then',
    "if false; then",
  );
  assert.notEqual(cleanupWithoutHoldGuard, cleanupBody);
  assert.throws(
    () => assertHoldBlocksRestore(cleanupWithoutHoldGuard),
    /active post-exit handoff must be checked before any restoration/,
  );

  const postExitRoot = join(workingDirectory, "post-exit-release");
  const postExitRelease = join(
    postExitRoot,
    ".native-provider-reader-post-exit-complete",
  );
  await mkdir(postExitRoot, { mode: 0o700 });
  const postExitHandoff = join(
    postExitRoot,
    "post-exit-acceptance-handoff.json",
  );
  await execFile("node", [
    "--input-type=module",
    "-e",
    postExitHandoffHeredoc,
    "fixture-handoff-writer",
    postExitHandoff,
    postExitRoot,
    join(postExitRoot, "Dakia Fictional Provider Reader.app"),
    join(postExitRoot, "approved-native-identity.json"),
    join(postExitRoot, "approved-native-identity-transaction.json"),
    "desktop-sha256",
    "cli-sha256",
    postExitRelease,
  ]);
  const handoff = JSON.parse(await readFile(postExitHandoff, "utf8"));
  assert.equal(
    handoff.expectedReleaseContent,
    "dakia-native-provider-reader-post-exit-complete-v1\n",
    "the published handoff must contain the validator's exact newline byte",
  );
  await writeFile(postExitRelease, handoff.expectedReleaseContent, {
    mode: 0o600,
  });
  await execFile("node", [
    "--input-type=module",
    "-e",
    postExitReleaseHeredoc,
    "fixture-validator",
    postExitRelease,
    postExitRoot,
  ]);
  await writeFile(
    postExitRelease,
    "dakia-native-provider-reader-post-exit-complete-v1\\n",
    { mode: 0o600 },
  );
  await assert.rejects(
    () =>
      execFile("node", [
        "--input-type=module",
        "-e",
        postExitReleaseHeredoc,
        "fixture-validator",
        postExitRelease,
        postExitRoot,
      ]),
    /post-exit release sentinel content is invalid/,
  );
  await writeFile(postExitRelease, "wrong\n", { mode: 0o600 });
  await assert.rejects(
    () =>
      execFile("node", [
        "--input-type=module",
        "-e",
        postExitReleaseHeredoc,
        "fixture-validator",
        postExitRelease,
        postExitRoot,
      ]),
    /post-exit release sentinel content is invalid/,
  );
  await rm(postExitRelease);
  const postExitTarget = join(postExitRoot, "outside-name");
  await writeFile(
    postExitTarget,
    "dakia-native-provider-reader-post-exit-complete-v1\n",
    { mode: 0o600 },
  );
  await symlink(postExitTarget, postExitRelease);
  await assert.rejects(
    () =>
      execFile("node", [
        "--input-type=module",
        "-e",
        postExitReleaseHeredoc,
        "fixture-validator",
        postExitRelease,
        postExitRoot,
      ]),
    /post-exit release sentinel is not a private owned regular file/,
  );
  assert.match(
    rustLibrary,
    /app\s*\.config\(\)\s*\.app\s*\.windows[\s\S]*configured\s*=\s*main_windows[\s\S]*\.data_store_identifier/,
    "fixture datastore helper must reuse the configured main-window identifier",
  );
  assert.match(
    rustLibrary,
    /apply_native_mail_safety_data_store_identifier\(&mut context\)/,
    "the fixture must inject the data-store identifier before the main window is built",
  );
  assert.match(
    rustLibrary,
    /context\.config_mut\(\)\.app\.windows/,
    "the fixture must mutate the typed Tauri context instead of serializing the identifier through JSON",
  );
  assert.match(
    rustLibrary,
    /DAKIA_NATIVE_MAIL_SAFETY_DATA_STORE_IDENTIFIER/,
    "the runtime context injection must read the guarded fixture identifier",
  );
  assert.doesNotMatch(
    tauriConfigBlock,
    /dataStoreIdentifier/,
    "generated Tauri JSON must omit the fixed-size identifier that Tauri codegen serializes as a Vec",
  );
  assert.equal(
    rustLibrary.match(/builder\.data_store_identifier\(identifier\)/g)?.length,
    1,
    "detached mail-window builder must receive the configured identifier once",
  );
  assert.equal(
    nativeAcl.match(/\.data_store_identifier\(identifier\)/g)?.length,
    2,
    "both ACL probe builders must receive the configured identifier",
  );
  assert.match(
    heredoc,
    /await command\('QUIT', '221'\);\nsocket\.destroy\(\);/,
  );
  const { server, connections } = startSmtpServer(credentials);
  activeServer = server;
  server.listen(0, "127.0.0.1");
  await withTimeout(once(server, "listening"), "loopback SMTP server startup");
  const address = server.address();
  assert(
    address && typeof address !== "string",
    "loopback SMTP server must expose a port",
  );

  const manifestPath = join(workingDirectory, "native-mailto-regressions.json");
  let success;
  try {
    success = await runExactHeredoc({
      heredoc,
      port: address.port,
      caCertificate: credentials.caCertificate,
      manifestPath,
    });
  } catch (error) {
    throw new Error(
      `${error.message}; commands=${JSON.stringify(connections.map(({ commands }) => commands))}`,
    );
  }
  assert.equal(
    success.exitCode,
    0,
    `fixture SMTP heredoc failed: ${success.stderr}`,
  );
  assert.equal(connections.length, 1);
  assert.notEqual(
    connections[0].quitAt,
    undefined,
    "fixture must issue QUIT before exiting",
  );
  assert(
    success.exitedAt - connections[0].quitAt < childExitTimeoutMs,
    "fixture must naturally exit within the post-QUIT bound against a half-open peer",
  );
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  assert.equal(manifest.schemaVersion, 1);
  assert.equal(
    manifest.sourceAccountId,
    "62a9320c-04f5-47c2-b4fa-d4952f0b6121",
  );
  assert.equal(manifest.sourceMessageSubject, "Remote content safety check");
  assert.equal(manifest.recipientInvariant, "fictional@example.test");
  assert.equal(manifest.neverSend, true);
  assert.equal(manifest.cases.length, 11);
  assert.equal(
    manifest.cases.filter(
      (testCase) => testCase.expectedNormalizedSubmission === null,
    ).length,
    6,
  );
  assert.equal(
    manifest.cases.filter(
      (testCase) => testCase.expectedNormalizedSubmission !== null,
    ).length,
    5,
  );

  assert.equal(connections[0].messages.length, 3);
  assert.equal(
    connections[0].commands.filter((command) => /^MAIL FROM:/i.test(command))
      .length,
    3,
  );
  const readerMessage = connections[0].messages.find((message) =>
    message.includes("Subject: Remote content safety check"),
  );
  assert(readerMessage, "fixture must submit the Reader message through SMTP");
  for (const testCase of manifest.cases) {
    assert(readerMessage.includes(`id="${testCase.id}"`));
    assert(
      readerMessage.includes(
        `href="${testCase.href.replaceAll("&", "&amp;")}"`,
      ),
    );
  }
  const exactAllowedRemoteMarkup = [
    "http://127.0.0.1:48731/remote-image.svg?via=direct",
    "http://127.0.0.1:48731/redirect-image",
    "http&#58;//127.0.0.1:48731/remote-image.svg?via=encoded-html",
    "http://127.0.0.1:48731/remote-image.svg?via=event-handler",
    "//127.0.0.1:48732/remote-image.svg?via=protocol-relative",
  ];
  for (const markup of exactAllowedRemoteMarkup)
    assert(readerMessage.includes(markup));

  const brokenHeredoc = heredoc.replace("socket.destroy();", "");
  assert.notEqual(
    brokenHeredoc,
    heredoc,
    "socket cleanup sensitivity mutation must apply",
  );
  await assert.rejects(
    () =>
      runExactHeredoc({
        heredoc: brokenHeredoc,
        port: address.port,
        caCertificate: credentials.caCertificate,
        manifestPath: join(workingDirectory, "missing-socket-cleanup.json"),
      }),
    /fixture SMTP heredoc timed out/,
  );

  const acceptancePlanPath = join(
    workingDirectory,
    "native-acceptance-plan.json",
  );
  await runExactPlanHeredoc({
    heredoc: acceptancePlanHeredoc,
    output: acceptancePlanPath,
    manifestPath,
  });
  const acceptancePlan = JSON.parse(await readFile(acceptancePlanPath, "utf8"));
  assert.equal(acceptancePlan.schemaVersion, 3);
  assert.deepEqual(acceptancePlan.mailtoRegressionChecks, manifest);
  assert.equal(acceptancePlan.afterConsent.exactAllowedRequests.length, 5);

  // Execute the harness's exact config-writing block against a quoted
  // temporary profile. This catches wrong nesting, dropped lines, and scope
  // regressions that a regex or independently-built expected object misses.
  const fixtureRoot = join(workingDirectory, 'fixture root "quoted"');
  const profileRoot = join(fixtureRoot, "profile");
  await mkdir(profileRoot, { recursive: true });
  await execFile("bash", ["-c", tauriConfigBlock], {
    env: {
      ...process.env,
      run_root: fixtureRoot,
      profile_root: profileRoot,
      wrapper_identifier: "dev.dakia.fictional-provider-reader.run-quoted",
    },
  });
  const generatedConfig = JSON.parse(
    await readFile(join(fixtureRoot, "tauri.json"), "utf8"),
  );
  const generatedScope = `${profileRoot}/translation-models/**`;
  const productionConfig = JSON.parse(
    readFileSync(
      join(scriptRoot, "..", "apps/desktop/src-tauri/tauri.conf.json"),
      "utf8",
    ),
  );
  assert.deepEqual(productionConfig.app.security.assetProtocol.scope, [
    "$APPLOCALDATA/translation-models/**",
  ]);
  assert.equal(productionConfig.app.security.assetProtocol.enable, true);
  assertExactFixtureScope(generatedConfig, profileRoot);
  assert.equal(
    JSON.stringify(generatedScope),
    JSON.stringify(`${profileRoot}/translation-models/**`),
  );
  assert.equal(generatedConfig.app.windows[0].dataStoreIdentifier, undefined);
  assert.equal(generatedConfig.build.devUrl, "http://127.0.0.1:1420/");
  assert.equal(generatedConfig.build.frontendDist, undefined);
  assert.equal(generatedConfig.bundle.active, false);
  await execFile("bash", ["-c", buildModeEvidenceBlock], {
    env: {
      ...process.env,
      packaged: "false",
      run_root: fixtureRoot,
      tauri_config: join(fixtureRoot, "tauri.json"),
      web_dist: join(fixtureRoot, "web-dist"),
      private_binaries: join(fixtureRoot, "binaries"),
    },
  });
  const developmentBuildMode = JSON.parse(
    await readFile(join(fixtureRoot, "native-build-mode.json"), "utf8"),
  );
  assert.equal(developmentBuildMode.mode, "development-server");
  assert.deepEqual(developmentBuildMode.frontend, {
    command: "vite dev",
    url: "http://127.0.0.1:1420/",
  });
  assert.equal(developmentBuildMode.desktop.command, "tauri dev");
  assert.equal(developmentBuildMode.desktop.noWatch, true);
  assert.equal(developmentBuildMode.externalBin, null);

  const packagedRoot = join(workingDirectory, "packaged-identity-config");
  const packagedProfile = join(packagedRoot, "profile");
  const packagedWebDist = join(packagedRoot, "web-dist");
  const packagedBinaryBase = join(packagedRoot, "binaries", "dakia");
  await mkdir(packagedProfile, { recursive: true });
  await execFile("bash", ["-c", tauriConfigBlock], {
    env: {
      ...process.env,
      packaged: "true",
      run_root: packagedRoot,
      profile_root: packagedProfile,
      web_dist: packagedWebDist,
      private_binaries: join(packagedRoot, "binaries"),
      wrapper_identifier:
        "dev.dakia.fictional-provider-reader.run-approvedtest",
      reuse_approved_identity: "true",
      native_data_store_identifier: "00112233-4455-4677-8899-aabbccddeeff",
    },
  });
  const packagedConfig = JSON.parse(
    await readFile(join(packagedRoot, "tauri.json"), "utf8"),
  );
  assert.equal(packagedConfig.productName, "Dakia Fictional Provider Reader");
  assert.equal(
    packagedConfig.identifier,
    "dev.dakia.fictional-provider-reader.run-approvedtest",
  );
  assert.equal(packagedConfig.build.devUrl, null);
  assert.equal(packagedConfig.build.frontendDist, packagedWebDist);
  assert.equal(packagedConfig.bundle.active, true);
  assert.deepEqual(packagedConfig.bundle.targets, ["app"]);
  assert.equal(packagedConfig.bundle.createUpdaterArtifacts, false);
  assert.deepEqual(packagedConfig.bundle.externalBin, [packagedBinaryBase]);
  assert.equal(packagedConfig.bundle.macOS.signingIdentity, null);
  assert.equal(packagedConfig.bundle.macOS.hardenedRuntime, false);
  assert.deepEqual(packagedConfig.plugins.updater.endpoints, []);
  assert.equal(packagedConfig.app.windows[0].dataStoreIdentifier, undefined);
  const packagedDataStoreEvidencePath = join(
    packagedRoot,
    "native-data-store-identifier.json",
  );
  const packagedDataStoreEvidence = JSON.parse(
    await readFile(packagedDataStoreEvidencePath, "utf8"),
  );
  assert.deepEqual(packagedDataStoreEvidence, {
    schemaVersion: 1,
    injection: "runtime-context",
    identifier: "00112233-4455-4677-8899-aabbccddeeff",
    bytes: [
      0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x46, 0x77, 0x88, 0x99, 0xaa, 0xbb,
      0xcc, 0xdd, 0xee, 0xff,
    ],
  });
  assert.equal((await stat(packagedDataStoreEvidencePath)).mode & 0o777, 0o600);
  assertExactFixtureScope(packagedConfig, packagedProfile);

  await execFile("bash", ["-c", buildModeEvidenceBlock], {
    env: {
      ...process.env,
      packaged: "true",
      run_root: packagedRoot,
      tauri_config: join(packagedRoot, "tauri.json"),
      web_dist: packagedWebDist,
      private_binaries: join(packagedRoot, "binaries"),
    },
  });
  const packagedBuildMode = JSON.parse(
    await readFile(join(packagedRoot, "native-build-mode.json"), "utf8"),
  );
  assert.equal(packagedBuildMode.mode, "packaged-custom-protocol");
  assert.deepEqual(packagedBuildMode.frontend, {
    command: "vite build",
    output: packagedWebDist,
  });
  assert.equal(packagedBuildMode.desktop.command, "tauri build");
  assert.equal(packagedBuildMode.desktop.debug, true);
  assert.equal(packagedBuildMode.desktop.target, "aarch64-apple-darwin");
  assert.equal(packagedBuildMode.desktop.bundle, "app");
  assert.equal(packagedBuildMode.desktop.noSign, true);
  assert.equal(packagedBuildMode.externalBin, packagedBinaryBase);

  const reusedRoot = join(workingDirectory, "reused-identity-config");
  const reusedProfile = join(reusedRoot, "profile");
  await mkdir(reusedProfile, { recursive: true });
  await execFile("bash", ["-c", tauriConfigBlock], {
    env: {
      ...process.env,
      run_root: reusedRoot,
      profile_root: reusedProfile,
      wrapper_identifier:
        "dev.dakia.fictional-provider-reader.run-approvedtest",
      reuse_approved_identity: "true",
      native_data_store_identifier: "00112233-4455-4677-8899-aabbccddeeff",
    },
  });
  const reusedConfig = JSON.parse(
    await readFile(join(reusedRoot, "tauri.json"), "utf8"),
  );
  assert.equal(
    reusedConfig.identifier,
    "dev.dakia.fictional-provider-reader.run-approvedtest",
  );
  assert.equal(reusedConfig.app.windows[0].dataStoreIdentifier, undefined);
  const reusedDataStoreEvidence = JSON.parse(
    await readFile(
      join(reusedRoot, "native-data-store-identifier.json"),
      "utf8",
    ),
  );
  assert.deepEqual(reusedDataStoreEvidence, packagedDataStoreEvidence);

  const secondReusedRoot = join(
    workingDirectory,
    "second-reused-identity-config",
  );
  await mkdir(join(secondReusedRoot, "profile"), { recursive: true });
  await execFile("bash", ["-c", tauriConfigBlock], {
    env: {
      ...process.env,
      run_root: secondReusedRoot,
      profile_root: join(secondReusedRoot, "profile"),
      wrapper_identifier:
        "dev.dakia.fictional-provider-reader.run-approvedtest",
      reuse_approved_identity: "true",
      native_data_store_identifier: "10213243-5465-4767-98a9-bacbdcedfe0f",
    },
  });
  const secondReusedConfig = JSON.parse(
    await readFile(join(secondReusedRoot, "tauri.json"), "utf8"),
  );
  assert.equal(
    secondReusedConfig.app.windows[0].dataStoreIdentifier,
    undefined,
  );
  const secondReusedDataStoreEvidence = JSON.parse(
    await readFile(
      join(secondReusedRoot, "native-data-store-identifier.json"),
      "utf8",
    ),
  );
  assert.notDeepEqual(
    reusedDataStoreEvidence.bytes,
    secondReusedDataStoreEvidence.bytes,
    "each reused identity run must get a distinct WKWebView data store",
  );
  assert.equal(
    secondReusedDataStoreEvidence.identifier,
    "10213243-5465-4767-98a9-bacbdcedfe0f",
  );

  const invalidDataStoreRoot = join(
    workingDirectory,
    "invalid-data-store-config",
  );
  await mkdir(join(invalidDataStoreRoot, "profile"), { recursive: true });
  await assert.rejects(
    () =>
      execFile("bash", ["-e", "-c", tauriConfigBlock], {
        env: {
          ...process.env,
          run_root: invalidDataStoreRoot,
          profile_root: join(invalidDataStoreRoot, "profile"),
          wrapper_identifier:
            "dev.dakia.fictional-provider-reader.run-approvedtest",
          reuse_approved_identity: "true",
          native_data_store_identifier: "00112233-4455-4677-8899-aabbccddeefg",
        },
      }),
    /native fixture data-store UUID is invalid/,
  );
  await assert.rejects(
    () => readFile(join(invalidDataStoreRoot, "tauri.json"), "utf8"),
    /ENOENT/,
  );

  const controlRoot = join(workingDirectory, "control-config");
  const controlProfile = join(controlRoot, "profile");
  await mkdir(controlProfile, { recursive: true });
  await execFile("bash", ["-c", tauriConfigBlock], {
    env: {
      ...process.env,
      run_root: controlRoot,
      profile_root: controlProfile,
      wrapper_identifier: "dev.dakia.fictional-provider-reader.run-control",
    },
  });
  assertExactFixtureScope(
    JSON.parse(await readFile(join(controlRoot, "tauri.json"), "utf8")),
    controlProfile,
  );

  const brokenRoot = join(workingDirectory, "broken-config");
  const brokenProfile = join(brokenRoot, "profile");
  await mkdir(brokenRoot, { recursive: true });
  const brokenBlock = tauriConfigBlock.replace(
    '\\"scope\\": [$asset_scope_json]',
    '\\"scope\\": []',
  );
  assert.notEqual(brokenBlock, tauriConfigBlock);
  await execFile("bash", ["-c", brokenBlock], {
    env: {
      ...process.env,
      run_root: brokenRoot,
      profile_root: brokenProfile,
      wrapper_identifier: "dev.dakia.fictional-provider-reader.run-broken",
    },
  });
  const brokenConfig = JSON.parse(
    await readFile(join(brokenRoot, "tauri.json"), "utf8"),
  );
  assert.throws(() => assertExactFixtureScope(brokenConfig, brokenProfile));

  const rootScopedRoot = join(workingDirectory, "root-scoped-config");
  const rootScopedProfile = join(rootScopedRoot, "profile");
  await mkdir(rootScopedProfile, { recursive: true });
  const rootScopedBlock = tauriConfigBlock.replace(
    '"$profile_root")',
    '"$run_root")',
  );
  assert.notEqual(rootScopedBlock, tauriConfigBlock);
  await execFile("bash", ["-c", rootScopedBlock], {
    env: {
      ...process.env,
      run_root: rootScopedRoot,
      profile_root: rootScopedProfile,
      wrapper_identifier: "dev.dakia.fictional-provider-reader.run-root-scoped",
    },
  });
  const rootScopedConfig = JSON.parse(
    await readFile(join(rootScopedRoot, "tauri.json"), "utf8"),
  );
  assert.throws(() =>
    assertExactFixtureScope(rootScopedConfig, rootScopedProfile),
  );

  assert.match(approvedIdentityHeredoc, /plistlib\.loads/);
  assert.match(approvedIdentityHeredoc, /class RejectDuplicateDict/);
  assert.doesNotMatch(
    approvedIdentityHeredoc,
    /plist\.includes\(<key>CFBundleIdentifier/,
    "approved identity must not use an XML substring as its plist oracle",
  );

  const formattedPlistIdentity = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-REDACTED",
    "identity-formatted-plist",
  );
  const packagedInfoFixturePath = join(
    scriptRoot,
    "..",
    "testdata/native-mail-safety/packaged-info.plist",
  );
  const packagedInfoProvenance = JSON.parse(
    await readFile(`${packagedInfoFixturePath}.provenance.json`, "utf8"),
  );
  const formattedPlist = await readFile(packagedInfoFixturePath, "utf8");
  assert.equal(packagedInfoProvenance.fixtureSha256, digest(formattedPlist));
  assert.deepEqual(packagedInfoProvenance.expected, {
    CFBundleIdentifier: formattedPlistIdentity.identifier,
    CFBundleName: "Dakia Fictional Provider Reader",
  });
  assert.equal(
    formattedPlist.includes(
      `<key>CFBundleIdentifier</key><string>${formattedPlistIdentity.identifier}</string>`,
    ),
    false,
    "the real formatted plist must remain sensitive to the removed substring oracle",
  );
  await writeFile(bundlePlistPath(formattedPlistIdentity), formattedPlist);
  const legacySubstringIdentityHeredoc = approvedIdentityHeredoc.replace(
    "const metadata = await readBundleMetadata(app);",
    "const plist = await readFile(join(app, 'Contents/Info.plist'), 'utf8');\n  const metadata = {\n    CFBundleIdentifier: plist.includes(`<key>CFBundleIdentifier</key><string>${identifier}</string>`) ? identifier : '',\n    CFBundleName: plist.includes('<key>CFBundleName</key><string>Dakia Fictional Provider Reader</string>') ? 'Dakia Fictional Provider Reader' : '',\n  };",
  );
  assert.notEqual(legacySubstringIdentityHeredoc, approvedIdentityHeredoc);
  let plistIdentityResult = await runApprovedIdentityEngine(
    legacySubstringIdentityHeredoc,
    identityArgs("bootstrap", formattedPlistIdentity),
  );
  assert.notEqual(
    plistIdentityResult.exitCode,
    0,
    "the removed substring oracle must fail against a standard formatted plist",
  );
  assert.match(
    plistIdentityResult.stderr,
    /identifier is not the approved exact identity/,
  );
  plistIdentityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("bootstrap", formattedPlistIdentity),
  );
  assert.equal(plistIdentityResult.exitCode, 0, plistIdentityResult.stderr);

  const binaryPlistIdentity = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-binaryplist",
    "identity-binary-plist",
  );
  await writeBinaryBundlePlist(binaryPlistIdentity);
  assert.equal(
    (await readFile(bundlePlistPath(binaryPlistIdentity)))
      .subarray(0, 8)
      .toString(),
    "bplist00",
  );
  plistIdentityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("bootstrap", binaryPlistIdentity),
  );
  assert.equal(plistIdentityResult.exitCode, 0, plistIdentityResult.stderr);

  const invalidPlists = [
    {
      name: "wrong-identifier",
      xml: (scenario) =>
        `<plist><dict><key>CFBundleIdentifier</key><string>dev.dakia.fictional-provider-reader.run-wrong</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string></dict></plist>`,
      error: /identifier is not the approved exact identity/,
    },
    {
      name: "comment-forgery",
      xml: (scenario) =>
        `<plist><dict><!-- <key>CFBundleIdentifier</key><string>${scenario.identifier}</string> --><key>CFBundleIdentifier</key><string>dev.dakia.fictional-provider-reader.run-wrong</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string></dict></plist>`,
      error: /identifier is not the approved exact identity/,
    },
    {
      name: "nested-forgery",
      xml: (scenario) =>
        `<plist><dict><key>CFBundleIdentifier</key><string>dev.dakia.fictional-provider-reader.run-wrong</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string><key>Nested</key><dict><key>CFBundleIdentifier</key><string>${scenario.identifier}</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string></dict></dict></plist>`,
      error: /identifier is not the approved exact identity/,
    },
    {
      name: "duplicate-identifier",
      xml: (scenario) =>
        `<plist><dict><key>CFBundleIdentifier</key><string>dev.dakia.fictional-provider-reader.run-wrong</string><key>CFBundleIdentifier</key><string>${scenario.identifier}</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string></dict></plist>`,
      error: /not a valid unambiguous property list/,
    },
    {
      name: "non-string-name",
      xml: (scenario) =>
        `<plist><dict><key>CFBundleIdentifier</key><string>${scenario.identifier}</string><key>CFBundleName</key><integer>1</integer></dict></plist>`,
      error: /not a valid unambiguous property list/,
    },
    {
      name: "malformed",
      xml: (scenario) =>
        `<plist><dict><key>CFBundleIdentifier</key><string>${scenario.identifier}</string>`,
      error: /not a valid unambiguous property list/,
    },
  ];
  for (const testCase of invalidPlists) {
    const scenario = await createIdentityScenario(
      workingDirectory,
      `dev.dakia.fictional-provider-reader.run-${testCase.name.replaceAll("-", "")}`,
      `identity-${testCase.name}`,
    );
    await writeFile(bundlePlistPath(scenario), testCase.xml(scenario));
    plistIdentityResult = await runApprovedIdentityEngine(
      approvedIdentityHeredoc,
      identityArgs("bootstrap", scenario),
    );
    assert.notEqual(
      plistIdentityResult.exitCode,
      0,
      `${testCase.name} property list must fail closed`,
    );
    assert.match(plistIdentityResult.stderr, testCase.error);
    await assert.rejects(() => readFile(scenario.recordPath), /ENOENT/);
  }

  const symlinkPlistIdentity = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-symlinkplist",
    "identity-symlink-plist",
  );
  await rm(bundlePlistPath(symlinkPlistIdentity));
  await symlink(packagedInfoFixturePath, bundlePlistPath(symlinkPlistIdentity));
  plistIdentityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("bootstrap", symlinkPlistIdentity),
  );
  assert.notEqual(plistIdentityResult.exitCode, 0);
  assert.match(plistIdentityResult.stderr, /metadata is not a regular file/);

  const oversizedPlistIdentity = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-oversizedplist",
    "identity-oversized-plist",
  );
  await writeFile(
    bundlePlistPath(oversizedPlistIdentity),
    Buffer.alloc(1024 * 1024 + 1, 0x20),
  );
  plistIdentityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("bootstrap", oversizedPlistIdentity),
  );
  assert.notEqual(plistIdentityResult.exitCode, 0);
  assert.match(plistIdentityResult.stderr, /metadata size is invalid/);

  const duplicateBlindHeredoc = approvedIdentityHeredoc.replace(
    "if key in self:",
    "if False:",
  );
  assert.notEqual(duplicateBlindHeredoc, approvedIdentityHeredoc);
  const duplicateSensitivity = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-duplicatesensitivity",
    "identity-duplicate-sensitivity",
  );
  await writeFile(
    bundlePlistPath(duplicateSensitivity),
    `<plist><dict><key>CFBundleIdentifier</key><string>dev.dakia.fictional-provider-reader.run-wrong</string><key>CFBundleIdentifier</key><string>${duplicateSensitivity.identifier}</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string></dict></plist>`,
  );
  plistIdentityResult = await runApprovedIdentityEngine(
    duplicateBlindHeredoc,
    identityArgs("bootstrap", duplicateSensitivity),
  );
  assert.equal(
    plistIdentityResult.exitCode,
    0,
    "the duplicate-key regression must fail the production test oracle",
  );

  const identity = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-normal",
  );
  let identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("bootstrap", identity),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  const sealedRecord = JSON.parse(await readFile(identity.recordPath, "utf8"));
  assert.equal(
    sealedRecord.contract,
    "dakia-native-provider-reader-approved-identity-v1",
  );
  assert.equal(
    sealedRecord.inventory.entries.some(
      (entry) =>
        entry.path === "Contents/MacOS/dakia-desktop" && entry.mode === 0o755,
    ),
    true,
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("verify", identity),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);

  const sealedResource = join(
    identity.approvedApp,
    "Contents",
    "Resources",
    "fixture.txt",
  );
  const sealedResourceBytes = await readFile(sealedResource);
  await writeFile(sealedResource, "tampered-resource");
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("verify", identity),
  );
  assert.notEqual(
    identityResult.exitCode,
    0,
    "full sealed inventory must reject a modified resource",
  );
  assert.match(identityResult.stderr, /sealed full inventory/);
  await writeFile(sealedResource, sealedResourceBytes);

  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("begin", identity),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  assert.equal(
    await readFile(
      join(identity.approvedApp, "Contents", "MacOS", "dakia-desktop"),
      "utf8",
    ),
    "candidate-desktop",
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("begin", identity),
  );
  assert.notEqual(
    identityResult.exitCode,
    0,
    "a second transaction must not replace an unresolved journal",
  );
  assert.match(
    identityResult.stderr,
    /unresolved approved identity transaction/,
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("restore", identity),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  assert.equal(
    await readFile(
      join(identity.approvedApp, "Contents", "MacOS", "dakia-desktop"),
      "utf8",
    ),
    "sealed-desktop",
  );
  assert.equal(
    await readFile(
      join(
        identity.runRoot,
        "tested-approved-candidate.app",
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "candidate-desktop",
  );
  await assert.rejects(() => readFile(identity.journalPath), /ENOENT/);

  const identicalCandidate = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-identical-candidate",
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("bootstrap", identicalCandidate),
      )
    ).exitCode,
    0,
  );
  await writeFile(
    join(identicalCandidate.candidateApp, "Contents", "MacOS", "dakia-desktop"),
    "sealed-desktop",
    { mode: 0o755 },
  );
  await writeFile(
    join(identicalCandidate.candidateApp, "Contents", "MacOS", "dakia"),
    "sealed-cli",
    { mode: 0o755 },
  );
  await writeFile(
    join(
      identicalCandidate.candidateApp,
      "Contents",
      "Resources",
      "fixture.txt",
    ),
    "resource:sealed-desktop",
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("begin", identicalCandidate),
  );
  assert.notEqual(
    identityResult.exitCode,
    0,
    "an identical candidate must not create an ambiguous recovery transaction",
  );
  assert.match(identityResult.stderr, /byte-identical to the sealed original/);

  const preparedStage = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-prepared-stage",
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("bootstrap", preparedStage),
      )
    ).exitCode,
    0,
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("begin", preparedStage),
      )
    ).exitCode,
    0,
  );
  await rename(preparedStage.approvedApp, preparedStage.candidateApp);
  await rename(
    join(preparedStage.runRoot, "sealed-approved-original.app"),
    preparedStage.approvedApp,
  );
  const preparedJournal = JSON.parse(
    await readFile(preparedStage.journalPath, "utf8"),
  );
  await writeFile(
    preparedStage.journalPath,
    `${JSON.stringify({ ...preparedJournal, stage: "prepared" }, null, 2)}\n`,
    { mode: 0o600 },
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("recover", preparedStage),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  assert.equal(
    await readFile(
      join(preparedStage.approvedApp, "Contents", "MacOS", "dakia-desktop"),
      "utf8",
    ),
    "sealed-desktop",
  );

  const afterOriginalMove = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-after-original-move",
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("bootstrap", afterOriginalMove),
      )
    ).exitCode,
    0,
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("begin", afterOriginalMove),
      )
    ).exitCode,
    0,
  );
  await rename(afterOriginalMove.approvedApp, afterOriginalMove.candidateApp);
  const originalRetainedJournal = JSON.parse(
    await readFile(afterOriginalMove.journalPath, "utf8"),
  );
  await writeFile(
    afterOriginalMove.journalPath,
    `${JSON.stringify({ ...originalRetainedJournal, stage: "prepared" }, null, 2)}\n`,
    { mode: 0o600 },
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("recover", afterOriginalMove),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  assert.equal(
    await readFile(
      join(afterOriginalMove.approvedApp, "Contents", "MacOS", "dakia-desktop"),
      "utf8",
    ),
    "sealed-desktop",
  );
  assert.equal(
    await readFile(
      join(
        afterOriginalMove.candidateApp,
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "candidate-desktop",
  );

  const afterCandidateInstall = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-after-candidate-install",
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("bootstrap", afterCandidateInstall),
      )
    ).exitCode,
    0,
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("begin", afterCandidateInstall),
      )
    ).exitCode,
    0,
  );
  const staleOriginalRetainedJournal = JSON.parse(
    await readFile(afterCandidateInstall.journalPath, "utf8"),
  );
  await writeFile(
    afterCandidateInstall.journalPath,
    `${JSON.stringify({ ...staleOriginalRetainedJournal, stage: "original-retained" }, null, 2)}\n`,
    { mode: 0o600 },
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("recover", afterCandidateInstall),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  assert.equal(
    await readFile(
      join(
        afterCandidateInstall.approvedApp,
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "sealed-desktop",
  );

  const afterOriginalRestore = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-after-original-restore",
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("bootstrap", afterOriginalRestore),
      )
    ).exitCode,
    0,
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("begin", afterOriginalRestore),
      )
    ).exitCode,
    0,
  );
  await rename(
    afterOriginalRestore.approvedApp,
    join(afterOriginalRestore.runRoot, "tested-approved-candidate.app"),
  );
  await rename(
    join(afterOriginalRestore.runRoot, "sealed-approved-original.app"),
    afterOriginalRestore.approvedApp,
  );
  const originalRestoredJournal = JSON.parse(
    await readFile(afterOriginalRestore.journalPath, "utf8"),
  );
  await writeFile(
    afterOriginalRestore.journalPath,
    `${JSON.stringify({ ...originalRestoredJournal, stage: "candidate-retained" }, null, 2)}\n`,
    { mode: 0o600 },
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("recover", afterOriginalRestore),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  assert.equal(
    await readFile(
      join(
        afterOriginalRestore.approvedApp,
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "sealed-desktop",
  );

  const afterCandidateRetain = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-after-candidate-retain",
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("bootstrap", afterCandidateRetain),
      )
    ).exitCode,
    0,
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("begin", afterCandidateRetain),
      )
    ).exitCode,
    0,
  );
  await rename(
    afterCandidateRetain.approvedApp,
    join(afterCandidateRetain.runRoot, "tested-approved-candidate.app"),
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("recover", afterCandidateRetain),
  );
  assert.equal(identityResult.exitCode, 0, identityResult.stderr);
  assert.equal(
    await readFile(
      join(
        afterCandidateRetain.approvedApp,
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "sealed-desktop",
  );

  const unknownCandidate = await createIdentityScenario(
    workingDirectory,
    "dev.dakia.fictional-provider-reader.run-identitytest",
    "identity-unknown-candidate",
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("bootstrap", unknownCandidate),
      )
    ).exitCode,
    0,
  );
  assert.equal(
    (
      await runApprovedIdentityEngine(
        approvedIdentityHeredoc,
        identityArgs("begin", unknownCandidate),
      )
    ).exitCode,
    0,
  );
  await writeFile(
    join(unknownCandidate.approvedApp, "Contents", "Resources", "fixture.txt"),
    "unexpected-live-change",
  );
  identityResult = await runApprovedIdentityEngine(
    approvedIdentityHeredoc,
    identityArgs("recover", unknownCandidate),
  );
  assert.notEqual(
    identityResult.exitCode,
    0,
    "recovery must not move a bundle with an unknown full inventory",
  );
  assert.match(identityResult.stderr, /unrecognized inventory/);
  assert.equal(
    await readFile(
      join(
        unknownCandidate.runRoot,
        "sealed-approved-original.app",
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "sealed-desktop",
  );

  assert.match(
    launchServicesRegistrationHeredoc,
    /const LS_REGISTER = '\/System\/Library\/Frameworks\/CoreServices\.framework\/Frameworks\/LaunchServices\.framework\/Support\/lsregister';/,
    "registration must use the fixed system LaunchServices registrar",
  );
  assert.match(
    launchServicesRegistrationHeredoc,
    /LSCopyApplicationURLsForBundleIdentifier/,
    "registration must verify the bundle through LaunchServices rather than a plist field",
  );
  assert.match(
    launchServicesRegistrationHeredoc,
    /await execFile\(SWIFT, \['-module-cache-path', moduleCachePath, '-e', SWIFT_LOOKUP, value\], \{ timeout: 15_000, maxBuffer: 4_096, windowsHide: true \}\);/,
    "the fixed Swift lookup must use only its bounded private module cache",
  );
  assert.match(
    launchServicesRegistrationHeredoc,
    /candidateLookup\.urls\.length === 0 && candidateLookup\.errorDomain === 'NSOSStatusErrorDomain' && candidateLookup\.errorCode === -10814/,
    "only the typed zero-URL LaunchServices-unavailable result may use the partial native fallback",
  );
  assert.match(
    launchServicesRegistrationHeredoc,
    /controlLookup = await lookup\('com\.apple\.TextEdit'\);/,
    "the partial native fallback must run the same structured Swift lookup against TextEdit",
  );
  assert.match(
    notificationPreflightWarningPredicate,
    /desktop notifications are unavailable because the application identity could not be verified/,
    "the partial native fallback must require the product notification warning before acceptance continues",
  );
  assert.match(
    notificationPreflightWarningPredicate,
    /LaunchServices did not resolve application \$expected_identifier/,
    "the partial native fallback must require the product notification-preflight warning before acceptance continues",
  );
  assert.match(
    launchServicesRegistrationHeredoc,
    /await ownedRegular\(sourceEvidencePath, 0o600\);/,
    "registration must consume only the private source-freeze evidence it just recorded",
  );
  assert.doesNotMatch(
    launchServicesRegistrationHeredoc,
    /dakia-desktop"\s*,\s*\[|spawn\(|execFile\([^,]+, \[[^\]]*dakia-desktop/,
    "registration must not launch the fixture executable",
  );
  const registrationCall = script.indexOf(
    'launch_services_registration_receipt="$(register_approved_identity_with_launch_services)"',
  );
  const directLaunch = script.indexOf(
    '"$app_root/Contents/MacOS/dakia-desktop" >',
  );
  assert.ok(
    registrationCall >
      script.indexOf(
        "verify_source_snapshot before-launch-services-registration",
      ),
    "registration must follow the final source snapshot",
  );
  assert.ok(
    directLaunch > registrationCall,
    "direct launch must wait for successful registration",
  );

  const notificationWarningIdentifier =
    "dev.dakia.fictional-provider-reader.run-warningtest";
  const notificationWarningLogs = {
    complete: [
      "desktop notifications are unavailable because the application identity could not be verified",
      `LaunchServices did not resolve application ${notificationWarningIdentifier}`,
    ].join("\n"),
    genericOnly:
      "desktop notifications are unavailable because the application identity could not be verified\n",
    launchServicesOnly: `LaunchServices did not resolve application ${notificationWarningIdentifier}\n`,
    wrongIdentifier: [
      "desktop notifications are unavailable because the application identity could not be verified",
      "LaunchServices did not resolve application dev.dakia.fictional-provider-reader.run-wrongidentity",
    ].join("\n"),
    identifierWithPrefix: [
      "desktop notifications are unavailable because the application identity could not be verified",
      `LaunchServices did not resolve application x${notificationWarningIdentifier}`,
    ].join("\n"),
    identifierWithSuffix: [
      "desktop notifications are unavailable because the application identity could not be verified",
      `LaunchServices did not resolve application ${notificationWarningIdentifier}extra`,
    ].join("\n"),
    identifierWithDotSuffix: [
      "desktop notifications are unavailable because the application identity could not be verified",
      `LaunchServices did not resolve application ${notificationWarningIdentifier}.evil`,
    ].join("\n"),
    identifierWithHyphenSuffix: [
      "desktop notifications are unavailable because the application identity could not be verified",
      `LaunchServices did not resolve application ${notificationWarningIdentifier}-evil`,
    ].join("\n"),
  };
  for (const [name, contents] of Object.entries(notificationWarningLogs)) {
    const appLog = join(workingDirectory, `notification-preflight-${name}.log`);
    await writeFile(appLog, contents);
    assert.equal(
      await notificationPreflightWarningIsAccepted(
        notificationPreflightWarningPredicate,
        appLog,
        notificationWarningIdentifier,
      ),
      name === "complete",
      `the exact runtime notification-preflight predicate must ${name === "complete" ? "accept both required warning records" : `reject ${name}`}`,
    );
  }

  async function createRegistrationScenario(name) {
    const scenario = await createIdentityScenario(
      workingDirectory,
      `dev.dakia.fictional-provider-reader.run-${name.replaceAll("-", "")}`,
      `launch-services-${name}`,
    );
    let result = await runApprovedIdentityEngine(
      approvedIdentityHeredoc,
      identityArgs("bootstrap", scenario),
    );
    assert.equal(result.exitCode, 0, result.stderr);
    result = await runApprovedIdentityEngine(
      approvedIdentityHeredoc,
      identityArgs("begin", scenario),
    );
    assert.equal(result.exitCode, 0, result.stderr);
    await writeFile(
      join(scenario.runRoot, "source-before-launch-services-registration.json"),
      `${JSON.stringify(
        {
          status: "passed",
          baseline: { digest: "a".repeat(64), fileCount: 935 },
          current: { digest: "a".repeat(64), fileCount: 935 },
        },
        null,
        2,
      )}\n`,
      { mode: 0o600 },
    );
    return {
      ...scenario,
      candidateDesktopSha256: digest("candidate-desktop"),
      candidateCliSha256: digest("candidate-cli"),
    };
  }

  async function createLaunchServicesMock(root, name, behavior = "ok") {
    const registrar = join(root, `${name}-lsregister`);
    const lookup = join(root, `${name}-swift`);
    await writeFile(
      registrar,
      `#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$DAKIA_TEST_LSREGISTER_LOG\"\ncase \"${behavior}\" in failure) exit 23 ;; signal) kill -TERM $$ ;; esac\nexit 0\n`,
      { mode: 0o755 },
    );
    await writeFile(
      lookup,
      '#!/bin/sh\nprintf \'%s\\n\' "$@" >> "$DAKIA_TEST_LSLOOKUP_LOG"\nlast=\nfor argument in "$@"; do last="$argument"; done\nif [ "$last" = com.apple.TextEdit ]; then printf \'%s\' "$DAKIA_TEST_LS_CONTROL_JSON"; else printf \'%s\' "$DAKIA_TEST_LS_CANDIDATE_JSON"; fi\n',
      { mode: 0o755 },
    );
    return { registrar, lookup };
  }

  function lookupJson(urls, errorDomain = null, errorCode = null) {
    return JSON.stringify({ urls, errorDomain, errorCode });
  }

  async function runMockedLaunchServicesRegistration(
    scenario,
    behavior,
    candidateLookup = lookupJson([scenario.approvedApp]),
    controlLookup = lookupJson(["/System/Applications/TextEdit.app"]),
  ) {
    const mocks = await createLaunchServicesMock(
      workingDirectory,
      scenario.identifier.slice(-12),
      behavior,
    );
    const mocked = launchServicesRegistrationHeredoc
      .replace(
        "'/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister'",
        JSON.stringify(mocks.registrar),
      )
      .replace("'/usr/bin/swift'", JSON.stringify(mocks.lookup));
    assert.notEqual(mocked, launchServicesRegistrationHeredoc);
    const logPath = join(scenario.runRoot, "mock-lsregister.log");
    const lookupLogPath = join(scenario.runRoot, "mock-swift.log");
    const result = await runLaunchServicesRegistration(
      mocked,
      [
        scenario.approvedRoot,
        scenario.approvedApp,
        scenario.recordPath,
        scenario.journalPath,
        scenario.identifier,
        scenario.candidateDesktopSha256,
        scenario.candidateCliSha256,
        scenario.runRoot,
      ],
      {
        DAKIA_TEST_LSREGISTER_LOG: logPath,
        DAKIA_TEST_LSLOOKUP_LOG: lookupLogPath,
        DAKIA_TEST_LS_CANDIDATE_JSON: candidateLookup,
        DAKIA_TEST_LS_CONTROL_JSON: controlLookup,
      },
    );
    return { ...result, logPath, lookupLogPath, mocks };
  }

  const registeredScenario = await createRegistrationScenario("registered");
  let registrationResult = await runMockedLaunchServicesRegistration(
    registeredScenario,
    "ok",
  );
  assert.equal(registrationResult.exitCode, 0, registrationResult.stderr);
  assert.equal(
    await readFile(registrationResult.logPath, "utf8"),
    `-f\n${registeredScenario.approvedApp}\n`,
  );
  const lookupArguments = (
    await readFile(registrationResult.lookupLogPath, "utf8")
  ).split("\n");
  assert.deepEqual(lookupArguments.slice(0, 3), [
    "-module-cache-path",
    join(registeredScenario.runRoot, "swift-module-cache"),
    "-e",
  ]);
  assert.equal(
    lookupArguments.at(-2),
    registeredScenario.identifier,
    "the fixed Swift lookup must receive the approved bundle identifier after its source",
  );
  assert.equal(
    (await stat(join(registeredScenario.runRoot, "swift-module-cache"))).mode &
      0o777,
    0o700,
  );
  const registrationReceiptPath = join(
    registeredScenario.runRoot,
    "launch-services-registration.json",
  );
  const registrationReceipt = JSON.parse(
    await readFile(registrationReceiptPath, "utf8"),
  );
  assert.equal((await stat(registrationReceiptPath)).mode & 0o777, 0o600);
  assert.deepEqual(
    {
      approvedApp: registrationReceipt.approvedApp,
      bundleIdentifier: registrationReceipt.bundleIdentifier,
      desktopSha256: registrationReceipt.desktopSha256,
      cliSha256: registrationReceipt.cliSha256,
      sourceDigest: registrationReceipt.sourceDigest,
      sourceFileCount: registrationReceipt.sourceFileCount,
      registrationCommand: registrationReceipt.registrationCommand,
      lookup: registrationReceipt.lookup,
      swiftModuleCachePath: registrationReceipt.swiftModuleCachePath,
      resolvedApplication: registrationReceipt.resolvedApplication,
    },
    {
      approvedApp: registeredScenario.approvedApp,
      bundleIdentifier: registeredScenario.identifier,
      desktopSha256: registeredScenario.candidateDesktopSha256,
      cliSha256: registeredScenario.candidateCliSha256,
      sourceDigest: "a".repeat(64),
      sourceFileCount: 935,
      registrationCommand: registrationResult.mocks.registrar,
      lookup: "LSCopyApplicationURLsForBundleIdentifier",
      swiftModuleCachePath: join(
        registeredScenario.runRoot,
        "swift-module-cache",
      ),
      resolvedApplication: registeredScenario.approvedApp,
    },
  );
  assert.equal(
    await readFile(
      join(
        registeredScenario.approvedApp,
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "candidate-desktop",
    "registration must not launch or alter the candidate executable",
  );

  const mismatchedScenario = await createRegistrationScenario("path-mismatch");
  const mismatchedMocks = await createLaunchServicesMock(
    workingDirectory,
    "path-mismatch",
  );
  const mismatchedHeredoc = launchServicesRegistrationHeredoc
    .replace(
      "'/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister'",
      JSON.stringify(mismatchedMocks.registrar),
    )
    .replace("'/usr/bin/swift'", JSON.stringify(mismatchedMocks.lookup));
  registrationResult = await runLaunchServicesRegistration(
    mismatchedHeredoc,
    [
      mismatchedScenario.approvedRoot,
      join(mismatchedScenario.runRoot, "tested-approved-candidate.app"),
      mismatchedScenario.recordPath,
      mismatchedScenario.journalPath,
      mismatchedScenario.identifier,
      mismatchedScenario.candidateDesktopSha256,
      mismatchedScenario.candidateCliSha256,
      mismatchedScenario.runRoot,
    ],
    {
      DAKIA_TEST_LSREGISTER_LOG: join(
        mismatchedScenario.runRoot,
        "unexpected.log",
      ),
      DAKIA_TEST_LSLOOKUP_PATH: mismatchedScenario.approvedApp,
    },
  );
  assert.notEqual(registrationResult.exitCode, 0);
  assert.equal(
    registrationResult.stderr,
    "LaunchServices registration could not be verified for the sealed fictional app.\n",
  );
  await assert.rejects(
    () => readFile(join(mismatchedScenario.runRoot, "unexpected.log")),
    /ENOENT/,
    "a path mismatch must fail before LaunchServices registration or app launch",
  );

  const unsealedSourceEvidence = await createRegistrationScenario(
    "unsealed-source-evidence",
  );
  await chmod(
    join(
      unsealedSourceEvidence.runRoot,
      "source-before-launch-services-registration.json",
    ),
    0o644,
  );
  registrationResult = await runMockedLaunchServicesRegistration(
    unsealedSourceEvidence,
    "ok",
  );
  assert.notEqual(registrationResult.exitCode, 0);
  assert.equal(
    registrationResult.stderr,
    "LaunchServices registration could not be verified for the sealed fictional app.\n",
  );
  await assert.rejects(
    () => readFile(registrationResult.logPath),
    /ENOENT/,
    "unsealed source evidence must fail before LaunchServices registration or app launch",
  );

  const occupiedModuleCache = await createRegistrationScenario(
    "occupied-module-cache",
  );
  await mkdir(join(occupiedModuleCache.runRoot, "swift-module-cache"), {
    mode: 0o700,
  });
  registrationResult = await runMockedLaunchServicesRegistration(
    occupiedModuleCache,
    "ok",
  );
  assert.notEqual(registrationResult.exitCode, 0);
  assert.equal(
    registrationResult.stderr,
    "LaunchServices registration could not be verified for the sealed fictional app.\n",
  );
  await assert.rejects(
    () => readFile(registrationResult.logPath),
    /ENOENT/,
    "a preexisting module cache must fail before LaunchServices registration or app launch",
  );

  const resolvedPathMismatch = await createRegistrationScenario(
    "lookup-path-mismatch",
  );
  registrationResult = await runMockedLaunchServicesRegistration(
    resolvedPathMismatch,
    "ok",
    lookupJson([join(resolvedPathMismatch.runRoot, "a-different-app.app")]),
  );
  assert.notEqual(registrationResult.exitCode, 0);
  assert.equal(
    registrationResult.stderr,
    "LaunchServices registration could not be verified for the sealed fictional app.\n",
  );
  assert.equal(
    await readFile(registrationResult.logPath, "utf8"),
    `-f\n${resolvedPathMismatch.approvedApp}\n`,
    "registration must target only the exact approved path before checking LaunchServices resolution",
  );
  await assert.rejects(
    () =>
      readFile(
        join(resolvedPathMismatch.runRoot, "launch-services-registration.json"),
      ),
    /ENOENT/,
    "a LaunchServices path mismatch must not leave a success receipt or launch the app",
  );
  assert.equal(
    await readFile(
      join(
        resolvedPathMismatch.approvedApp,
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "utf8",
    ),
    "candidate-desktop",
    "a LaunchServices path mismatch must not launch or alter the candidate executable",
  );

  const unavailableScenario = await createRegistrationScenario(
    "typed-launch-services-unavailable",
  );
  registrationResult = await runMockedLaunchServicesRegistration(
    unavailableScenario,
    "ok",
    lookupJson([], "NSOSStatusErrorDomain", -10814),
  );
  assert.equal(registrationResult.exitCode, 0, registrationResult.stderr);
  const unavailableReceiptPath = join(
    unavailableScenario.runRoot,
    "launch-services-registration-unavailable.json",
  );
  assert.equal(registrationResult.stdout, `${unavailableReceiptPath}\n`);
  const unavailableReceipt = JSON.parse(
    await readFile(unavailableReceiptPath, "utf8"),
  );
  assert.equal((await stat(unavailableReceiptPath)).mode & 0o777, 0o600);
  assert.deepEqual(
    {
      purpose: unavailableReceipt.purpose,
      status: unavailableReceipt.status,
      candidateLookup: unavailableReceipt.candidateLookup,
      controlLookup: unavailableReceipt.controlLookup,
      notificationDelivery: unavailableReceipt.notificationDelivery,
      bundleIdentifier: unavailableReceipt.bundleIdentifier,
    },
    {
      purpose: "dakia-native-launch-services-registration-unavailable-v1",
      status: "unavailable",
      candidateLookup: {
        urls: [],
        errorDomain: "NSOSStatusErrorDomain",
        errorCode: -10814,
      },
      controlLookup: {
        urls: ["/System/Applications/TextEdit.app"],
        errorDomain: null,
        errorCode: null,
      },
      notificationDelivery: "not-tested",
      bundleIdentifier: unavailableScenario.identifier,
    },
  );
  assert.equal(
    Object.hasOwn(unavailableReceipt, "resolvedApplication"),
    false,
    "an unavailable receipt must not claim a resolved application",
  );
  await assert.rejects(
    () =>
      readFile(
        join(unavailableScenario.runRoot, "launch-services-registration.json"),
      ),
    /ENOENT/,
    "an unavailable receipt must remain separate from a successful registration receipt",
  );
  const unavailableLookupLog = await readFile(
    registrationResult.lookupLogPath,
    "utf8",
  );
  assert.match(
    unavailableLookupLog,
    new RegExp(unavailableScenario.identifier),
  );
  assert.match(unavailableLookupLog, /com\.apple\.TextEdit/);

  for (const [name, candidateLookup, controlLookup, expectedStage] of [
    ["empty-lookup", lookupJson([]), undefined, "candidate-lookup"],
    [
      "wrong-domain",
      lookupJson([], "NSCocoaErrorDomain", -10814),
      undefined,
      "candidate-lookup",
    ],
    [
      "wrong-code",
      lookupJson([], "NSOSStatusErrorDomain", -10813),
      undefined,
      "candidate-lookup",
    ],
    ["malformed-lookup", "{", undefined, "candidate-lookup"],
    [
      "wrong-control",
      lookupJson([], "NSOSStatusErrorDomain", -10814),
      lookupJson(["/Applications/Forged.app"]),
      "unavailable-control",
    ],
  ]) {
    const failedLookupScenario = await createRegistrationScenario(name);
    registrationResult = await runMockedLaunchServicesRegistration(
      failedLookupScenario,
      "ok",
      candidateLookup,
      controlLookup,
    );
    assert.notEqual(registrationResult.exitCode, 0);
    assert.equal(
      registrationResult.stderr,
      "LaunchServices registration could not be verified for the sealed fictional app.\n",
    );
    const stageReceipt = JSON.parse(
      await readFile(
        join(
          failedLookupScenario.runRoot,
          "launch-services-registration-stage.json",
        ),
        "utf8",
      ),
    );
    assert.deepEqual(stageReceipt, {
      schemaVersion: 1,
      purpose: "dakia-native-launch-services-registration-stage-v1",
      status: "failed",
      stage: expectedStage,
    });
    assert.equal(
      (
        await stat(
          join(
            failedLookupScenario.runRoot,
            "launch-services-registration-stage.json",
          ),
        )
      ).mode & 0o777,
      0o600,
    );
  }

  for (const behavior of ["failure", "signal"]) {
    const failureScenario = await createRegistrationScenario(
      `registration-${behavior}`,
    );
    registrationResult = await runMockedLaunchServicesRegistration(
      failureScenario,
      behavior,
    );
    assert.notEqual(registrationResult.exitCode, 0);
    assert.equal(
      registrationResult.stderr,
      "LaunchServices registration could not be verified for the sealed fictional app.\n",
    );
    await assert.rejects(
      () =>
        readFile(
          join(failureScenario.runRoot, "launch-services-registration.json"),
        ),
      /ENOENT/,
      `${behavior} registration failure must not leave a success receipt`,
    );
    assert.deepEqual(
      JSON.parse(
        await readFile(
          join(
            failureScenario.runRoot,
            "launch-services-registration-stage.json",
          ),
          "utf8",
        ),
      ),
      {
        schemaVersion: 1,
        purpose: "dakia-native-launch-services-registration-stage-v1",
        status: "failed",
        stage: "registrar",
      },
    );
  }

  const errorOutput = join(workingDirectory, "manifest-directory");
  await mkdir(errorOutput);
  const commandsBeforeWriteFailure = connections.flatMap(
    (connection) => connection.commands,
  ).length;
  const writeFailure = await runExactHeredoc({
    heredoc,
    port: address.port,
    caCertificate: credentials.caCertificate,
    manifestPath: errorOutput,
  });
  assert.notEqual(
    writeFailure.exitCode,
    0,
    "a manifest output directory must fail fixture setup",
  );
  assert.match(writeFailure.stderr, /EISDIR/);
  const writeFailureCommands = connections
    .flatMap((connection) => connection.commands)
    .slice(commandsBeforeWriteFailure);
  assert(writeFailureCommands.some((command) => /^AUTH PLAIN /i.test(command)));
  assert.equal(
    writeFailureCommands.some((command) =>
      /^(MAIL FROM:|RCPT TO:|DATA$)/i.test(command),
    ),
    false,
  );

  for (const socket of activeSockets) socket.destroy();
  server.close();
  await withTimeout(once(server, "close"), "loopback SMTP server shutdown");
  activeServer = undefined;
  console.log("native provider Reader SMTP fixture runtime: passed");
} finally {
  for (const socket of activeSockets) socket.destroy();
  if (activeServer?.listening) activeServer.close();
  await rm(workingDirectory, { recursive: true, force: true });
}
