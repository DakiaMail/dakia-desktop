import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const harnessPath = join(root, "native-provider-reader-acceptance.sh");
const helperPath = join(root, "native-provider-reader-attachment-binding.py");
const harness = readFileSync(harnessPath, "utf8");
const helper = readFileSync(helperPath, "utf8");

function exactSmtpHeredoc(source) {
  const startMarker =
    'node --input-type=module - "$smtp_port" "$certificate_root/ca.crt" "$fixture_http_port" "$fixture_tls_probe_port" "$mailto_regressions" <<\'NODE\'\n';
  const start = source.indexOf(startMarker);
  assert.notEqual(start, -1, "stock SMTP heredoc start must remain exact");
  const bodyStart = start + startMarker.length;
  const end = source.indexOf("\nNODE\n\npython3 - ", bodyStart);
  assert.notEqual(end, -1, "stock SMTP heredoc end must remain exact");
  return source.slice(bodyStart, end);
}

function submittedExpression(source, subject, functionName) {
  const smtp = exactSmtpHeredoc(source);
  const subjectIndex = smtp.indexOf(`'Subject: ${subject}'`);
  assert.notEqual(
    subjectIndex,
    -1,
    `${subject} must remain in the SMTP fixture`,
  );
  const call = `await ${functionName}(`;
  const start = smtp.lastIndexOf(call, subjectIndex);
  assert.notEqual(start, -1, `${subject} submit call must remain exact`);
  const marker = "].join('\\r\\n'));";
  const end = smtp.indexOf(marker, subjectIndex);
  assert.notEqual(end, -1, `${subject} submit expression must remain exact`);
  return smtp.slice(start + call.length, end + marker.length - 2);
}

function actualRaw(source, subject, functionName) {
  const expression = submittedExpression(source, subject, functionName);
  return Buffer.from(
    Function(
      "Buffer",
      "fixtureOrigin",
      "tlsProbeOrigin",
      "fixtureHttpPort",
      "mailtoAnchorHtml",
      `return (${expression});`,
    )(
      Buffer,
      "http://127.0.0.1:48731",
      "//127.0.0.1:48732",
      48731,
      "<a>mailto fixture</a>",
    ),
    "utf8",
  );
}

function inspectThroughPython(functionName, raw) {
  const program = [
    "import importlib.util,json,sys",
    "spec=importlib.util.spec_from_file_location('binding',sys.argv[1])",
    "module=importlib.util.module_from_spec(spec)",
    "spec.loader.exec_module(module)",
    `result=module.${functionName}(sys.stdin.buffer.read()${functionName === "inspect_raw" ? ",account_id=module.ACCOUNT_ID,uid_validity=77" : ""})`,
    "print(json.dumps(result,separators=(',',':')))",
  ].join(";");
  return spawnSync("python3", ["-c", program, helperPath], {
    input: raw,
    encoding: "utf8",
  });
}

function index(label, needle, source = harness) {
  const found = source.indexOf(needle);
  assert.notEqual(found, -1, `${label} must be present`);
  return found;
}

function validateWiring(source) {
  assert.equal(
    (source.match(/--attachment-interruption\)/g) || []).length,
    1,
    "the opt-in flag must have one parser branch",
  );
  assert.match(
    source,
    /attachment_interruption=false/,
    "default behavior must keep the fault overlay disabled",
  );
  assert.match(
    source,
    /DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT=1/,
    "the collision account must be provisioned in fault mode",
  );
  assert.ok(
    index("account-A read-only verification", 'python3 - "$imap_port"') <
      index(
        "binding preflight",
        'python3 "$repo_root/scripts/native-provider-reader-attachment-binding.py"',
      ),
  );
  assert.ok(
    index(
      "binding preflight",
      'python3 "$repo_root/scripts/native-provider-reader-attachment-binding.py"',
    ) < index("prepare-only exit", 'if [[ "$prepare_only" == "true" ]]'),
  );
  assert.ok(
    index(
      "inner interruption overlay",
      'native-fictional-attachment-interruption-proxy.mjs" serve',
    ) < index("stock outer proxy", 'native-fictional-imap-proxy.mjs" \\\n'),
    "the inner TLS overlay must start before the stock controlled outer proxy",
  );
  assert.match(
    source,
    /backend_imap_port="\$\(node --input-type=module - "\$run_root\/attachment-interruption-overlay\.json"/,
  );
  assert.match(source, /--account-id '62a9320c-04f5-47c2-b4fa-d4952f0b6121'/);
  assert.match(source, /--collision-email 'fictional-reader-two@localhost'/);
  assert.match(source, /attachmentSection !== '3'/);
  assert.match(source, /INBOX:3:mime-v1:3/);
  assert.doesNotMatch(
    source,
    /GMAIL_|GMAIL_PASSWORD|DAKIA_NATIVE_MAIL_SAFETY_PASSWORD=.*\$\{/,
  );
  const cleanupOuter = index("outer cleanup", 'stop_owned_child "$proxy_pid"');
  const cleanupInner = index(
    "inner cleanup",
    'stop_owned_child "$attachment_interruption_pid"',
  );
  assert.ok(
    cleanupOuter < cleanupInner,
    "the outer proxy must stop before its inner upstream",
  );
}

