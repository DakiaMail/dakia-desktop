#!/usr/bin/env node

import { createHash } from "node:crypto";
import {
  access,
  constants,
  lstat,
  link,
  open,
  readFile,
  readdir,
  readlink,
  realpath,
  unlink,
} from "node:fs/promises";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { basename, dirname, join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { execFile, spawn } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const PRIMARY_ACCOUNT_ID = "62a9320c-04f5-47c2-b4fa-d4952f0b6121";
const SECONDARY_ACCOUNT_ID = "62a9320c-04f5-47c2-b4fa-d4952f0b6122";
const PRIMARY_UID3_BODY_QUERY =
  "Fictional provider body reached the real Reader";
const PRIMARY_UID3_MESSAGE_ID = `${PRIMARY_ACCOUNT_ID}:INBOX:3`;
const PRIMARY_UID3_NORMALIZED_BODY_SHA256 =
  "9ea8bac4c145580dead7b8197b8c89157efd78053a1b12bd65b110407c45f3c5";
const PRIMARY_UID3_NORMALIZED_BODY_UTF8_BYTES = 925;
const EXPECTED_ACCOUNTS = new Map([
  [PRIMARY_ACCOUNT_ID, "fictional-reader@localhost"],
  [SECONDARY_ACCOUNT_ID, "fictional-reader-two@localhost"],
]);
const ROOT_MARKER = "dakia-native-mail-safety-fixture-v1\n";
const APPROVED_IDENTITY_CONTRACT =
  "dakia-native-provider-reader-approved-identity-v1";
const SNAPSHOT_TABLES = [
  "accounts",
  "attachments",
  "credentials",
  "deleted_account_tombstones",
  "mail_rebuild_jobs",
  "mailbox_action_tombstones",
  "message_content_cache",
  "messages",
  "send_attempts",
  "sent_correspondents",
  "starred_attachment_metadata",
  "starred_message_bodies",
];
const SHA256 = /^[a-f0-9]{64}$/;
const MAX_INFO_PLIST_BYTES = 1_048_576;
const MAX_PLIST_PARSER_OUTPUT_BYTES = 4_096;
const BUNDLE_INFO_PLIST_PARSER = String.raw`
import json
import plistlib
import sys

class RejectDuplicateDict(dict):
    def __setitem__(self, key, value):
        if key in self:
            raise ValueError("duplicate dictionary key")
        super().__setitem__(key, value)

source = sys.stdin.buffer.read()
if not (0 < len(source) <= 1048576):
    raise ValueError("invalid plist size")
value = plistlib.loads(source, dict_type=RejectDuplicateDict)
if type(value) is not RejectDuplicateDict:
    raise ValueError("plist root is not a dictionary")
identifier = value.get("CFBundleIdentifier")
name = value.get("CFBundleName")
if type(identifier) is not str or type(name) is not str:
    raise ValueError("plist identity fields are not strings")
print(json.dumps({"bundleIdentifier": identifier, "bundleName": name}, separators=(",", ":")))
`;

function fail(message) {
  throw new Error(message);
}

function invalidOrAmbiguousPlist() {
  fail("approved application plist is invalid or ambiguous");
}

function parseBundleInfoPlistBytes(source) {
  return new Promise((resolveParser, rejectParser) => {
    const child = spawn("/usr/bin/python3", ["-c", BUNDLE_INFO_PLIST_PARSER], {
      stdio: ["pipe", "pipe", "ignore"],
      windowsHide: true,
    });
    let settled = false;
    let output = Buffer.alloc(0);
    const settle = (callback) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      callback();
    };
    const rejectInvalid = () =>
      settle(() => rejectParser(new Error("invalid")));
    const timeout = setTimeout(() => {
      child.kill();
      rejectInvalid();
    }, 5_000);
    child.once("error", rejectInvalid);
    child.stdin.once("error", rejectInvalid);
    child.stdout.on("data", (chunk) => {
      if (settled) return;
      output = Buffer.concat([output, chunk]);
      if (output.length > MAX_PLIST_PARSER_OUTPUT_BYTES) {
        child.kill();
        rejectInvalid();
      }
    });
    child.once("close", (code, signal) => {
      if (settled) return;
      if (code !== 0 || signal !== null) return rejectInvalid();
      settle(() => resolveParser(output));
    });
    child.stdin.end(source);
  });
}