function validatePrepareOnlyOrdering(source) {
  const defaultExit = index(
    "default prepare-only exit",
    'if [[ "$prepare_only" == "true" && "$attachment_interruption" != true ]]',
    source,
  );
  const inner = index(
    "inner interruption overlay",
    'native-fictional-attachment-interruption-proxy.mjs" serve',
    source,
  );
  const outer = index(
    "stock outer proxy",
    'native-fictional-imap-proxy.mjs" \\\n',
    source,
  );
  const outerReady = index(
    "validated outer proxy port",
    'imap_port="$(node -e',
    source,
  );
  const optInExit = index(
    "opt-in prepare-only exit",
    "Fictional provider fixture and attachment interruption proxies are ready.",
    source,
  );
  const profileBootstrap = index(
    "profile bootstrap boundary",
    "export DAKIA_NATIVE_MAIL_SAFETY_FIXTURE=1",
    source,
  );
  assert.ok(
    defaultExit < inner,
    "default prepare-only must still exit before any proxy layering",
  );
  assert.ok(
    inner < outer && outer < outerReady,
    "opt-in preparation must start inner then outer and validate readiness",
  );
  assert.ok(
    outerReady < optInExit && optInExit < profileBootstrap,
    "opt-in prepare-only must exit after both proxies are ready and before profile bootstrap/build",
  );
}

test("patched harness has valid shell syntax and fail-closed option composition", () => {
  execFileSync("bash", ["-n", harnessPath]);
  const result = spawnSync(
    "bash",
    [
      harnessPath,
      "--attachment-interruption",
      "--bootstrap-approved-identity-record",
      "--from-run-root",
      "/does-not-exist",
    ],
    { encoding: "utf8" },
  );
  assert.equal(result.status, 64);
  assert.match(result.stderr, /cannot be combined with run options/);
});

test("faithful preflight regression suite passes", () => {
  const result = spawnSync(
    "python3",
    [join(root, "native-provider-reader-attachment-binding.test.py")],
    { encoding: "utf8" },
  );
  assert.equal(result.status, 0, result.stderr || result.stdout);
  const count = /Ran (\d+) tests?/.exec(`${result.stdout}\n${result.stderr}`);
  assert.ok(
    count && Number(count[1]) > 0,
    "Python preflight suite must execute tests",
  );
});

test("optional mode binds account A UID3 before layering the stock outer proxy", () => {
  validateWiring(harness);
  validatePrepareOnlyOrdering(harness);
});

test("ownership sentinels are created mode 0600 under caller umask 022", () => {
  const match =
    /\(\n  umask 0177\n  printf '%s\\n' 'dakia-native-mail-safety-fixture-v1' > "\$run_root\/\.dakia-native-mail-safety-root"\n  printf '%s\\n' 'dakia-native-mail-safety-fixture-v1' > "\$profile_root\/\.dakia-native-mail-safety-profile"\n\)/.exec(
      harness,
    );
  assert.ok(match, "exact private sentinel creation block must be present");
  const temporary = mkdtempSync(join(tmpdir(), "dakia-native-sentinel."));
  const profile = join(temporary, "profile");
  mkdirSync(profile, { mode: 0o700 });
  const result = spawnSync(
    "bash",
    [
      "-c",
      `set -euo pipefail\nrun_root="$1"\nprofile_root="$2"\numask 0022\n${match[0]}`,
      "sentinel-test",
      temporary,
      profile,
    ],
    { encoding: "utf8" },
  );
  assert.equal(result.status, 0, result.stderr);
  for (const [path, text] of [
    [
      join(temporary, ".dakia-native-mail-safety-root"),
      "dakia-native-mail-safety-fixture-v1\n",
    ],
    [
      join(profile, ".dakia-native-mail-safety-profile"),
      "dakia-native-mail-safety-fixture-v1\n",
    ],
  ]) {
    assert.equal(lstatSync(path).mode & 0o777, 0o600);
    assert.equal(readFileSync(path, "utf8"), text);
  }
  rmSync(temporary, { recursive: true });
});

test("prepare-only ordering oracle rejects an exit before outer readiness", () => {
  const exit = `if [[ "$prepare_only" == "true" ]]; then\n  printf 'Fictional provider fixture and attachment interruption proxies are ready.\\n'\n  exit 0\nfi\n`;
  assert.throws(
    () =>
      validatePrepareOnlyOrdering(
        harness
          .replace(exit, "")
          .replace(
            'node "$repo_root/scripts/native-fictional-imap-proxy.mjs" \\\n',
            `${exit}node "$repo_root/scripts/native-fictional-imap-proxy.mjs" \\\n`,
          ),
      ),
    /after both proxies are ready/,
  );
});

test("the production SMTP UID3 fixtures satisfy both account-specific preflight parsers", () => {
  const accountA = actualRaw(harness, "Remote content safety check", "submit");
  const accountB = actualRaw(
    harness,
    "Account two collision safety check",
    "submitSecond",
  );
  const inspectedA = inspectThroughPython("inspect_raw", accountA);
  assert.equal(inspectedA.status, 0, inspectedA.stderr);
  const artifactA = JSON.parse(inspectedA.stdout);
  assert.equal(artifactA.attachment.section, "3");
  assert.equal(
    artifactA.attachment.decodedSha256,
    "666dec5256feac267cd6c46b97107397c0a87fcde80a932321819f5a1cf2955e",
  );
  const inspectedB = inspectThroughPython("inspect_collision_raw", accountB);
  assert.equal(inspectedB.status, 0, inspectedB.stderr);
  const artifactB = JSON.parse(inspectedB.stdout);
  assert.equal(artifactB.attachmentFilename, "account-two-notes.txt");
  assert.equal(
    artifactB.attachmentSha256,
    "8d75a6c97173c8be55b14daccce4f7cc4cfb2821ce13d3cb98fe2b2b28796541",
  );
  assert.notEqual(artifactA.rawSha256, artifactB.rawSha256);
});

test("production fixture sensitivity rejects changed account A and copied account B content", () => {
  const changedA = harness.replace(
    "fictional safe attachment bytes\\n",
    "changed safe attachment bytes\\n",
  );
  const inspectedA = inspectThroughPython(
    "inspect_raw",
    actualRaw(changedA, "Remote content safety check", "submit"),
  );
  assert.notEqual(inspectedA.status, 0);
  assert.match(inspectedA.stderr, /safe attachment identity changed/);

  const accountA = actualRaw(harness, "Remote content safety check", "submit");
  const copiedB = inspectThroughPython("inspect_collision_raw", accountA);
  assert.notEqual(copiedB.status, 0);
  assert.match(copiedB.stderr, /collision UID3 subject changed/);
});

test("wiring oracle rejects a stale section-two assumption", () => {
  assert.throws(
    () =>
      validateWiring(
        harness
          .replaceAll("mime-v1:3", "mime-v1:2")
          .replace("attachmentSection !== '3'", "attachmentSection !== '2'"),
      ),
    /attachmentSection|mime-v1:3/,
  );
});

test("preflight helper is loopback-only, TLS 1.2+, read-neutral, and never reads a password environment variable", () => {
  assert.match(helper, /args\.host == "127\.0\.0\.1"/);
  assert.match(helper, /context\.minimum_version = ssl\.TLSVersion\.TLSv1_2/);
  assert.match(helper, /client\.select\(MAILBOX, readonly=True\)/);
  assert.match(helper, /BODY\.PEEK\[\]/);
  assert.match(helper, /password = sys\.stdin\.buffer\.readline/);
  assert.doesNotMatch(helper, /getenv|environ/);
  assert.match(helper, /stat\.S_IMODE\(info\.st_mode\) == 0o600/);
});