export async function parseApprovedBundleInfoPlist(
  plistPath,
  expectedBundleIdentifier,
  expectedBundleName,
) {
  try {
    const info = lstatSync(plistPath);
    if (
      !info.isFile() ||
      info.isSymbolicLink() ||
      info.uid !== process.getuid() ||
      info.size < 1 ||
      info.size > MAX_INFO_PLIST_BYTES ||
      realpathSync(plistPath) !== plistPath
    ) {
      invalidOrAmbiguousPlist();
    }
    const source = readFileSync(plistPath);
    if (source.length !== info.size) invalidOrAmbiguousPlist();
    const output = await parseBundleInfoPlistBytes(source);
    const decoded = JSON.parse(output.toString("utf8"));
    if (
      Object.getPrototypeOf(decoded) !== Object.prototype ||
      Object.keys(decoded).length !== 2 ||
      !Object.hasOwn(decoded, "bundleIdentifier") ||
      !Object.hasOwn(decoded, "bundleName") ||
      typeof decoded.bundleIdentifier !== "string" ||
      typeof decoded.bundleName !== "string" ||
      decoded.bundleIdentifier !== expectedBundleIdentifier ||
      decoded.bundleName !== expectedBundleName
    ) {
      invalidOrAmbiguousPlist();
    }
    return decoded;
  } catch {
    invalidOrAmbiguousPlist();
  }
}

export function parseArguments(argv) {
  const values = {};
  for (let index = 0; index < argv.length; index += 2) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (!flag?.startsWith("--") || !value || value.startsWith("--")) {
      fail(`invalid argument near ${flag ?? "end of command"}`);
    }
    const key = flag.slice(2);
    if (values[key] !== undefined) fail(`duplicate argument: ${flag}`);
    values[key] = value;
  }
  for (const required of [
    "root",
    "app-pid",
    "app-executable",
    "cli",
    "profile",
    "imap-port",
    "imap-proxy-pid",
    "expected-app-sha256",
    "expected-cli-sha256",
    "report",
  ]) {
    if (!values[required]) fail(`missing --${required}`);
  }
  const appPid = Number(values["app-pid"]);
  const imapPort = Number(values["imap-port"]);
  const imapProxyPid = Number(values["imap-proxy-pid"]);
  if (!Number.isSafeInteger(appPid) || appPid < 1) fail("app PID is invalid");
  if (!Number.isSafeInteger(imapProxyPid) || imapProxyPid < 1) {
    fail("IMAP proxy PID is invalid");
  }
  if (!Number.isSafeInteger(imapPort) || imapPort < 1 || imapPort > 65535) {
    fail("IMAP port is invalid");
  }
  for (const name of ["expected-app-sha256", "expected-cli-sha256"]) {
    if (!SHA256.test(values[name])) fail(`${name} is not a lowercase SHA-256`);
  }
  return { ...values, appPid, imapPort, imapProxyPid };
}

async function sha256(path) {
  return createHash("sha256")
    .update(await readFile(path))
    .digest("hex");
}

async function bundleInventory(root) {
  const entries = [];
  async function walk(path) {
    const info = await lstat(path);
    const entry = {
      path: path === root ? "." : relative(root, path),
      mode: info.mode & 0o7777,
      uid: info.uid,
      gid: info.gid,
    };
    if (info.isDirectory()) {
      entry.type = "directory";
      entries.push(entry);
      for (const name of (await readdir(path)).sort())
        await walk(join(path, name));
    } else if (info.isFile()) {
      entry.type = "file";
      entry.bytes = info.size;
      entry.links = info.nlink;
      entry.sha256 = await sha256(path);
      entries.push(entry);
    } else if (info.isSymbolicLink()) {
      entry.type = "symlink";
      entry.target = await readlink(path);
      entries.push(entry);
    } else {
      fail(
        `approved identity contains an unsupported filesystem entry: ${path}`,
      );
    }
  }
  await walk(root);
  return {
    entries,
    digest: createHash("sha256").update(JSON.stringify(entries)).digest("hex"),
  };
}

export function validateApprovedIdentityDocuments({
  record,
  journal,
  recordPath,
  root,
  appBundle,
  appSha256,
  cliSha256,
  candidateInventory,
  backupInventory,
}) {
  const approvedRoot = record.approvedRoot;
  const approvedApp = record.approvedApp;
  const desktopEntry = record.inventory?.entries?.find(
    (entry) => entry.path === "Contents/MacOS/dakia-desktop",
  );
  const cliEntry = record.inventory?.entries?.find(
    (entry) => entry.path === "Contents/MacOS/dakia",
  );
  if (
    record.schemaVersion !== 1 ||
    record.contract !== APPROVED_IDENTITY_CONTRACT ||
    !/^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$/.test(
      record.bundleIdentifier,
    ) ||
    record.displayName !== "Dakia Fictional Provider Reader" ||
    approvedApp !== join(approvedRoot, "Dakia Fictional Provider Reader.app") ||
    recordPath !== join(approvedRoot, "approved-native-identity.json") ||
    desktopEntry?.sha256 !== record.desktopSha256 ||
    cliEntry?.sha256 !== record.cliSha256 ||
    record.inventory?.digest !== backupInventory.digest ||
    JSON.stringify(record.inventory?.entries) !==
      JSON.stringify(backupInventory.entries)
  ) {
    fail(
      "approved identity record does not match the retained sealed original",
    );
  }
  if (
    journal.schemaVersion !== 1 ||
    journal.contract !== APPROVED_IDENTITY_CONTRACT ||
    journal.stage !== "candidate-installed" ||
    journal.approvedRoot !== approvedRoot ||
    journal.approvedApp !== approvedApp ||
    journal.runRoot !== root ||
    journal.candidateApp !==
      join(root, "Dakia Fictional Provider Reader.app") ||
    journal.backupApp !== join(root, "sealed-approved-original.app") ||
    journal.testedCandidateApp !==
      join(root, "tested-approved-candidate.app") ||
    journal.originalInventoryDigest !== backupInventory.digest ||
    journal.candidateInventoryDigest !== candidateInventory.digest ||
    journal.candidateDesktopSha256 !== appSha256 ||
    journal.candidateCliSha256 !== cliSha256 ||
    appBundle !== approvedApp
  ) {
    fail("active approved identity transaction does not bind the live wrapper");
  }
}

function quoteIdentifier(value) {
  return `"${value.replaceAll('"', '""')}"`;
}

function quoteLiteral(value) {
  return `'${value.replaceAll("'", "''")}'`;
}

async function execute(path, args, options = {}) {
  return execFileAsync(path, args, {
    encoding: "utf8",
    maxBuffer: 4 * 1024 * 1024,
    timeout: 10_000,
    ...options,
  });
}

async function sqliteJson(database, sql) {
  const { stdout, stderr } = await execute("sqlite3", [
    "-readonly",
    "-json",
    database,
    sql,
  ]);
  if (stderr !== "") fail(`sqlite3 wrote unexpected stderr: ${stderr.trim()}`);
  const trimmed = stdout.trim();
  return trimmed === "" ? [] : JSON.parse(trimmed);
}

async function tableSnapshot(database, table) {
  const existing = await sqliteJson(
    database,
    `SELECT name FROM sqlite_master WHERE type='table' AND name=${quoteLiteral(table)}`,
  );
  if (existing.length === 0) return { present: false, columns: [], rows: [] };
  const columns = await sqliteJson(
    database,
    `PRAGMA table_info(${quoteIdentifier(table)})`,
  );
  const names = columns.map(({ name }) => name);
  if (names.length === 0) fail(`table ${table} has no columns`);
  const pairs = names.flatMap((name) => {
    const identifier = quoteIdentifier(name);
    return [
      quoteLiteral(name),
      `CASE WHEN typeof(${identifier})='blob' THEN 'hex:' || hex(${identifier}) ELSE ${identifier} END`,
    ];
  });
  const encoded = await sqliteJson(
    database,
    `SELECT json_object(${pairs.join(",")}) AS row_json FROM ${quoteIdentifier(table)}`,
  );
  const rows = encoded
    .map(({ row_json }) => JSON.parse(row_json))
    .sort((left, right) =>
      JSON.stringify(left).localeCompare(JSON.stringify(right)),
    );
  return { present: true, columns: names, rows };
}

async function logicalSnapshot(database) {
  const tables = {};
  for (const table of SNAPSHOT_TABLES)
    tables[table] = await tableSnapshot(database, table);
  const canonical = JSON.stringify({ tables });
  return {
    digest: createHash("sha256").update(canonical).digest("hex"),
    tableCounts: Object.fromEntries(
      Object.entries(tables).map(([table, value]) => [
        table,
        value.rows.length,
      ]),
    ),
    canonical,
    tables,
  };
}

async function databaseHealth(database) {
  const integrity = await sqliteJson(database, "PRAGMA integrity_check");
  const foreignKeys = await sqliteJson(database, "PRAGMA foreign_key_check");
  if (integrity.length !== 1 || integrity[0].integrity_check !== "ok") {
    fail("SQLite integrity check failed");
  }
  if (foreignKeys.length !== 0) fail("SQLite foreign-key check failed");
  return { integrity: "ok", foreignKeyViolations: 0 };
}

async function stableLogicalSnapshot(database) {
  let previous;
  for (let attempt = 1; attempt <= 20; attempt += 1) {
    const current = await logicalSnapshot(database);
    if (previous?.digest === current.digest)
      return { ...current, stabilityAttempts: attempt };
    previous = current;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 250));
  }
  fail(
    "fictional profile did not reach two consecutive stable logical snapshots",
  );
}

export function readOnlyWavePlan(profile) {
  const common = ["--data-dir", profile, "--json"];
  return [
    { kind: "accounts", args: [...common, "account", "list"] },
    { kind: "accounts", args: [...common, "account", "list"] },
    {
      kind: "primary-positive",
      args: [
        ...common,
        "search",
        PRIMARY_UID3_BODY_QUERY,
        "--account",
        PRIMARY_ACCOUNT_ID,
      ],
    },
    {
      kind: "primary-positive",
      args: [
        ...common,
        "search",
        PRIMARY_UID3_BODY_QUERY,
        "--account",
        PRIMARY_ACCOUNT_ID,
      ],
    },
    {
      kind: "secondary-positive",
      args: [
        ...common,
        "search",
        "Account two collision safety check",
        "--account",
        SECONDARY_ACCOUNT_ID,
      ],
    },
    {
      kind: "secondary-positive",
      args: [
        ...common,
        "search",
        "Account two collision safety check",
        "--account",
        SECONDARY_ACCOUNT_ID,
      ],
    },
    {
      kind: "primary-negative",
      args: [
        ...common,
        "search",
        "Account two collision safety check",
        "--account",
        PRIMARY_ACCOUNT_ID,
      ],
    },
    {
      kind: "secondary-negative",
      args: [
        ...common,
        "search",
        PRIMARY_UID3_BODY_QUERY,
        "--account",
        SECONDARY_ACCOUNT_ID,
      ],
    },
  ];
}

export function validateReadOnlyPlan(plan, profile) {
  if (plan.length !== 8)
    fail("shared-profile acceptance must run exactly eight CLI readers");
  for (const { args } of plan) {
    if (
      args[0] !== "--data-dir" ||
      args[1] !== profile ||
      args[2] !== "--json"
    ) {
      fail("every CLI reader must use the exact owned profile");
    }
    const command = args[3];
    const accountList =
      command === "account" && args.length === 5 && args[4] === "list";
    const localSearch =
      command === "search" &&
      args.length === 7 &&
      args[5] === "--account" &&
      EXPECTED_ACCOUNTS.has(args[6]);
    if (!accountList && !localSearch) fail("CLI wave contains a mutation");
    if (args.includes("--remote"))
      fail("CLI wave must not contact the provider");
  }
}

export function validateAccounts(value) {
  if (!Array.isArray(value) || value.length !== EXPECTED_ACCOUNTS.size) {
    fail("bundled CLI did not return exactly two fictional accounts");
  }
  const observed = new Map(value.map((account) => [account.id, account.email]));
  for (const [id, email] of EXPECTED_ACCOUNTS) {
    if (observed.get(id) !== email)
      fail(`bundled CLI account mismatch for ${id}`);
  }
}

export function normalizeReaderBodyText(value) {
  if (typeof value !== "string") fail("cached body text is not a string");
  return value.replaceAll("\r\n", "\n").replaceAll("\r", "\n");
}

export function validatePrimaryCachedBody(snapshot) {
  const messages = snapshot?.tables?.messages?.rows;
  const cached = snapshot?.tables?.message_content_cache?.rows;
  if (!Array.isArray(messages) || !Array.isArray(cached)) {
    fail("logical snapshot does not include message and cache rows");
  }
  const targets = messages.filter(
    (message) =>
      message.id === PRIMARY_UID3_MESSAGE_ID &&
      message.account_id === PRIMARY_ACCOUNT_ID &&
      message.uid === 3 &&
      message.subject === "Remote content safety check",
  );
  if (targets.length !== 1) {
    fail("fictional profile does not contain the exact primary UID 3 message");
  }
  const entries = cached.filter(
    (entry) =>
      entry.message_id === PRIMARY_UID3_MESSAGE_ID &&
      entry.content_state === "complete",
  );
  if (entries.length !== 1) {
    fail("fictional profile does not contain one complete primary cached body");
  }
  const bodyText = normalizeReaderBodyText(entries[0].body_text);
  const utf8Bytes = Buffer.byteLength(bodyText, "utf8");
  const sha256 = createHash("sha256").update(bodyText, "utf8").digest("hex");
  if (
    utf8Bytes !== PRIMARY_UID3_NORMALIZED_BODY_UTF8_BYTES ||
    sha256 !== PRIMARY_UID3_NORMALIZED_BODY_SHA256
  ) {
    fail("primary cached body does not match the faithful Reader fixture");
  }
  return {
    messageId: PRIMARY_UID3_MESSAGE_ID,
    normalization: "CRLF and CR normalized to LF; no whitespace is trimmed",
    utf8Bytes,
    sha256,
  };
}

export function validateSearch(kind, value) {
  if (!Array.isArray(value)) fail(`${kind} search did not return an array`);
  const positive = kind.endsWith("positive");
  if (value.length !== (positive ? 1 : 0))
    fail(`${kind} search returned an unexpected count`);
  if (!positive) return;
  const message = value[0];
  const primary = kind.startsWith("primary");
  const expectedAccount = primary ? PRIMARY_ACCOUNT_ID : SECONDARY_ACCOUNT_ID;
  const expectedSubject = primary
    ? "Remote content safety check"
    : "Account two collision safety check";
  if (
    message.account_id !== expectedAccount ||
    message.subject !== expectedSubject ||
    message.uid !== 3
  ) {
    fail(`${kind} search crossed an account or immutable UID boundary`);
  }
}

async function runCliReader(cli, item) {
  const environment = { ...process.env };
  for (const name of Object.keys(environment)) {
    if (
      name === "DAKIA_DATA_DIR" ||
      name.startsWith("DAKIA_NATIVE_MAIL_SAFETY_") ||
      name.startsWith("DAKIA_RELEASE_SMOKE_")
    ) {
      delete environment[name];
    }
  }
  const { stdout, stderr } = await execute(cli, item.args, {
    env: environment,
  });
  if (stderr !== "") fail(`${item.kind} CLI reader wrote unexpected stderr`);
  const value = JSON.parse(stdout);
  if (item.kind === "accounts") validateAccounts(value);
  else validateSearch(item.kind, value);
  return {
    kind: item.kind,
    args: item.args,
    stdoutSha256: createHash("sha256").update(stdout).digest("hex"),
    resultCount: Array.isArray(value) ? value.length : null,
  };
}

async function processCommand(pid) {
  const { stdout } = await execute("ps", ["-p", String(pid), "-o", "command="]);
  return stdout.trim();
}

async function ownedImapProxy(root, pid, port) {
  const metadata = JSON.parse(
    await readFile(join(root, "imap-proxy.json"), "utf8"),
  );
  if (
    metadata.pid !== pid ||
    metadata.port !== port ||
    !Number.isSafeInteger(metadata.backendPort) ||
    metadata.backendPort < 1 ||
    metadata.backendPort > 65535 ||
    !/^[a-f0-9]{48}$/.test(metadata.controlNonce)
  ) {
    fail(
      "owned IMAP proxy metadata does not match the requested process and port",
    );
  }
  const command = await processCommand(pid);
  if (
    !command.includes("native-fictional-imap-proxy.mjs") ||
    !command.includes(`--root ${root}`) ||
    !command.includes(`--backend-port ${metadata.backendPort}`)
  ) {
    fail("IMAP proxy PID does not run the owned fictional proxy command");
  }
  const { stdout } = await execute("lsof", [
    "-nP",
    "-a",
    "-p",
    String(pid),
    `-iTCP:${port}`,
    "-sTCP:LISTEN",
  ]);
  if (!stdout.includes(`127.0.0.1:${port} (LISTEN)`)) {
    fail(
      "owned fictional IMAP proxy is not listening on its recorded loopback port",
    );
  }
  return { pid, port, backendPort: metadata.backendPort };
}

function isKnownTimeMachineWarning(stderr) {
  return /^lsof: WARNING: can't stat\(\) smbfs file system \/Volumes\/\.timemachine\/[^\n]+\/TimeMachine\n\s+Output information may be incomplete\.\n\s+assuming "dev=[0-9a-f]+" from mount table\n?$/.test(
    stderr,
  );
}

class RetryableActiveImapAbsence extends Error {}

export function parseActiveImapConnectionResult(result, pid, port) {
  const stdout = typeof result?.stdout === "string" ? result.stdout : "";
  const stderr = typeof result?.stderr === "string" ? result.stderr : "";
  if (result?.error || ![0, 1].includes(result?.status)) {
    fail("active IMAP lsof probe is unavailable");
  }
  if (stderr.trim() && !isKnownTimeMachineWarning(stderr)) {
    fail("active IMAP lsof probe returned unknown diagnostics");
  }
  if (result.status === 1 && !isKnownTimeMachineWarning(stderr)) {
    fail("active IMAP lsof exit 1 is not the exact host warning");
  }

  const exactEndpoint = new RegExp(
    `^127\\.0\\.0\\.1:(\\d+)->127\\.0\\.0\\.1:${port}$`,
  );
  let currentPid = null;
  let commandSeen = false;
  let descriptorSeen = false;
  const connections = [];
  for (const line of stdout.split("\n")) {
    if (!line) continue;
    if (line[0] === "p") {
      if (line !== `p${pid}`) fail("active IMAP lsof PID row is malformed");
      currentPid = pid;
      commandSeen = false;
      descriptorSeen = false;
    } else if (line[0] === "c") {
      if (currentPid === null || line.length < 2) {
        fail("active IMAP lsof command row is malformed");
      }
      commandSeen = true;
    } else if (line[0] === "f") {
      if (currentPid === null || !/^f[0-9A-Za-z]+$/.test(line)) {
        fail("active IMAP lsof descriptor row is malformed");
      }
      descriptorSeen = true;
    } else if (line[0] === "n") {
      if (currentPid === null || !commandSeen || !descriptorSeen) {
        fail("active IMAP lsof endpoint row is malformed");
      }
      const match = line.slice(1).match(exactEndpoint);
      if (!match) fail("active IMAP lsof endpoint escaped the owned proxy");
      const sourcePort = Number(match[1]);
      if (
        !Number.isSafeInteger(sourcePort) ||
        sourcePort < 1 ||
        sourcePort > 65535
      ) {
        fail("active IMAP lsof source port is invalid");
      }
      connections.push(line.slice(1));
      descriptorSeen = false;
    } else {
      fail("active IMAP lsof output is malformed");
    }
  }
  if (connections.length === 0) {
    throw new RetryableActiveImapAbsence(
      "active IMAP lsof reported no owned loopback connections",
    );
  }
  return connections;
}

export async function activeImapConnection(
  pid,
  port,
  {
    executeCommand = execute,
    sleep = (milliseconds) =>
      new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds)),
  } = {},
) {
  let lastOutput = "";
  for (let attempt = 1; attempt <= 20; attempt += 1) {
    let result;
    try {
      const { stdout, stderr } = await executeCommand("lsof", [
        "-nP",
        "-a",
        "-p",
        String(pid),
        `-iTCP:${port}`,
        "-sTCP:ESTABLISHED",
        "-Fpcfn",
      ]);
      result = { status: 0, stdout, stderr };
    } catch (error) {
      result = {
        status: error?.code,
        stdout: error?.stdout ?? "",
        stderr: error?.stderr ?? "",
        error: error?.code === undefined,
      };
    }
    try {
      const connections = parseActiveImapConnectionResult(result, pid, port);
      lastOutput = `${result.stdout}${result.stderr}`;
      if (connections.length >= EXPECTED_ACCOUNTS.size) {
        return {
          established: true,
          attempts: attempt,
          exactLoopbackConnections: connections.length,
        };
      }
    } catch (error) {
      lastOutput = `${result.stdout}${result.stderr}`;
      if (!(error instanceof RetryableActiveImapAbsence)) throw error;
    }
    await sleep(250);
  }
  fail(
    `desktop has no established connection to the owned IMAP proxy: ${lastOutput.trim()}`,
  );
}

export function validateRuntimeManifest(
  manifest,
  appPath,
  appSha,
  cliPath,
  cliSha,
) {
  if (manifest.schemaVersion !== 1 || !Array.isArray(manifest.files)) {
    fail("native runtime manifest is invalid");
  }
  const wrapperRoot = resolve(dirname(dirname(dirname(appPath))));
  const entries = new Map(manifest.files.map((entry) => [entry.path, entry]));
  if (entries.size !== manifest.files.length) {
    fail("native runtime manifest contains duplicate paths");
  }
  for (const [path, expectedSha] of [
    [appPath, appSha],
    [cliPath, cliSha],
  ]) {
    const relativePath = relative(wrapperRoot, path);
    const entry = entries.get(relativePath);
    if (
      !entry ||
      entry.sha256 !== expectedSha ||
      !Number.isSafeInteger(entry.bytes)
    ) {
      fail(`native runtime manifest does not own ${relativePath}`);
    }
  }
}

async function atomicEvidence(path, value) {
  const temporary = join(
    dirname(path),
    `.${basename(path)}.${process.pid}.${Date.now()}.tmp`,
  );
  const file = await open(temporary, "wx", 0o600);
  try {
    await file.writeFile(`${JSON.stringify(value, null, 2)}\n`);
    await file.sync();
  } finally {
    await file.close();
  }
  try {
    await link(temporary, path);
    const directory = await open(dirname(path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
  } finally {
    await unlink(temporary).catch(() => {});
  }
}

async function acceptance(options) {
  const root = await realpath(options.root);
  const profile = await realpath(options.profile);
  const appExecutable = await realpath(options["app-executable"]);
  const cli = await realpath(options.cli);
  const report = resolve(options.report);
  if (profile !== join(root, "profile"))
    fail("profile is outside the owned fictional root");
  const wrapper = join(
    root,
    "Dakia Fictional Provider Reader.app",
    "Contents",
    "MacOS",
  );
  const approvedRecordOption = options["approved-identity-record"];
  const approvedIdentityReuse = approvedRecordOption !== undefined;
  let approvedRecordPath;
  let approvedRoot;
  let approvedApp;
  let approvedJournal;
  let record;
  let approvedRecordInfo;
  if (!approvedIdentityReuse) {
    if (
      appExecutable !== join(wrapper, "dakia-desktop") ||
      cli !== join(wrapper, "dakia")
    ) {
      fail("native executables are outside the owned fictional wrapper");
    }
  } else {
    approvedRecordPath = resolve(approvedRecordOption);
    approvedRecordInfo = await lstat(approvedRecordPath);
    const uid = process.getuid?.();
    if (
      !approvedRecordInfo.isFile() ||
      approvedRecordInfo.isSymbolicLink() ||
      (approvedRecordInfo.mode & 0o777) !== 0o600 ||
      (uid !== undefined && approvedRecordInfo.uid !== uid)
    ) {
      fail("approved identity record is not a private owned regular file");
    }
    if ((await realpath(approvedRecordOption)) !== approvedRecordPath) {
      fail("approved identity record path is not canonical");
    }
    record = JSON.parse(await readFile(approvedRecordPath, "utf8"));
    approvedRoot = record.approvedRoot;
    approvedApp = record.approvedApp;
    approvedJournal = join(approvedRoot, ".approved-native-identity-swap.json");
    if (
      approvedRecordPath !==
        join(approvedRoot, "approved-native-identity.json") ||
      approvedApp !==
        join(approvedRoot, "Dakia Fictional Provider Reader.app") ||
      appExecutable !== join(approvedApp, "Contents/MacOS/dakia-desktop") ||
      cli !== join(approvedApp, "Contents/MacOS/dakia")
    ) {
      fail("approved identity reuse does not name its exact sealed wrapper");
    }
  }
  if (dirname(report) !== root)
    fail("evidence report must stay in the owned fictional root");
  if (
    (await readFile(join(root, ".dakia-native-mail-safety-root"), "utf8")) !==
    ROOT_MARKER
  ) {
    fail("fictional root ownership marker is invalid");
  }
  if (
    (await readFile(
      join(profile, ".dakia-native-mail-safety-profile"),
      "utf8",
    )) !== ROOT_MARKER
  ) {
    fail("fictional profile ownership marker is invalid");
  }
  await access(appExecutable, constants.X_OK);
  await access(cli, constants.X_OK);
  const appSha256 = await sha256(appExecutable);
  const cliSha256 = await sha256(cli);
  if (appSha256 !== options["expected-app-sha256"])
    fail("desktop executable SHA-256 changed");
  if (cliSha256 !== options["expected-cli-sha256"])
    fail("bundled CLI SHA-256 changed");
  let approvedIdentity;
  if (approvedIdentityReuse) {
    const [journalInfo, approvedRootInfo] = await Promise.all([
      lstat(approvedJournal),
      lstat(approvedRoot),
    ]);
    const uid = process.getuid?.();
    if (
      !journalInfo.isFile() ||
      journalInfo.isSymbolicLink() ||
      !approvedRootInfo.isDirectory() ||
      approvedRootInfo.isSymbolicLink() ||
      (journalInfo.mode & 0o777) !== 0o600 ||
      (approvedRootInfo.mode & 0o777) !== 0o700 ||
      (uid !== undefined &&
        [approvedRecordInfo.uid, journalInfo.uid, approvedRootInfo.uid].some(
          (owner) => owner !== uid,
        ))
    ) {
      fail("approved identity record, journal, or root ownership is invalid");
    }
    if (
      (await readFile(
        join(approvedRoot, ".dakia-native-mail-safety-root"),
        "utf8",
      )) !== ROOT_MARKER
    ) {
      fail("approved identity root marker is invalid");
    }
    const appBundle = dirname(dirname(dirname(appExecutable)));
    const backupBundle = join(root, "sealed-approved-original.app");
    const [journal, candidateInventory, backupInventory] = await Promise.all([
      readFile(approvedJournal, "utf8").then(JSON.parse),
      bundleInventory(appBundle),
      bundleInventory(backupBundle),
    ]);
    await parseApprovedBundleInfoPlist(
      join(appBundle, "Contents/Info.plist"),
      record.bundleIdentifier,
      "Dakia Fictional Provider Reader",
    );
    validateApprovedIdentityDocuments({
      record,
      journal,
      recordPath: approvedRecordPath,
      root,
      appBundle,
      appSha256,
      cliSha256,
      candidateInventory,
      backupInventory,
    });
    await execute("codesign", ["--verify", "--deep", "--strict", appBundle]);
    approvedIdentity = {
      record: approvedRecordPath,
      journal: approvedJournal,
      bundleIdentifier: record.bundleIdentifier,
      candidateInventoryDigest: candidateInventory.digest,
      sealedOriginalInventoryDigest: backupInventory.digest,
    };
  }
  const manifest = JSON.parse(
    await readFile(join(root, "native-runtime-files.json"), "utf8"),
  );
  validateRuntimeManifest(manifest, appExecutable, appSha256, cli, cliSha256);
  const command = await processCommand(options.appPid);
  if (command !== appExecutable)
    fail("desktop PID does not run the expected executable");
  const { stdout: appFile } = await execute("file", [appExecutable]);
  const { stdout: cliFile } = await execute("file", [cli]);
  if (!appFile.includes("Mach-O 64-bit executable arm64"))
    fail("desktop is not arm64 Mach-O");
  if (!cliFile.includes("Mach-O 64-bit executable arm64"))
    fail("bundled CLI is not arm64 Mach-O");
  const { stdout: cliVersion, stderr: cliVersionError } = await execute(cli, [
    "--version",
  ]);
  if (cliVersionError !== "" || !/^dakia \d+\.\d+\.\d+\n$/.test(cliVersion)) {
    fail("bundled CLI version output is invalid");
  }
  const proxy = await ownedImapProxy(
    root,
    options.imapProxyPid,
    options.imapPort,
  );
  const beforeConnection = await activeImapConnection(
    options.appPid,
    options.imapPort,
  );
  const database = join(profile, "dakia.db");
  const beforeHealth = await databaseHealth(database);
  const before = await stableLogicalSnapshot(database);
  validateAccounts(
    before.tables.accounts.rows.map(({ id, email }) => ({ id, email })),
  );
  const cachedPrimaryBody = validatePrimaryCachedBody(before);
  if (before.tables.messages.rows.length < 6)
    fail("fictional profile is missing provider messages");
  if (
    before.tables.messages.rows.some(
      ({ account_id }) => !EXPECTED_ACCOUNTS.has(account_id),
    )
  ) {
    fail("fictional profile contains an unexpected account message");
  }
  const plan = readOnlyWavePlan(profile);
  validateReadOnlyPlan(plan, profile);
  const wave = await Promise.all(plan.map((item) => runCliReader(cli, item)));
  const afterConnection = await activeImapConnection(
    options.appPid,
    options.imapPort,
  );
  if ((await processCommand(options.appPid)) !== appExecutable) {
    fail("desktop process identity changed during CLI access");
  }
  const afterHealth = await databaseHealth(database);
  const after = await stableLogicalSnapshot(database);
  if (before.digest !== after.digest || before.canonical !== after.canonical) {
    fail("read-only bundled CLI wave changed durable fictional mail state");
  }
  const sourceSnapshot = JSON.parse(
    await readFile(join(root, "source-before-build.json"), "utf8"),
  );
  const evidence = {
    schemaVersion: 1,
    status: "passed",
    requirement: "MS-H-015",
    generatedAt: new Date().toISOString(),
    source: {
      algorithm: sourceSnapshot.algorithm,
      digest: sourceSnapshot.digest,
      fileCount: sourceSnapshot.fileCount,
    },
    runtime: {
      appPid: options.appPid,
      appExecutable,
      appSha256,
      cli,
      cliSha256,
      cliVersion: cliVersion.trim(),
      profile,
      imapPort: options.imapPort,
      architecture: "arm64",
      signatureScope: "ad-hoc fictional acceptance wrapper",
      approvedIdentity,
    },
    desktopRealtime: {
      proxy,
      before: beforeConnection,
      after: afterConnection,
    },
    database: {
      before: {
        ...beforeHealth,
        digest: before.digest,
        tableCounts: before.tableCounts,
      },
      after: {
        ...afterHealth,
        digest: after.digest,
        tableCounts: after.tableCounts,
      },
      unchanged: true,
    },
    cachedReaderBody: cachedPrimaryBody,
    cliReaders: {
      parallelCount: wave.length,
      allCommandsUserReadOnly: true,
      providerContactRequested: false,
      logicalSnapshotTables: SNAPSHOT_TABLES,
      wave,
    },
    limitations: [
      "The wrapper is ad-hoc signed and is not a Developer ID signed, notarized, stapled, or distributed release candidate.",
      "The CLI opens the current Store and may rewrite current-schema coordination markers. The unchanged assertion covers durable account, credential, message, cache, attachment, action, tombstone, rebuild, and correspondence state rather than live database and WAL bytes.",
      "This helper proves local read-only CLI metadata access while the fictional desktop Store and realtime IMAP connections remain active. The separately reported cache hash binds the full UID 3 Reader fixture body, but the CLI does not display that body.",
      "Native webview identity and visible Reader markers must be recorded separately by CUA for the same account, mailbox, UID and subject. The cached source-body hash is not a hash of sanitized visible HTML text.",
    ],
  };
  await atomicEvidence(report, evidence);
  process.stdout.write(
    `Native desktop and bundled CLI shared-profile acceptance passed: ${report}\n`,
  );
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  try {
    await acceptance(options);
  } catch (error) {
    const bounded =
      error instanceof Error
        ? error.message.slice(0, 2_000)
        : String(error).slice(0, 2_000);
    const report = resolve(options.report);
    if (dirname(report) === resolve(options.root)) {
      await atomicEvidence(report, {
        schemaVersion: 1,
        status: "failed",
        requirement: "MS-H-015",
        generatedAt: new Date().toISOString(),
        error: bounded,
      }).catch(() => {});
    }
    throw error;
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  main().catch((error) => {
    process.stderr.write(`${error.stack || error}\n`);
    process.exitCode = 1;
  });
}
