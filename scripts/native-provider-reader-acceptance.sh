#!/usr/bin/env bash
set -euo pipefail

# Runs an isolated, fictional IMAP provider through the production Tauri
# watcher and Reader. It intentionally builds the non-default fixture feature;
# ordinary and release builds cannot enable its loopback CA trust seam.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
approved_identity_root=''
approved_identity_app=''
approved_identity_record=''
approved_identity_journal=''
approved_identity_identifier=''
approved_identity_desktop_sha256=''
approved_identity_cli_sha256=''

approved_identity_state() {
  node --input-type=module - "$@" <<'APPROVED_IDENTITY_NODE'
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import {
  chmod,
  lstat,
  link,
  mkdir,
  open,
  readFile,
  readdir,
  readlink,
  realpath,
  rename,
  rm,
  stat,
  unlink,
} from 'node:fs/promises';
import { basename, dirname, join, relative, resolve, sep } from 'node:path';

const [
  action,
  approvedRoot,
  approvedApp,
  recordPath,
  journalPath,
  runRoot,
  candidateApp,
  expectedIdentifier,
  expectedDesktopSha256,
  expectedCliSha256,
] = process.argv.slice(2);
const marker = 'dakia-native-mail-safety-fixture-v1\n';
const contract = 'dakia-native-provider-reader-approved-identity-v1';
const uid = process.getuid?.();
if (!/^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$/.test(expectedIdentifier)) {
  throw new Error('approved identity does not use a fictional provider Reader identifier');
}

function fail(message) {
  throw new Error(message);
}

function exactChild(parent, child, expectedName) {
  if (resolve(child) !== join(resolve(parent), expectedName)) {
    fail(`${expectedName} is outside its exact owned parent`);
  }
}

async function requireOwnedDirectory(path, mode, description) {
  const info = await lstat(path);
  if (!info.isDirectory() || info.isSymbolicLink()) fail(`${description} is not a real directory`);
  if (uid !== undefined && info.uid !== uid) fail(`${description} is not owned by this user`);
  if ((info.mode & 0o777) !== mode) fail(`${description} mode is not ${mode.toString(8)}`);
  if (await realpath(path) !== resolve(path)) fail(`${description} path is not canonical`);
  return info;
}

async function sha256(path) {
  return createHash('sha256').update(await readFile(path)).digest('hex');
}

async function inventory(root) {
  const entries = [];
  async function walk(path) {
    const info = await lstat(path);
    if (uid !== undefined && info.uid !== uid) fail(`bundle entry is not owned by this user: ${path}`);
    const item = {
      path: path === root ? '.' : relative(root, path),
      mode: info.mode & 0o7777,
      uid: info.uid,
      gid: info.gid,
    };
    if (info.isDirectory()) {
      item.type = 'directory';
      entries.push(item);
      for (const name of (await readdir(path)).sort()) await walk(join(path, name));
    } else if (info.isFile()) {
      item.type = 'file';
      item.bytes = info.size;
      item.links = info.nlink;
      item.sha256 = await sha256(path);
      entries.push(item);
    } else if (info.isSymbolicLink()) {
      item.type = 'symlink';
      item.target = await readlink(path);
      entries.push(item);
    } else {
      fail(`bundle contains unsupported filesystem entry: ${path}`);
    }
  }
  await walk(root);
  const digest = createHash('sha256').update(JSON.stringify(entries)).digest('hex');
  return { digest, entries };
}

async function readBundleMetadata(app) {
  const path = join(app, 'Contents/Info.plist');
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink()) fail('application bundle metadata is not a regular file');
  if (uid !== undefined && info.uid !== uid) fail('application bundle metadata is not owned by this user');
  if (info.size === 0 || info.size > 1024 * 1024) fail('application bundle metadata size is invalid');
  const source = await readFile(path);
  const parser = String.raw`
import json
import plistlib
import sys

class RejectDuplicateDict(dict):
    def __setitem__(self, key, value):
        if key in self:
            raise ValueError("duplicate property-list key")
        super().__setitem__(key, value)

source = sys.stdin.buffer.read(1024 * 1024 + 1)
if not source or len(source) > 1024 * 1024:
    raise ValueError("property-list size is invalid")
value = plistlib.loads(source, dict_type=RejectDuplicateDict)
if type(value) is not RejectDuplicateDict:
    raise ValueError("property-list root is not a dictionary")
result = {}
for key in ("CFBundleIdentifier", "CFBundleName"):
    field = value.get(key)
    if type(field) is not str:
        raise ValueError("required property-list value is not a string")
    result[key] = field
sys.stdout.write(json.dumps(result, separators=(",", ":")))
`;
  try {
    const output = execFileSync('/usr/bin/python3', ['-c', parser], {
      encoding: 'utf8',
      input: source,
      maxBuffer: 4096,
      timeout: 5000,
      windowsHide: true,
    });
    const metadata = JSON.parse(output);
    if (
      !metadata || typeof metadata !== 'object' || Array.isArray(metadata) ||
      Object.keys(metadata).sort().join(',') !== 'CFBundleIdentifier,CFBundleName'
    ) fail('application bundle metadata parser returned an invalid contract');
    return metadata;
  } catch {
    fail('application bundle metadata is not a valid unambiguous property list');
  }
}

async function bundleState(app, identifier, desktopSha256, cliSha256) {
  const info = await lstat(app);
  if (!info.isDirectory() || info.isSymbolicLink()) fail(`application bundle is not a real directory: ${app}`);
  if (uid !== undefined && info.uid !== uid) fail(`application bundle is not owned by this user: ${app}`);
  const metadata = await readBundleMetadata(app);
  if (metadata.CFBundleIdentifier !== identifier) {
    fail('application bundle identifier is not the approved exact identity');
  }
  if (metadata.CFBundleName !== 'Dakia Fictional Provider Reader') {
    fail('application bundle name is not the approved exact display name');
  }
  const desktop = await sha256(join(app, 'Contents/MacOS/dakia-desktop'));
  const cli = await sha256(join(app, 'Contents/MacOS/dakia'));
  if (desktopSha256 && desktop !== desktopSha256) fail('desktop executable hash does not match the sealed identity');
  if (cliSha256 && cli !== cliSha256) fail('bundled CLI hash does not match the sealed identity');
  return { desktopSha256: desktop, cliSha256: cli, inventory: await inventory(app) };
}

async function writeTemporaryJson(path, value) {
  const temporary = `${path}.tmp.${process.pid}.${randomUUID()}`;
  const handle = await open(temporary, 'wx', 0o600);
  try {
    await handle.writeFile(`${JSON.stringify(value, null, 2)}\n`);
    await handle.sync();
  } finally {
    await handle.close();
  }
  await chmod(temporary, 0o600);
  return temporary;
}

async function syncDirectory(path) {
  const directory = await open(path, 'r');
  try { await directory.sync(); } finally { await directory.close(); }
}

async function createJsonExclusive(path, value) {
  const temporary = await writeTemporaryJson(path, value);
  try {
    await link(temporary, path);
    await syncDirectory(dirname(path));
  } finally {
    await unlink(temporary).catch(() => {});
  }
}

async function replaceJson(path, value) {
  const current = JSON.parse(await readFile(path, 'utf8'));
  if (current.transactionId !== value.transactionId) fail('transaction journal identity changed');
  const temporary = await writeTemporaryJson(path, value);
  await rename(temporary, path);
  await syncDirectory(dirname(path));
}

async function validateApprovedRoot() {
  await requireOwnedDirectory(approvedRoot, 0o700, 'approved identity root');
  if (await readFile(join(approvedRoot, '.dakia-native-mail-safety-root'), 'utf8') !== marker) {
    fail('approved identity root marker is invalid');
  }
  exactChild(approvedRoot, approvedApp, 'Dakia Fictional Provider Reader.app');
  exactChild(approvedRoot, recordPath, 'approved-native-identity.json');
  exactChild(approvedRoot, journalPath, '.approved-native-identity-swap.json');
}

async function readVerifiedRecord() {
  await validateApprovedRoot();
  const info = await lstat(recordPath);
  if (!info.isFile() || info.isSymbolicLink() || (info.mode & 0o777) !== 0o600) fail('approved identity record is not a private regular file');
  if (uid !== undefined && info.uid !== uid) fail('approved identity record is not owned by this user');
  const record = JSON.parse(await readFile(recordPath, 'utf8'));
  if (
    record.schemaVersion !== 1 || record.contract !== contract ||
    record.approvedRoot !== approvedRoot || record.approvedApp !== approvedApp ||
    record.bundleIdentifier !== expectedIdentifier ||
    record.desktopSha256 !== expectedDesktopSha256 || record.cliSha256 !== expectedCliSha256
  ) fail('approved identity record contract does not match the exact sealed identity');
  return record;
}

async function verifyOriginal(record) {
  const current = await bundleState(approvedApp, expectedIdentifier, expectedDesktopSha256, expectedCliSha256);
  if (current.inventory.digest !== record.inventory.digest || JSON.stringify(current.inventory.entries) !== JSON.stringify(record.inventory.entries)) {
    fail('approved application bundle differs from its sealed full inventory');
  }
  return current;
}

async function validateRunRoot() {
  await requireOwnedDirectory(runRoot, 0o700, 'transaction run root');
  if (await readFile(join(runRoot, '.dakia-native-mail-safety-root'), 'utf8') !== marker) fail('transaction run root marker is invalid');
  exactChild(runRoot, candidateApp, 'Dakia Fictional Provider Reader.app');
}

async function digestAt(path) {
  try {
    const found = await bundleState(path, expectedIdentifier, '', '');
    return found.inventory.digest;
  } catch (error) {
    if (error?.code === 'ENOENT') return null;
    throw error;
  }
}

async function completeRecovery(journal, result) {
  const backup = journal.backupApp;
  const tested = journal.testedCandidateApp;
  exactChild(runRoot, backup, 'sealed-approved-original.app');
  exactChild(runRoot, tested, 'tested-approved-candidate.app');
  const originalDigest = journal.originalInventoryDigest;
  const candidateDigest = journal.candidateInventoryDigest;
  let historicalDigest = await digestAt(approvedApp);
  let backupDigest = await digestAt(backup);
  const candidateDigestAtPath = await digestAt(candidateApp);
  let testedDigest = await digestAt(tested);
  for (const [name, value, allowed] of [
    ['approved application', historicalDigest, [originalDigest, candidateDigest]],
    ['sealed original backup', backupDigest, [originalDigest]],
    ['candidate application', candidateDigestAtPath, [candidateDigest]],
    ['retained tested candidate', testedDigest, [candidateDigest]],
  ]) {
    if (value !== null && !allowed.includes(value)) fail(`refusing recovery because ${name} has an unrecognized inventory`);
  }
  if (historicalDigest === originalDigest && backupDigest === originalDigest) fail('refusing recovery with duplicate sealed originals');
  if (candidateDigestAtPath === candidateDigest && testedDigest === candidateDigest) fail('refusing recovery with duplicate candidates');

  if (historicalDigest === candidateDigest) {
    if (testedDigest !== null) fail('refusing to overwrite a retained tested candidate');
    if (backupDigest !== originalDigest) fail('sealed original is unavailable during recovery');
    await rename(approvedApp, tested);
    await syncDirectory(approvedRoot);
    await syncDirectory(runRoot);
    testedDigest = candidateDigest;
    historicalDigest = null;
  }

  if (historicalDigest === null && backupDigest === originalDigest) {
    await rename(backup, approvedApp);
    await syncDirectory(approvedRoot);
    await syncDirectory(runRoot);
    historicalDigest = originalDigest;
    backupDigest = null;
  }
  if (historicalDigest !== originalDigest || backupDigest !== null) fail('approved identity recovery cannot restore exactly one sealed original');

  const record = await readVerifiedRecord();
  await verifyOriginal(record);
  const completed = { ...journal, stage: result, completedAt: new Date().toISOString() };
  await replaceJson(journalPath, completed);
  const receipt = join(runRoot, `approved-identity-swap-${journal.transactionId}-${result}.json`);
  await rename(journalPath, receipt);
  await syncDirectory(approvedRoot);
  await syncDirectory(runRoot);
  return receipt;
}

if (action === 'bootstrap') {
  await validateApprovedRoot();
  try { await lstat(journalPath); fail('an unresolved approved identity transaction exists'); } catch (error) { if (error?.code !== 'ENOENT') throw error; }
  const original = await bundleState(approvedApp, expectedIdentifier, expectedDesktopSha256, expectedCliSha256);
  const record = {
    schemaVersion: 1,
    contract,
    sealedAt: new Date().toISOString(),
    approvedRoot,
    approvedApp,
    displayName: 'Dakia Fictional Provider Reader',
    bundleIdentifier: expectedIdentifier,
    desktopSha256: original.desktopSha256,
    cliSha256: original.cliSha256,
    inventory: original.inventory,
  };
  await createJsonExclusive(recordPath, record);
  console.log(recordPath);
} else if (action === 'verify') {
  await verifyOriginal(await readVerifiedRecord());
  try { await lstat(journalPath); fail('an unresolved approved identity transaction exists'); } catch (error) { if (error?.code !== 'ENOENT') throw error; }
} else if (action === 'begin') {
  const record = await readVerifiedRecord();
  try { await lstat(journalPath); fail('an unresolved approved identity transaction exists'); } catch (error) { if (error?.code !== 'ENOENT') throw error; }
  await verifyOriginal(record);
  await validateRunRoot();
  const originalDevice = (await stat(approvedRoot)).dev;
  if ((await stat(runRoot)).dev !== originalDevice) fail('candidate and approved application are not on the same filesystem');
  const candidate = await bundleState(candidateApp, expectedIdentifier, '', '');
  if (candidate.inventory.digest === record.inventory.digest) fail('candidate is byte-identical to the sealed original');
  const journal = {
    schemaVersion: 1,
    contract,
    transactionId: randomUUID(),
    stage: 'prepared',
    createdAt: new Date().toISOString(),
    approvedRoot,
    approvedApp,
    runRoot,
    candidateApp,
    backupApp: join(runRoot, 'sealed-approved-original.app'),
    testedCandidateApp: join(runRoot, 'tested-approved-candidate.app'),
    originalInventoryDigest: record.inventory.digest,
    candidateInventoryDigest: candidate.inventory.digest,
    candidateDesktopSha256: candidate.desktopSha256,
    candidateCliSha256: candidate.cliSha256,
  };
  for (const path of [journal.backupApp, journal.testedCandidateApp]) {
    try { await lstat(path); fail(`transaction destination already exists: ${path}`); } catch (error) { if (error?.code !== 'ENOENT') throw error; }
  }
  await createJsonExclusive(journalPath, journal);
  try {
    await rename(approvedApp, journal.backupApp);
    await syncDirectory(approvedRoot);
    await syncDirectory(runRoot);
    journal.stage = 'original-retained';
    await replaceJson(journalPath, journal);
    await rename(candidateApp, approvedApp);
    await syncDirectory(approvedRoot);
    await syncDirectory(runRoot);
    journal.stage = 'candidate-installed';
    await replaceJson(journalPath, journal);
  } catch (error) {
    await completeRecovery(journal, 'begin-rolled-back').catch((recoveryError) => {
      error.message += `; automatic rollback failed: ${recoveryError.message}`;
    });
    throw error;
  }
  console.log(JSON.stringify({ transactionId: journal.transactionId, desktopSha256: candidate.desktopSha256, cliSha256: candidate.cliSha256 }));
} else if (action === 'restore' || action === 'recover') {
  await validateApprovedRoot();
  const journal = JSON.parse(await readFile(journalPath, 'utf8'));
  if (
    journal.schemaVersion !== 1 || journal.contract !== contract ||
    journal.approvedRoot !== approvedRoot || journal.approvedApp !== approvedApp ||
    journal.runRoot !== runRoot || journal.candidateApp !== candidateApp
  ) fail('transaction journal does not match the exact approved identity paths');
  await validateRunRoot();
  console.log(await completeRecovery(journal, action === 'restore' ? 'restored' : 'recovered'));
} else {
  fail(`unsupported approved identity action: ${action}`);
}
APPROVED_IDENTITY_NODE
}

verify_approved_bundle_signature() {
  local bundle="$1"
  codesign --verify --deep --strict --verbose=2 "$bundle"
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$bundle/Contents/Info.plist")" == "$approved_identity_identifier" ]]
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleName' "$bundle/Contents/Info.plist")" == 'Dakia Fictional Provider Reader' ]]
}

notification_preflight_warning_is_exact() {
  local app_log="$1"
  local expected_identifier="$2"
  local launch_services_warning="LaunchServices did not resolve application $expected_identifier"
  [[ -f "$app_log" ]] \
    && [[ "$expected_identifier" =~ ^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$ ]] \
    && rg -Fq 'desktop notifications are unavailable because the application identity could not be verified' "$app_log" \
    && {
      local line
      while IFS= read -r line || [[ -n "$line" ]]; do
        case "$line" in
          *"$launch_services_warning" | *"$launch_services_warning"[^[:alnum:]._-]*) return 0 ;;
        esac
      done < "$app_log"
      return 1
    }
}

register_approved_identity_with_launch_services() {
  node --input-type=module - "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" "$approved_identity_identifier" "$app_sha256" "$cli_sha256" "$run_root" <<'NODE'
import { createHash } from 'node:crypto';
import { execFile as execFileCallback } from 'node:child_process';
import { lstat, mkdir, open, readFile, realpath } from 'node:fs/promises';
import { join } from 'node:path';
import { promisify } from 'node:util';

const execFile = promisify(execFileCallback);
const LS_REGISTER = '/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister';
const SWIFT = '/usr/bin/swift';
const SWIFT_LOOKUP = String.raw`import CoreServices
import Foundation

let identifier = CommandLine.arguments[1] as CFString
var error: Unmanaged<CFError>?
let applications = LSCopyApplicationURLsForBundleIdentifier(identifier, &error)?.takeRetainedValue()
var urls = [String]()
if let applications {
  let values = applications as NSArray
  for value in values {
    guard let application = value as? URL else { exit(2) }
    urls.append(application.resolvingSymlinksInPath().standardizedFileURL.path)
  }
}
let resolvedError = error?.takeRetainedValue()
let nsError = resolvedError.map { errorValue in errorValue as NSError }
let output: [String: Any] = [
  "urls": urls,
  "errorDomain": nsError?.domain ?? NSNull(),
  "errorCode": nsError.map { errorValue in errorValue.code } ?? NSNull(),
]
let data = try! JSONSerialization.data(withJSONObject: output, options: [])
FileHandle.standardOutput.write(data)`;
const [approvedRoot, approvedApp, recordPath, journalPath, identifier, desktopSha256, cliSha256, runRoot] = process.argv.slice(2);
const receiptPath = join(runRoot, 'launch-services-registration.json');
const unavailableReceiptPath = join(runRoot, 'launch-services-registration-unavailable.json');
const stagePath = join(runRoot, 'launch-services-registration-stage.json');
const sourceEvidencePath = join(runRoot, 'source-before-launch-services-registration.json');
const moduleCachePath = join(runRoot, 'swift-module-cache');
const TEXTEDIT_APPLICATION = '/System/Applications/TextEdit.app';
const marker = 'dakia-native-mail-safety-fixture-v1\n';
const contract = 'dakia-native-provider-reader-approved-identity-v1';
const sha256 = async (path) => createHash('sha256').update(await readFile(path)).digest('hex');
const fail = () => { throw new Error('invalid'); };
let stage = 'preconditions';

async function ownedRegular(path, mode) {
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || (info.mode & 0o777) !== mode || info.uid !== process.getuid() || await realpath(path) !== path) fail();
}
async function ownedDirectory(path, mode) {
  const info = await lstat(path);
  if (!info.isDirectory() || info.isSymbolicLink() || (info.mode & 0o777) !== mode || info.uid !== process.getuid() || await realpath(path) !== path) fail();
}
async function json(path) { return JSON.parse(await readFile(path, 'utf8')); }
async function writePrivateJson(path, value) {
  const handle = await open(path, 'wx', 0o600);
  try {
    await handle.writeFile(`${JSON.stringify(value, null, 2)}\n`);
    await handle.sync();
  } finally {
    await handle.close();
  }
  await ownedRegular(path, 0o600);
}
function lookupResult(stdout) {
  const value = JSON.parse(stdout);
  if (!value || typeof value !== 'object' || Array.isArray(value) || Object.keys(value).sort().join(',') !== 'errorCode,errorDomain,urls' || !Array.isArray(value.urls) || value.urls.some((url) => typeof url !== 'string') || (value.errorDomain !== null && typeof value.errorDomain !== 'string') || (value.errorCode !== null && !Number.isInteger(value.errorCode))) fail();
  return value;
}
async function lookup(value) {
  const { stdout } = await execFile(SWIFT, ['-module-cache-path', moduleCachePath, '-e', SWIFT_LOOKUP, value], { timeout: 15_000, maxBuffer: 4_096, windowsHide: true });
  return lookupResult(stdout);
}
async function revalidateSealedInputs(recordSha256, journalSha256, sourceEvidenceSha256, desktopPath, cliPath) {
  await ownedRegular(recordPath, 0o600);
  await ownedRegular(journalPath, 0o600);
  await ownedRegular(sourceEvidencePath, 0o600);
  await ownedRegular(desktopPath, 0o755);
  await ownedRegular(cliPath, 0o755);
  await ownedDirectory(moduleCachePath, 0o700);
  if (await sha256(recordPath) !== recordSha256 || await sha256(journalPath) !== journalSha256 || await sha256(sourceEvidencePath) !== sourceEvidenceSha256 || await sha256(desktopPath) !== desktopSha256 || await sha256(cliPath) !== cliSha256) fail();
}

try {
  if (!/^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$/.test(identifier)) fail();
  await ownedDirectory(approvedRoot, 0o700);
  await ownedDirectory(runRoot, 0o700);
  if (approvedApp !== join(approvedRoot, 'Dakia Fictional Provider Reader.app') || recordPath !== join(approvedRoot, 'approved-native-identity.json') || journalPath !== join(approvedRoot, '.approved-native-identity-swap.json')) fail();
  if (await readFile(join(approvedRoot, '.dakia-native-mail-safety-root'), 'utf8') !== marker || await readFile(join(runRoot, '.dakia-native-mail-safety-root'), 'utf8') !== marker) fail();
  await ownedRegular(recordPath, 0o600);
  await ownedRegular(journalPath, 0o600);
  await ownedRegular(sourceEvidencePath, 0o600);
  const [record, journal, source] = await Promise.all([json(recordPath), json(journalPath), json(sourceEvidencePath)]);
  if (record.schemaVersion !== 1 || record.contract !== contract || record.approvedRoot !== approvedRoot || record.approvedApp !== approvedApp || record.bundleIdentifier !== identifier || !/^[a-f0-9]{64}$/.test(record.desktopSha256) || !/^[a-f0-9]{64}$/.test(record.cliSha256)) fail();
  if (journal.schemaVersion !== 1 || journal.contract !== contract || journal.stage !== 'candidate-installed' || journal.approvedRoot !== approvedRoot || journal.approvedApp !== approvedApp || journal.runRoot !== runRoot || journal.candidateDesktopSha256 !== desktopSha256 || journal.candidateCliSha256 !== cliSha256 || !/^[a-f0-9]{64}$/.test(journal.candidateInventoryDigest)) fail();
  if (source.status !== 'passed' || source.baseline?.digest !== source.current?.digest || source.baseline?.fileCount !== source.current?.fileCount) fail();
  const appInfo = await lstat(approvedApp);
  if (!appInfo.isDirectory() || appInfo.isSymbolicLink() || appInfo.uid !== process.getuid() || await realpath(approvedApp) !== approvedApp) fail();
  const desktopPath = join(approvedApp, 'Contents/MacOS/dakia-desktop');
  const cliPath = join(approvedApp, 'Contents/MacOS/dakia');
  await ownedRegular(desktopPath, 0o755);
  await ownedRegular(cliPath, 0o755);
  if (await sha256(desktopPath) !== desktopSha256 || await sha256(cliPath) !== cliSha256) fail();
  const [recordSha256, journalSha256, sourceEvidenceSha256] = await Promise.all([sha256(recordPath), sha256(journalPath), sha256(sourceEvidencePath)]);
  await mkdir(moduleCachePath, { mode: 0o700 });
  await ownedDirectory(moduleCachePath, 0o700);
  stage = 'registrar';
  await execFile(LS_REGISTER, ['-f', approvedApp], { timeout: 5_000, maxBuffer: 4_096, windowsHide: true });
  stage = 'candidate-lookup';
  const candidateLookup = await lookup(identifier);
  const registered = candidateLookup.urls.length === 1 && candidateLookup.urls[0] === approvedApp && candidateLookup.errorDomain === null && candidateLookup.errorCode === null;
  const unavailable = candidateLookup.urls.length === 0 && candidateLookup.errorDomain === 'NSOSStatusErrorDomain' && candidateLookup.errorCode === -10814;
  let controlLookup = null;
  if (!registered && !unavailable) fail();
  if (unavailable) {
    stage = 'unavailable-control';
    const controlInfo = await lstat(TEXTEDIT_APPLICATION);
    if (!controlInfo.isDirectory() || controlInfo.isSymbolicLink() || await realpath(TEXTEDIT_APPLICATION) !== TEXTEDIT_APPLICATION) fail();
    controlLookup = await lookup('com.apple.TextEdit');
    if (controlLookup.urls.length !== 1 || controlLookup.urls[0] !== TEXTEDIT_APPLICATION || controlLookup.errorDomain !== null || controlLookup.errorCode !== null) fail();
  }
  stage = 'post-lookup-validation';
  await revalidateSealedInputs(recordSha256, journalSha256, sourceEvidenceSha256, desktopPath, cliPath);
  const common = {
    schemaVersion: 1,
    approvedRoot,
    approvedApp,
    recordPath,
    recordSha256,
    journalPath,
    journalSha256,
    bundleIdentifier: identifier,
    desktopSha256,
    cliSha256,
    sourceEvidencePath,
    sourceEvidenceSha256,
    swiftModuleCachePath: moduleCachePath,
    sourceDigest: source.current.digest,
    sourceFileCount: source.current.fileCount,
    registrationCommand: LS_REGISTER,
    lookup: 'LSCopyApplicationURLsForBundleIdentifier',
  };
  if (registered) {
    stage = 'registered-receipt';
    await writePrivateJson(receiptPath, {
      ...common,
      purpose: 'dakia-native-launch-services-registration-v1',
      resolvedApplication: approvedApp,
    });
    process.stdout.write(`${receiptPath}\n`);
  } else {
    stage = 'unavailable-receipt';
    await writePrivateJson(unavailableReceiptPath, {
      ...common,
      purpose: 'dakia-native-launch-services-registration-unavailable-v1',
      status: 'unavailable',
      candidateLookup,
      controlLookup,
      notificationDelivery: 'not-tested',
    });
    process.stdout.write(`${unavailableReceiptPath}\n`);
  }
} catch {
  try {
    await writePrivateJson(stagePath, {
      schemaVersion: 1,
      purpose: 'dakia-native-launch-services-registration-stage-v1',
      status: 'failed',
      stage,
    });
  } catch {}
  process.stderr.write('LaunchServices registration could not be verified for the sealed fictional app.\n');
  process.exitCode = 1;
}
NODE
}

approved_identity_process_state() {
  node --input-type=module - "$approved_identity_app/Contents/MacOS/dakia-desktop" <<'NODE'
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
const expected = process.argv[2];
try {
  const { stdout } = await promisify(execFile)('/bin/ps', ['-axo', 'command=']);
  const commands = stdout.split('\n').map((line) => line.trim());
  process.stdout.write(commands.some((command) => command === expected || command.startsWith(`${expected} `)) ? 'running' : 'absent');
} catch (error) {
  console.error(`could not enumerate approved-identity processes: ${error.message}`);
  process.stdout.write('probe-error');
}
NODE
}

load_approved_identity_from_root() {
  local requested_root="$1"
  approved_identity_root="$(cd -P "$requested_root" && pwd -P)"
  approved_identity_app="$approved_identity_root/Dakia Fictional Provider Reader.app"
  approved_identity_record="$approved_identity_root/approved-native-identity.json"
  approved_identity_journal="$approved_identity_root/.approved-native-identity-swap.json"
  approved_identity_identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$approved_identity_app/Contents/Info.plist")"
  [[ "$approved_identity_identifier" =~ ^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$ ]] || {
    printf 'The source wrapper does not use a fictional provider Reader identifier.\n' >&2
    exit 64
  }
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleName' "$approved_identity_app/Contents/Info.plist")" == 'Dakia Fictional Provider Reader' ]] || {
    printf 'The source wrapper does not use the exact fictional display name.\n' >&2
    exit 64
  }
  approved_identity_desktop_sha256="$(shasum -a 256 "$approved_identity_app/Contents/MacOS/dakia-desktop" | awk '{print $1}')"
  approved_identity_cli_sha256="$(shasum -a 256 "$approved_identity_app/Contents/MacOS/dakia" | awk '{print $1}')"
}

load_approved_identity_record() {
  local requested_record="$1"
  local canonical_parent
  canonical_parent="$(cd -P "$(dirname "$requested_record")" && pwd -P)"
  approved_identity_record="$canonical_parent/$(basename "$requested_record")"
  [[ "$approved_identity_record" == "$canonical_parent/approved-native-identity.json" ]] || {
    printf 'The approved identity record must use its exact governed filename.\n' >&2
    exit 64
  }
  local metadata
  metadata="$(node --input-type=module - "$approved_identity_record" <<'NODE'
import { lstat, readFile, realpath } from 'node:fs/promises';
import { join } from 'node:path';
const path = process.argv[2];
const info = await lstat(path);
if (!info.isFile() || info.isSymbolicLink() || (info.mode & 0o777) !== 0o600 || (process.getuid?.() !== undefined && info.uid !== process.getuid())) {
  throw new Error('approved identity record is not a private owned regular file');
}
if (await realpath(path) !== path) throw new Error('approved identity record path is not canonical');
const record = JSON.parse(await readFile(path, 'utf8'));
const values = [record.approvedRoot, record.approvedApp, record.bundleIdentifier, record.desktopSha256, record.cliSha256];
if (record.schemaVersion !== 1 || record.contract !== 'dakia-native-provider-reader-approved-identity-v1') throw new Error('approved identity record contract is invalid');
if (values.some((value) => typeof value !== 'string' || /[\t\r\n]/.test(value))) throw new Error('approved identity metadata is invalid');
if (record.approvedApp !== join(record.approvedRoot, 'Dakia Fictional Provider Reader.app')) throw new Error('approved application path is outside its sealed root');
if (!/^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$/.test(record.bundleIdentifier)) throw new Error('approved identity is not a fictional provider Reader identifier');
if (![record.desktopSha256, record.cliSha256].every((value) => /^[a-f0-9]{64}$/.test(value))) throw new Error('approved executable hash is invalid');
process.stdout.write(values.join('\t'));
NODE
)"
  IFS=$'\t' read -r approved_identity_root approved_identity_app approved_identity_identifier approved_identity_desktop_sha256 approved_identity_cli_sha256 <<< "$metadata"
  approved_identity_journal="$approved_identity_root/.approved-native-identity-swap.json"
  [[ "$approved_identity_record" == "$approved_identity_root/approved-native-identity.json" ]] || {
    printf 'The approved identity record is outside its sealed fictional root.\n' >&2
    exit 64
  }
}

mode='run'
prepare_only=false
packaged=false
hold_approved_candidate_after_exit=false
attachment_interruption=false
reuse_approved_identity_record=''
bootstrap_from_run_root=''
recover_approved_identity_record=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prepare-only)
      prepare_only=true
      shift
      ;;
    --packaged)
      packaged=true
      shift
      ;;
    --hold-approved-candidate-after-exit)
      hold_approved_candidate_after_exit=true
      shift
      ;;
    --attachment-interruption)
      attachment_interruption=true
      shift
      ;;
    --reuse-approved-identity)
      [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || { printf '%s requires a sealed record path.\n' "$1" >&2; exit 64; }
      reuse_approved_identity_record="$2"
      shift 2
      ;;
    --bootstrap-approved-identity-record)
      [[ "$mode" == 'run' ]] || { printf 'Identity maintenance modes are mutually exclusive.\n' >&2; exit 64; }
      mode='bootstrap'
      shift
      ;;
    --from-run-root)
      [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || { printf '%s requires an owned fictional fixture root.\n' "$1" >&2; exit 64; }
      bootstrap_from_run_root="$2"
      shift 2
      ;;
    --recover-approved-identity)
      [[ "$mode" == 'run' ]] || { printf 'Identity maintenance modes are mutually exclusive.\n' >&2; exit 64; }
      mode='recover'
      [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || { printf '%s requires a sealed record path.\n' "$1" >&2; exit 64; }
      recover_approved_identity_record="$2"
      shift 2
      ;;
    *)
      printf 'Usage: %s [--prepare-only] [--attachment-interruption] [--packaged --reuse-approved-identity RECORD [--hold-approved-candidate-after-exit]] | --bootstrap-approved-identity-record --from-run-root ROOT | --recover-approved-identity RECORD\n' "$0" >&2
      exit 64
      ;;
  esac
done
if [[ "$mode" != 'run' && ( "$prepare_only" == true || "$packaged" == true || "$hold_approved_candidate_after_exit" == true || "$attachment_interruption" == true || -n "$reuse_approved_identity_record" ) ]]; then
  printf 'Identity maintenance modes cannot be combined with run options.\n' >&2
  exit 64
fi
if [[ "$mode" == 'bootstrap' && -z "$bootstrap_from_run_root" ]]; then
  printf 'Bootstrap requires --from-run-root with an existing owned fictional fixture.\n' >&2
  exit 64
fi
if [[ "$mode" != 'bootstrap' && -n "$bootstrap_from_run_root" ]]; then
  printf '%s is valid only with --bootstrap-approved-identity-record.\n' '--from-run-root' >&2
  exit 64
fi
if [[ "$packaged" == true && ( "$prepare_only" == true || -z "$reuse_approved_identity_record" ) ]]; then
  printf '%s requires an approved identity record and cannot be combined with --prepare-only.\n' '--packaged' >&2
  exit 64
fi
if [[ "$hold_approved_candidate_after_exit" == true && "$packaged" != true ]]; then
  printf '%s is valid only with --packaged and an approved identity record.\n' '--hold-approved-candidate-after-exit' >&2
  exit 64
fi

reuse_approved_identity=false
if [[ -n "$reuse_approved_identity_record" ]]; then
  load_approved_identity_record "$reuse_approved_identity_record"
  reuse_approved_identity=true
fi

if [[ "$mode" == 'bootstrap' || "$mode" == 'recover' ]]; then
  for command in codesign node ps; do
    command -v "$command" >/dev/null 2>&1 || { printf 'Required command is unavailable: %s\n' "$command" >&2; exit 69; }
  done
  if [[ "$mode" == 'bootstrap' ]]; then
    load_approved_identity_from_root "$bootstrap_from_run_root"
  else
    load_approved_identity_record "$recover_approved_identity_record"
  fi
  approved_identity_process_probe="$(approved_identity_process_state)"
  case "$approved_identity_process_probe" in
    absent) ;;
    running)
      printf 'The exact approved fixture executable is still running. Refusing identity maintenance.\n' >&2
      exit 69
      ;;
    *)
      printf 'Could not prove the approved fixture executable is absent. Refusing identity maintenance.\n' >&2
      exit 69
      ;;
  esac
  if [[ "$mode" == 'bootstrap' ]]; then
    verify_approved_bundle_signature "$approved_identity_app"
    approved_identity_state bootstrap "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" '' '' "$approved_identity_identifier" "$approved_identity_desktop_sha256" "$approved_identity_cli_sha256"
    printf 'Sealed approved native identity record: %s\n' "$approved_identity_record"
  else
    run_root="$(node --input-type=module - "$approved_identity_journal" <<'NODE'
import { readFile } from 'node:fs/promises';
const journal = JSON.parse(await readFile(process.argv[2], 'utf8'));
if (journal.schemaVersion !== 1 || typeof journal.runRoot !== 'string') process.exit(1);
process.stdout.write(journal.runRoot);
NODE
)"
    candidate_app="$run_root/Dakia Fictional Provider Reader.app"
    approved_identity_state recover "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" "$run_root" "$candidate_app" "$approved_identity_identifier" "$approved_identity_desktop_sha256" "$approved_identity_cli_sha256"
    verify_approved_bundle_signature "$approved_identity_app"
    printf 'Recovered the exact approved native identity. Review the retained transaction receipt under %s.\n' "$run_root"
  fi
  exit 0
fi

if [[ "$reuse_approved_identity" == true ]]; then
  macos_major="$(sw_vers -productVersion | cut -d. -f1)"
  [[ "$macos_major" =~ ^[0-9]+$ && "$macos_major" -ge 14 ]] || {
    printf 'Approved identity reuse requires macOS 14 or newer for an isolated WKWebView data store.\n' >&2
    exit 69
  }
  [[ "${DAKIA_KEEP_NATIVE_PROVIDER_READER_ARTIFACTS:-}" == '1' ]] || {
    printf 'Approved identity reuse requires DAKIA_KEEP_NATIVE_PROVIDER_READER_ARTIFACTS=1 so transaction evidence is retained.\n' >&2
    exit 64
  }
  verify_approved_bundle_signature "$approved_identity_app"
  approved_identity_state verify "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" '' '' "$approved_identity_identifier" "$approved_identity_desktop_sha256" "$approved_identity_cli_sha256"
fi

run_root="$(mktemp -d "${TMPDIR:-/tmp}/dakia-native-provider-reader.XXXXXX")"
# Tauri rejects a macOS executable path that contains `/var` -> `/private/var`
# symlinks while resolving bundled resources. Keep every wrapper and fixture
# path canonical before the app starts so `resource_dir` selects Contents/Resources.
run_root="$(cd -P "$run_root" && pwd -P)"
# macOS keys persistent WebKit state by application identity. A fresh profile
# directory alone must not reuse cookies, storage, or resource caches from a
# preceding fictional run.
wrapper_suffix="${run_root##*.}"
[[ "$wrapper_suffix" =~ ^[A-Za-z0-9]+$ ]] || exit 64
wrapper_identifier="dev.dakia.fictional-provider-reader.run-${wrapper_suffix}"
native_data_store_identifier=''
if [[ "$reuse_approved_identity" == true ]]; then
  wrapper_identifier="$approved_identity_identifier"
  native_data_store_identifier="$(node --input-type=module - <<'NODE'
import { randomUUID } from 'node:crypto';
process.stdout.write(randomUUID());
NODE
)"
fi
fixture_http_port=48731
fixture_tls_probe_port=48732
container="dakia-native-provider-reader-${RANDOM}-${RANDOM}"
container_id=""
cleanup_started=false
app_pid=""
vite_pid=""
fixture_pid=""
proxy_pid=""
attachment_interruption_pid=""
approved_identity_swapped=false
approved_identity_transaction_started=false
approved_identity_process_probe_failed=false
approved_identity_post_exit_hold_active=false
launch_services_notification_unavailable=false
retain_run_root=false

verify_source_snapshot() {
  node --input-type=module - "$repo_root" "$run_root" "$1" <<'NODE'
import { readFile, writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
process.chdir(process.argv[2]);
const { sourceSnapshot } = await import(pathToFileURL(`${process.argv[2]}/scripts/run-mail-safety.mjs`));
const baseline = JSON.parse(await readFile(`${process.argv[3]}/source-before-build.json`, 'utf8'));
const current = sourceSnapshot();
const passed = Boolean(current && baseline.digest === current.digest && baseline.fileCount === current.fileCount);
await writeFile(`${process.argv[3]}/source-${process.argv[4]}.json`, `${JSON.stringify({ status: passed ? 'passed' : 'failed', baseline, current }, null, 2)}\n`, { mode: 0o600 });
if (!passed) { console.error('Native fixture sources changed; this run cannot be accepted.'); process.exitCode = 1; }
NODE
}

stop_owned_child() {
  local owned_pid="$1"
  [[ -n "$owned_pid" ]] || return 0
  # Only signal jobs this shell still owns as running children. Reap them before
  # removing fixture files so final counter/evidence writes can complete.
  if jobs -pr | rg -qx "$owned_pid"; then
    kill -TERM "$owned_pid" >/dev/null 2>&1 || true
  fi
  wait "$owned_pid" >/dev/null 2>&1 || true
}

post_exit_release_is_valid() {
  local release_path="$1"
  node --input-type=module - "$release_path" "$run_root" <<'NODE'
import { lstat, readFile, realpath } from 'node:fs/promises';
import { join } from 'node:path';
const [path, root] = process.argv.slice(2);
if (path !== join(root, '.native-provider-reader-post-exit-complete')) throw new Error('post-exit release path is outside the exact run root');
const info = await lstat(path);
if (!info.isFile() || info.isSymbolicLink() || (info.mode & 0o777) !== 0o600 || (process.getuid?.() !== undefined && info.uid !== process.getuid())) {
  throw new Error('post-exit release sentinel is not a private owned regular file');
}
if (await realpath(path) !== path) throw new Error('post-exit release sentinel path is not canonical');
if (await readFile(path, 'utf8') !== 'dakia-native-provider-reader-post-exit-complete-v1\n') throw new Error('post-exit release sentinel content is invalid');
NODE
}

cleanup() {
  local status=$?
  if [[ "$cleanup_started" == true ]]; then return "$status"; fi
  cleanup_started=true
  trap - EXIT INT TERM
  stop_owned_child "$app_pid"
  app_pid=""
  if [[ "$approved_identity_transaction_started" == true ]]; then
    approved_identity_process_probe='probe-error'
    if [[ "$approved_identity_post_exit_hold_active" == true ]]; then
      printf 'The approved candidate post-exit handoff is still active. Retaining the transaction for explicit recovery.\n' >&2
      retain_run_root=true
      status=1
    elif [[ "$approved_identity_process_probe_failed" != true ]]; then
      approved_identity_process_probe="$(approved_identity_process_state)"
    fi
    if [[ "$approved_identity_post_exit_hold_active" == true ]]; then
      :
    elif [[ "$approved_identity_process_probe" == 'running' ]]; then
      printf 'The approved candidate is still running. Retaining the active transaction for explicit recovery instead of moving a live bundle.\n' >&2
      retain_run_root=true
      status=1
    elif [[ "$approved_identity_process_probe" != 'absent' ]]; then
      printf 'Could not prove the approved candidate is absent. Retaining the active transaction for explicit recovery.\n' >&2
      retain_run_root=true
      status=1
    elif { [[ ! -e "$approved_identity_journal" ]] || approved_identity_state restore "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" "$run_root" "$run_root/Dakia Fictional Provider Reader.app" "$approved_identity_identifier" "$approved_identity_desktop_sha256" "$approved_identity_cli_sha256"; } \
      && approved_identity_state verify "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" '' '' "$approved_identity_identifier" "$approved_identity_desktop_sha256" "$approved_identity_cli_sha256" \
      && verify_approved_bundle_signature "$approved_identity_app"; then
      approved_identity_swapped=false
      approved_identity_transaction_started=false
    else
      printf 'Approved application identity restoration failed. Retaining the exact run root and journal for explicit recovery.\n' >&2
      retain_run_root=true
      status=1
    fi
  fi
  stop_owned_child "$vite_pid"
  vite_pid=""
  stop_owned_child "$proxy_pid"
  proxy_pid=""
  stop_owned_child "$attachment_interruption_pid"
  attachment_interruption_pid=""
  stop_owned_child "$fixture_pid"
  fixture_pid=""
  if [[ -n "$container_id" ]]; then
    docker rm --force "$container_id" >/dev/null 2>&1 || true
    container_id=""
  fi
  if [[ -f "$run_root/source-before-build.json" ]]; then
    verify_source_snapshot after-run || status=1
  fi
  if [[ "$retain_run_root" != true && "${DAKIA_KEEP_NATIVE_PROVIDER_READER_ARTIFACTS:-}" != "1" ]]; then
    rm -rf "$run_root"
  else
    printf 'Retained native-provider-reader artifacts: %s\n' "$run_root"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in cargo docker file node python3 npx openssl codesign lsof rg sqlite3 otool install_name_tool; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'Required command is unavailable: %s\n' "$command" >&2
    exit 69
  }
done
if [[ "$packaged" == true ]]; then
  for command in ditto lipo; do
    command -v "$command" >/dev/null 2>&1 || {
      printf 'Required packaged-app command is unavailable: %s\n' "$command" >&2
      exit 69
    }
  done
  [[ -x /usr/libexec/PlistBuddy ]] || {
    printf 'Required packaged-app plist inspector is unavailable.\n' >&2
    exit 69
  }
fi
docker info >/dev/null 2>&1 || {
  printf 'Docker daemon is unavailable. No fictional provider was started.\n' >&2
  exit 69
}
if [[ "$packaged" != true ]] && lsof -nP -iTCP:1420 -sTCP:LISTEN >/dev/null 2>&1; then
  printf 'Port 1420 is already in use. Refusing to attach the acceptance app to an unknown Vite server.\n' >&2
  exit 69
fi

certificate_root="$run_root/tls"
profile_root="$run_root/profile"
downloads_root="$run_root/Downloads"
mkdir -p "$certificate_root" "$profile_root" "$downloads_root"
chmod 700 "$profile_root" "$downloads_root"
(
  umask 0177
  printf '%s\n' 'dakia-native-mail-safety-fixture-v1' > "$run_root/.dakia-native-mail-safety-root"
  printf '%s\n' 'dakia-native-mail-safety-fixture-v1' > "$profile_root/.dakia-native-mail-safety-profile"
)

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=Dakia fictional provider test CA' \
  -addext 'basicConstraints=critical,CA:true' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  -keyout "$certificate_root/ca.key" -out "$certificate_root/ca.crt" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj '/CN=localhost' \
  -keyout "$certificate_root/tls.key" -out "$certificate_root/tls.csr" >/dev/null 2>&1
printf '%s\n' \
  '[server]' \
  'basicConstraints=critical,CA:false' \
  'keyUsage=critical,digitalSignature,keyEncipherment' \
  'extendedKeyUsage=serverAuth' \
  'subjectAltName=DNS:localhost,IP:127.0.0.1' > "$certificate_root/server.ext"
openssl x509 -req -in "$certificate_root/tls.csr" \
  -CA "$certificate_root/ca.crt" -CAkey "$certificate_root/ca.key" -CAcreateserial \
  -days 1 -extfile "$certificate_root/server.ext" -extensions server \
  -out "$certificate_root/tls.crt" >/dev/null 2>&1
openssl pkcs12 -export -name greenmail -out "$certificate_root/greenmail.p12" \
  -inkey "$certificate_root/tls.key" -in "$certificate_root/tls.crt" \
  -certfile "$certificate_root/ca.crt" -passout pass:synthetic-keystore-password >/dev/null 2>&1
openssl x509 -in "$certificate_root/ca.crt" -outform DER -out "$certificate_root/tls.der"
chmod 755 "$certificate_root"
chmod 644 "$certificate_root/greenmail.p12" "$certificate_root/tls.der"

if [[ "$attachment_interruption" == true && -z "${DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT:-}" ]]; then
  export DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT=1
fi
fixture_users='fictional-reader:synthetic-only-password@localhost'
if [[ -n "${DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT:-}" ]]; then
  if [[ "$DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT" != "1" ]]; then
    printf 'The optional second fictional-account marker must equal 1.\n' >&2
    exit 64
  fi
  fixture_users+=',fictional-reader-two:synthetic-only-password@localhost'
fi
docker pull greenmail/standalone:2.1.14 >/dev/null
container_id="$(docker run -d --name "$container" --memory=512m --pids-limit=200 \
  -e 'GREENMAIL_OPTS=-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.tls.keystore.file=/tls/greenmail.p12 -Dgreenmail.tls.keystore.password=synthetic-keystore-password -Dgreenmail.tls.key.password=synthetic-keystore-password -Dgreenmail.users.login=email -Dgreenmail.users='"$fixture_users" \
  -v "$certificate_root:/tls:ro" -p 127.0.0.1::3993 -p 127.0.0.1::3465 \
  greenmail/standalone:2.1.14)"
[[ "$container_id" =~ ^[a-f0-9]{64}$ ]] || { printf 'Docker did not return an exact owned container ID.\n' >&2; exit 1; }
imap_port="$(docker port "$container" 3993/tcp | sed -n '1s/.*://p')"
smtp_port="$(docker port "$container" 3465/tcp | sed -n '1s/.*://p')"
[[ "$imap_port" =~ ^[0-9]+$ && "$smtp_port" =~ ^[0-9]+$ ]] || {
  printf 'Fictional provider did not expose IMAP and SMTP loopback ports.\n' >&2
  exit 1
}

tls_ready() {
  node --input-type=module - "$1" <<'NODE'
import tls from 'node:tls';

const port = Number(process.argv[2]);
const socket = tls.connect({ host: '127.0.0.1', port, servername: 'localhost', rejectUnauthorized: false });
const timer = setTimeout(() => socket.destroy(new Error('TLS readiness timed out')), 2_000);
socket.once('secureConnect', () => {
  clearTimeout(timer);
  socket.end();
  process.exit(0);
});
socket.once('error', () => {
  clearTimeout(timer);
  process.exit(1);
});
NODE
}
for _ in $(seq 1 30); do
  if tls_ready "$imap_port"; then break; fi
  sleep 1
done
tls_ready "$imap_port" || {
  printf 'Timed out waiting for fictional IMAP TLS.\n' >&2
  exit 1
}

for fixture_port in "$fixture_http_port" "$fixture_tls_probe_port"; do
  lsof -nP -iTCP:"$fixture_port" -sTCP:LISTEN >/dev/null 2>&1 && {
    printf 'Port %s is already in use. Refusing to attach fixture requests to an unknown process.\n' "$fixture_port" >&2
    exit 69
  }
done
node "$repo_root/scripts/native-mail-safety-fixture-server.mjs" \
  --port "$fixture_http_port" --tls-probe-port "$fixture_tls_probe_port" \
  --evidence "$run_root/network-requests.json" >"$run_root/fixture.log" 2>&1 &
fixture_pid=$!
fixture_ready=false
for _ in $(seq 1 30); do
  if ! kill -0 "$fixture_pid" >/dev/null 2>&1; then
    cat "$run_root/fixture.log" >&2
    exit 1
  fi
  if node -e "fetch('http://127.0.0.1:${fixture_http_port}/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"; then
    fixture_ready=true
    break
  fi
  sleep 1
done
if [[ "$fixture_ready" != "true" ]] || ! kill -0 "$fixture_pid" >/dev/null 2>&1; then
  printf 'Timed out waiting for the owned fictional content fixture.\n' >&2
  cat "$run_root/fixture.log" >&2
  exit 1
fi

# Submit a malformed regression input first, then the normal message which the
# Reader acceptance opens. Both are server-side bytes, not seeded local rows.
mailto_regressions="$run_root/native-mailto-regressions.json"
node --input-type=module - "$smtp_port" "$certificate_root/ca.crt" "$fixture_http_port" "$fixture_tls_probe_port" "$mailto_regressions" <<'NODE'
import tls from 'node:tls';
import { once } from 'node:events';
import { readFileSync } from 'node:fs';
import { writeFile } from 'node:fs/promises';

const port = Number(process.argv[2]);
const socket = tls.connect({ host: '127.0.0.1', port, servername: 'localhost', ca: readFileSync(process.argv[3]), rejectUnauthorized: true });
let buffered = '';
let waiting;
let socketFailure;
const drain = () => {
  if (!waiting) return;
  if (socketFailure) {
    const { reject, timer } = waiting;
    waiting = undefined;
    clearTimeout(timer);
    reject(socketFailure);
    return;
  }
  const index = buffered.indexOf('\n');
  if (index < 0) return;
  const value = buffered.slice(0, index + 1);
  buffered = buffered.slice(index + 1);
  const { resolve, timer } = waiting;
  waiting = undefined;
  clearTimeout(timer);
  resolve(value);
};
socket.on('data', (chunk) => {
  buffered += chunk;
  drain();
});
socket.on('error', (error) => {
  socketFailure = error;
  drain();
});
const line = () => new Promise((resolve, reject) => {
  if (waiting) throw new Error('SMTP reply reader received concurrent read');
  const timer = setTimeout(() => {
    if (waiting) {
      waiting = undefined;
      reject(new Error('SMTP reply timed out'));
    }
  }, 5_000);
  waiting = { resolve, reject, timer };
  drain();
});
const reply = async (expected) => {
  const first = (await line()).replace(/\r?\n$/, '');
  const match = /^(\d{3})([- ])/.exec(first);
  if (!match) throw new Error(`malformed SMTP reply: ${first}`);
  if (match[1] !== expected) throw new Error(`expected SMTP ${expected}, got: ${first}`);
  const lines = [first];
  if (match[2] === ' ') return lines;
  for (;;) {
    const next = (await line()).replace(/\r?\n$/, '');
    lines.push(next);
    if (new RegExp(`^${match[1]} `).test(next)) return lines;
  }
};
const command = async (value, expected) => {
  socket.write(`${value}\r\n`);
  return reply(expected);
};
await new Promise((resolve, reject) => {
  let settled = false;
  const finish = (callback, value) => {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    socket.off('secureConnect', onSecureConnect);
    socket.off('error', onHandshakeError);
    callback(value);
  };
  const onSecureConnect = () => finish(resolve);
  const onHandshakeError = (error) => finish(reject, error);
  const timer = setTimeout(
    () => finish(reject, new Error('SMTP TLS handshake timed out')),
    5_000,
  );
  socket.once('secureConnect', onSecureConnect);
  socket.once('error', onHandshakeError);
});
await reply('220');
await command('EHLO localhost', '250');
await command(`AUTH PLAIN ${Buffer.from('\0fictional-reader@localhost\0synthetic-only-password').toString('base64')}`, '235');
const submit = async (message) => {
  await command('MAIL FROM:<provider@example.test>', '250');
  await command('RCPT TO:<fictional-reader@localhost>', '250');
  await command('DATA', '354');
  socket.write(`${message.replace(/^\./gm, '..')}\r\n.\r\n`);
  await reply('250');
};
const fixtureHttpPort = Number(process.argv[4]);
const fixtureTlsProbePort = Number(process.argv[5]);
if (!Number.isInteger(fixtureHttpPort) || !Number.isInteger(fixtureTlsProbePort) || fixtureHttpPort === fixtureTlsProbePort) {
  throw new Error('Fictional content fixture ports are invalid');
}
const fixtureOrigin = `http://127.0.0.1:${fixtureHttpPort}`;
const tlsProbeOrigin = `//127.0.0.1:${fixtureTlsProbePort}`;
const mailtoOutput = process.argv[6];
if (!mailtoOutput) throw new Error('Native mailto regression output path is missing');

const sourceAccountId = '62a9320c-04f5-47c2-b4fa-d4952f0b6121';
const mailtoWarning = 'Some address or subject fields contain line breaks. Remove them before sending.';
const recipientInvariant = 'fictional@example.test';
const mailtoCases = [
  {
    id: 'native-mailto-cr-to',
    label: 'Native mailto CR To rejection',
    href: 'mailto:fictional%40example.test%0DBcc%3Afictional%40example.test?subject=Fictional%20report',
    expectedSeed: { to: 'fictional@example.test\rBcc:fictional@example.test', subject: 'Fictional report' },
    expectedNormalizedSubmission: null,
    warningVisible: true,
    sendDisabled: true,
  },
  {
    id: 'native-mailto-cr-cc',
    label: 'Native mailto CR Cc rejection',
    href: 'mailto:fictional%40example.test?cc=fictional%40example.test%0DBcc%3Afictional%40example.test&subject=Fictional%20report',
    expectedSeed: { to: 'fictional@example.test', cc: 'fictional@example.test\rBcc:fictional@example.test', subject: 'Fictional report' },
    expectedNormalizedSubmission: null,
    warningVisible: true,
    sendDisabled: true,
  },
  {
    id: 'native-mailto-cr-subject',
    label: 'Native mailto CR subject rejection',
    href: 'mailto:fictional%40example.test?subject=Fictional%20report%0DX-Injected%3Ayes',
    expectedSeed: { to: 'fictional@example.test', subject: 'Fictional report\rX-Injected:yes' },
    expectedNormalizedSubmission: null,
    warningVisible: true,
    sendDisabled: true,
  },
  {
    id: 'native-mailto-lf-to',
    label: 'Native mailto LF To rejection',
    href: 'mailto:fictional%40example.test%0ABcc%3Afictional%40example.test?subject=Fictional%20report',
    expectedSeed: { to: 'fictional@example.test\nBcc:fictional@example.test', subject: 'Fictional report' },
    expectedNormalizedSubmission: null,
    warningVisible: true,
    sendDisabled: true,
  },
  {
    id: 'native-mailto-lf-cc',
    label: 'Native mailto LF Cc rejection',
    href: 'mailto:fictional%40example.test?cc=fictional%40example.test%0ABcc%3Afictional%40example.test&subject=Fictional%20report',
    expectedSeed: { to: 'fictional@example.test', cc: 'fictional@example.test\nBcc:fictional@example.test', subject: 'Fictional report' },
    expectedNormalizedSubmission: null,
    warningVisible: true,
    sendDisabled: true,
  },
  {
    id: 'native-mailto-lf-subject',
    label: 'Native mailto LF subject rejection',
    href: 'mailto:fictional%40example.test?subject=Fictional%20report%0AX-Injected%3Ayes',
    expectedSeed: { to: 'fictional@example.test', subject: 'Fictional report\nX-Injected:yes' },
    expectedNormalizedSubmission: null,
    warningVisible: true,
    sendDisabled: true,
  },
  {
    id: 'native-mailto-double-to',
    label: 'Native mailto double encoded To',
    href: 'mailto:%22Fictional%250D%250AName%22%20%3Cfictional%40example.test%3E?subject=Fictional%20report',
    expectedSeed: { to: '"Fictional%0D%0AName" <fictional@example.test>', subject: 'Fictional report' },
    expectedNormalizedSubmission: { to: ['"Fictional%0D%0AName" <fictional@example.test>'], cc: [], subject: 'Fictional report', bodyText: '' },
    warningVisible: false,
    sendDisabled: false,
  },
  {
    id: 'native-mailto-double-cc',
    label: 'Native mailto double encoded Cc',
    href: 'mailto:fictional%40example.test?cc=%22Fictional%250D%250AName%22%20%3Cfictional%40example.test%3E&subject=Fictional%20report',
    expectedSeed: { to: 'fictional@example.test', cc: '"Fictional%0D%0AName" <fictional@example.test>', subject: 'Fictional report' },
    expectedNormalizedSubmission: { to: ['fictional@example.test'], cc: ['"Fictional%0D%0AName" <fictional@example.test>'], subject: 'Fictional report', bodyText: '' },
    warningVisible: false,
    sendDisabled: false,
  },
  {
    id: 'native-mailto-double-subject',
    label: 'Native mailto double encoded subject',
    href: 'mailto:fictional%40example.test?subject=Fictional%250D%250AX-Not-A-Header%3Ayes',
    expectedSeed: { to: 'fictional@example.test', subject: 'Fictional%0D%0AX-Not-A-Header:yes' },
    expectedNormalizedSubmission: { to: ['fictional@example.test'], cc: [], subject: 'Fictional%0D%0AX-Not-A-Header:yes', bodyText: '' },
    warningVisible: false,
    sendDisabled: false,
  },
  {
    id: 'native-mailto-double-body',
    label: 'Native mailto double encoded body',
    href: 'mailto:fictional%40example.test?subject=Fictional%20report&body=Literal%20keeps%20%250D%250Atext',
    expectedSeed: { to: 'fictional@example.test', subject: 'Fictional report', body: 'Literal keeps %0D%0Atext' },
    expectedNormalizedSubmission: { to: ['fictional@example.test'], cc: [], subject: 'Fictional report', bodyText: 'Literal keeps %0D%0Atext' },
    warningVisible: false,
    sendDisabled: false,
  },
  {
    id: 'native-mailto-body-crlf',
    label: 'Native mailto body CRLF',
    href: 'mailto:fictional%40example.test?subject=Fictional%20report&body=First%20line%0D%0ASecond%20line',
    expectedSeed: { to: 'fictional@example.test', subject: 'Fictional report', body: 'First line\r\nSecond line' },
    expectedNormalizedSubmission: { to: ['fictional@example.test'], cc: [], subject: 'Fictional report', bodyText: 'First line\nSecond line' },
    warningVisible: false,
    sendDisabled: false,
  },
];
if (mailtoCases.length !== 11 || new Set(mailtoCases.map(({ id }) => id)).size !== 11) {
  throw new Error('Native mailto regression fixture must have exactly eleven unique cases');
}
for (const testCase of mailtoCases) {
  if (!/^native-mailto-[a-z0-9-]+$/.test(testCase.id) || !/^mailto:/i.test(testCase.href) || /https?:/i.test(testCase.href)) {
    throw new Error(`Invalid native mailto regression case: ${testCase.id}`);
  }
}
const mailtoRegressions = {
  schemaVersion: 1,
  sourceAccountId,
  sourceMessageSubject: 'Remote content safety check',
  recipientInvariant,
  neverSend: true,
  warningText: mailtoWarning,
  cases: mailtoCases,
};
await writeFile(mailtoOutput, `${JSON.stringify(mailtoRegressions, null, 2)}\n`, { mode: 0o600 });
const escapeHtml = (value) => value.replace(/[&<>'"]/g, (character) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', "'": '&#39;', '"': '&quot;',
})[character]);
const mailtoAnchorHtml = [
  '<section aria-label="Native mailto regression anchors">',
  '<p>Native mailto regression anchors. Open each in Dakia compose and close without sending.</p>',
  ...mailtoCases.map(({ id, label, href }) => `<p><a id="${escapeHtml(id)}" href="${escapeHtml(href)}">${escapeHtml(label)}</a></p>`),
  '</section>',
].join('');
await submit([
  'From: Fictional Provider <provider@example.test>',
  'To: Fictional Reader <fictional-reader@localhost>',
  'Subject: Fictional Reader height-bound acceptance',
  'Message-ID: <fictional-reader-height-bound@localhost>',
  'In-Reply-To: <fictional-provider-reader-thread@localhost>',
  'References: <fictional-provider-reader-thread@localhost>',
  'MIME-Version: 1.0',
  'Content-Type: text/html; charset=utf-8',
  '',
  '<style>html,body { height:100px!important; max-height:100px!important; overflow:hidden!important }</style><p>Fictional long email starts here.</p><div style="height:9999999px">Fictional tall message content.</div><p>Fictional bottom marker remains reachable by scrolling inside the capped email surface.</p>',
  '',
].join('\r\n'));
await submit([
  'From: Fictional Provider <provider@example.test>',
  'To: Fictional Reader <fictional-reader@localhost>',
  'Subject: Malformed fictional MIME regression',
  'Message-ID: <fictional-malformed-mime-regression@localhost>',
  'MIME-Version: 1.0',
  'Content-Type: multipart/mixed; boundary="missing-boundary"',
  '',
  '--different-boundary',
  'Content-Type: text/plain; charset=utf-8',
  '',
  'This deliberately malformed multipart message must not stop the normal message.',
  '',
].join('\r\n'));
await submit([
  'From: Fictional Provider <provider@example.test>',
  'To: Fictional Reader <fictional-reader@localhost>',
  'Subject: Remote content safety check',
  'Message-ID: <fictional-provider-reader-acceptance@localhost>',
  'In-Reply-To: <fictional-provider-reader-thread@localhost>',
  'References: <fictional-provider-reader-thread@localhost>',
  'MIME-Version: 1.0',
  'Content-Type: multipart/mixed; boundary="fixture-boundary"',
  '',
  '--fixture-boundary',
  'Content-Type: text/html; charset=utf-8',
  '',
  [
    '<p>Fictional provider body reached the real Reader.</p>',
    '<p id="native-resource-matrix-marker">Native remote-resource matrix marker.</p>',
    `<link rel="stylesheet" href="${fixtureOrigin}/remote-style.css">`,
    '<style>',
    `@import url("${fixtureOrigin}/remote-style.css?via=import");`,
    `@font-face { font-family: NativeSafetyFixture; src: url("${fixtureOrigin}/remote-font.woff2"); }`,
    `.remote-style-marker { background-image: url("${fixtureOrigin}/background.svg"); font-family: NativeSafetyFixture, sans-serif; }`,
    `.encoded-css-marker { background-image: u\\72l("${fixtureOrigin}/background.svg?via=escaped-css"); }`,
    '</style>',
    `<p class="remote-style-marker encoded-css-marker" style="background-image:u\\72l(${fixtureOrigin}/background.svg?via=escaped-inline)">Remote CSS and font marker stays readable.</p>`,
    `<img src="${fixtureOrigin}/remote-image.svg?via=direct" alt="Direct remote fixture">`,
    `<img src="${fixtureOrigin}/redirect-image" alt="Redirected remote fixture">`,
    `<img src="http&#58;//127.0.0.1:${fixtureHttpPort}/remote-image.svg?via=encoded-html" alt="Encoded remote fixture">`,
    `<img src="${tlsProbeOrigin}/remote-image.svg?via=protocol-relative" alt="Protocol-relative remote fixture">`,
    `<img srcset="${fixtureOrigin}/srcset-image.svg 1x, ${fixtureOrigin}/srcset-image.svg?density=2 2x" alt="Srcset must stay inert">`,
    `<img src="data:image/svg+xml;base64,${Buffer.from("<svg xmlns='http://www.w3.org/2000/svg' width='320' height='48'><rect width='320' height='48' fill='#d7f5e8'/><text x='12' y='30' fill='#164a37' font-size='18'>Embedded image loaded locally</text></svg>").toString('base64')}" alt="Local embedded safety marker">`,
    `<img src="data:image/svg+xml;base64,${Buffer.from(`<svg xmlns="http://www.w3.org/2000/svg" width="320" height="48"><rect width="320" height="48" fill="#d7f5e8"/><text x="12" y="30" fill="#164a37" font-size="18">Embedded SVG stays inert</text><image href="${fixtureOrigin}/svg-external.svg?via=data-svg"/><script>fetch('${fixtureOrigin}/script-executed?via=data-svg')</script><foreignObject><iframe xmlns="http://www.w3.org/1999/xhtml" src="${fixtureOrigin}/hostile-frame.html?via=data-svg"></iframe></foreignObject></svg>`).toString('base64')}" alt="Embedded SVG active-content canary">`,
    `<svg width="20" height="20" aria-label="SVG remote references stay inert"><image href="${fixtureOrigin}/svg-external.svg" width="20" height="20"></image><rect width="20" height="20" fill="url(${fixtureOrigin}/background.svg?via=svg-fill)" filter="u\\72l(${fixtureOrigin}/svg-external.svg?via=svg-filter)"></rect><use href="${fixtureOrigin}/svg-external.svg#remote-symbol"></use><animate attributeName="href" values="${fixtureOrigin}/svg-external.svg"></animate></svg>`,
    `<video autoplay src="${fixtureOrigin}/remote-video.mp4" poster="${fixtureOrigin}/remote-poster.svg"><source src="${fixtureOrigin}/remote-video-source.mp4"></video>`,
    `<audio autoplay src="${fixtureOrigin}/remote-audio.mp3"><source src="${fixtureOrigin}/remote-audio-source.mp3"></audio>`,
    `<meta http-equiv="refresh" content="0; url=${fixtureOrigin}/form-submit?via=refresh">`,
    `<base href="${fixtureOrigin}/">`,
    `<script src="${fixtureOrigin}/hostile-script.js"></script>`,
    `<script>fetch('${fixtureOrigin}/script-executed?via=inline')</script>`,
    `<iframe src="${fixtureOrigin}/hostile-frame.html" srcdoc="&lt;script&gt;fetch('${fixtureOrigin}/frame-executed?via=srcdoc')&lt;/script&gt;"></iframe>`,
    `<object data="${fixtureOrigin}/hostile-frame.html?via=object"></object>`,
    `<form action="${fixtureOrigin}/form-submit"><input name="fixture" value="blocked"><button type="submit">Submit hostile form</button></form>`,
    `<img src="${fixtureOrigin}/remote-image.svg?via=event-handler" onload="fetch('${fixtureOrigin}/script-executed?via=onload')" onerror="fetch('${fixtureOrigin}/script-executed?via=onerror')" alt="Event handler canary">`,
    '<img src="file:///etc/passwd" alt="Local file canary"><img src="asset://localhost/private.png" alt="Asset canary"><img src="tauri://localhost/ipc" alt="Tauri canary"><img src="ipc://localhost/native-command" alt="IPC canary">',
    mailtoAnchorHtml,
    '<p id="native-active-content-marker">Active-content canaries must remain inert before and after consent.</p>',
    '<div class="gmail_quote">On 17 October 2026, Fictional Sender wrote:<blockquote><p id="native-history-cycle-marker">Fictional quoted history remains inside the isolated history iframe.</p><p>Expand, collapse, expand, collapse, and expand this history. The marker and Reader controls must remain visible and usable after every transition.</p><a href="https://example.invalid/history">History link</a></blockquote></div>',
  ].join(''),
  '--fixture-boundary',
  'Content-Type: application/octet-stream; name="quarterly-report.pdf.command"',
  'Content-Disposition: attachment; filename="../../quarterly-report.pdf.command"',
  'Content-Transfer-Encoding: base64',
  '',
  Buffer.from('fictional attachment bytes\0\xff', 'binary').toString('base64'),
  '--fixture-boundary',
  'Content-Type: text/plain; name="meeting-notes.txt"',
  'Content-Disposition: attachment; filename="meeting-notes.txt"',
  'Content-Transfer-Encoding: base64',
  '',
  Buffer.from('fictional safe attachment bytes\n', 'utf8').toString('base64'),
  '--fixture-boundary--',
  '',
].join('\r\n'));
if (process.env.DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT === '1') {
  const submitSecond = async (raw) => {
    await command('MAIL FROM:<provider@example.test>', '250');
    await command('RCPT TO:<fictional-reader-two@localhost>', '250');
    await command('DATA', '354');
    socket.write(`${raw.replace(/^\./gm, '..')}\r\n.\r\n`);
    await reply('250');
  };
  for (const index of [1, 2]) {
    await submitSecond([
      'From: Fictional Provider <provider@example.test>',
      'To: Fictional Reader Two <fictional-reader-two@localhost>',
      `Subject: Account two fictional filler ${index}`,
      `Message-ID: <fictional-account-two-filler-${index}@localhost>`,
      'Content-Type: text/plain; charset=utf-8',
      '',
      `Account two filler ${index} stays in its own account.`,
      '',
    ].join('\r\n'));
  }
  await submitSecond([
    'From: Fictional Provider <provider@example.test>',
    'To: Fictional Reader Two <fictional-reader-two@localhost>',
    'Subject: Account two collision safety check',
    'Message-ID: <fictional-provider-reader-acceptance@localhost>',
    'In-Reply-To: <fictional-provider-reader-thread@localhost>',
    'References: <fictional-provider-reader-thread@localhost>',
    'MIME-Version: 1.0',
    'Content-Type: multipart/mixed; boundary="account-two-boundary"',
    '',
    '--account-two-boundary',
    'Content-Type: text/plain; charset=utf-8',
    '',
    'Only account two owns this distinct cached body and attachment.',
    '--account-two-boundary',
    'Content-Type: text/plain; name="account-two-notes.txt"',
    'Content-Disposition: attachment; filename="account-two-notes.txt"',
    'Content-Transfer-Encoding: base64',
    '',
    Buffer.from('fictional account two attachment bytes\n').toString('base64'),
    '--account-two-boundary--',
    '',
  ].join('\r\n'));
}
await command('QUIT', '221');
socket.destroy();
NODE

python3 - "$imap_port" "$certificate_root/ca.crt" <<'PYTHON'
import imaplib, os, re, ssl, sys
context = ssl.create_default_context(cafile=sys.argv[2])
users = ['fictional-reader@localhost']
if os.environ.get('DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT') == '1':
    users.append('fictional-reader-two@localhost')
for user in users:
    with imaplib.IMAP4_SSL('127.0.0.1', int(sys.argv[1]), ssl_context=context, timeout=10) as client:
        client.login(user, 'synthetic-only-password')
        status, count = client.select('INBOX', readonly=True)
        if status != 'OK' or count != [b'3']:
            raise RuntimeError('Fictional account does not have exactly three seeded messages')
        status, data = client.uid('fetch', '1:*', '(UID FLAGS)')
        if status != 'OK' or len(data) != 3:
            raise RuntimeError('Fictional account did not return three UID flag records')
        observed = {int(match.group(1)) for row in data if isinstance(row, bytes) for match in re.finditer(rb'\bUID (\d+)\b', row)}
        if observed != {1, 2, 3}:
            raise RuntimeError('Fictional mailbox UID collision fixture is not exact')
        if any(b'\\Seen' in row for row in data if isinstance(row, bytes)):
            raise RuntimeError('Fictional fixture setup unexpectedly marked mail as read')
print(f'Confirmed three unread server-side messages in each of {len(users)} fictional account(s).')
PYTHON

attachment_interruption_binding=''
attachment_interruption_raw=''
attachment_interruption_receipt=''
attachment_interruption_binding_sha256=''
if [[ "$attachment_interruption" == true ]]; then
  [[ "${DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT:-}" == '1' ]] || {
    printf '%s requires DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT=1 so the duplicate Message-ID account boundary is present.\n' '--attachment-interruption' >&2
    exit 64
  }
  attachment_interruption_binding="$run_root/attachment-interruption-binding.json"
  attachment_interruption_raw="$run_root/attachment-interruption-uid3.eml"
  attachment_interruption_receipt="$run_root/attachment-interruption-preflight.json"
  [[ ! -e "$attachment_interruption_binding" && ! -e "$attachment_interruption_raw" && ! -e "$attachment_interruption_receipt" ]] || {
    printf 'Attachment interruption evidence paths already exist.\n' >&2
    exit 1
  }
  chmod 600 "$certificate_root/ca.crt" "$certificate_root/tls.crt" "$certificate_root/tls.key"
  python3 "$repo_root/scripts/native-provider-reader-attachment-binding.py" \
    --root "$run_root" \
    --host 127.0.0.1 \
    --port "$imap_port" \
    --ca "$certificate_root/ca.crt" \
    --account-id '62a9320c-04f5-47c2-b4fa-d4952f0b6121' \
    --email 'fictional-reader@localhost' \
    --collision-email 'fictional-reader-two@localhost' \
    --fixture-raw "$attachment_interruption_raw" \
    --binding-artifact "$attachment_interruption_binding" \
    <<< 'synthetic-only-password' > "$attachment_interruption_receipt"
  chmod 600 "$attachment_interruption_receipt"
  attachment_interruption_binding_sha256="$(node --input-type=module - "$attachment_interruption_receipt" "$attachment_interruption_binding" <<'NODE'
import { createHash } from 'node:crypto';
import { lstat, readFile, realpath } from 'node:fs/promises';
const [receiptPath, bindingPath] = process.argv.slice(2);
for (const path of [receiptPath, bindingPath]) {
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || (info.mode & 0o777) !== 0o600 || (process.getuid?.() !== undefined && info.uid !== process.getuid())) throw new Error('attachment interruption preflight output is unsafe');
  if (await realpath(path) !== path) throw new Error('attachment interruption preflight output path is not canonical');
}
const receipt = JSON.parse(await readFile(receiptPath, 'utf8'));
const binding = await readFile(bindingPath);
const digest = createHash('sha256').update(binding).digest('hex');
if (receipt.status !== 'prepared' || receipt.bindingArtifactSha256 !== digest || receipt.uid !== 3 || receipt.attachmentSection !== '3' || receipt.attachmentId !== '62a9320c-04f5-47c2-b4fa-d4952f0b6121:INBOX:3:mime-v1:3') throw new Error('attachment interruption preflight receipt is inconsistent');
process.stdout.write(digest);
NODE
)"
fi

if [[ "$prepare_only" == "true" && "$attachment_interruption" != true ]]; then
  printf 'Fictional provider fixture accepted normal and malformed MIME messages.\n'
  exit 0
fi

backend_imap_port="$imap_port"
if [[ "$attachment_interruption" == true ]]; then
  node "$repo_root/scripts/native-fictional-attachment-interruption-proxy.mjs" serve \
    --root "$run_root" \
    --backend-port "$backend_imap_port" \
    --certificate "$certificate_root/tls.crt" \
    --key "$certificate_root/tls.key" \
    --ca "$certificate_root/ca.crt" \
    --account-id '62a9320c-04f5-47c2-b4fa-d4952f0b6121' \
    --binding-artifact "$attachment_interruption_binding" \
    --binding-artifact-sha256 "$attachment_interruption_binding_sha256" \
    --fixture-raw "$attachment_interruption_raw" \
    >"$run_root/attachment-interruption-overlay.log" 2>&1 &
  attachment_interruption_pid=$!
  for _ in $(seq 1 30); do
    if [[ -f "$run_root/attachment-interruption-overlay.json" ]]; then break; fi
    if ! kill -0 "$attachment_interruption_pid" >/dev/null 2>&1; then break; fi
    sleep 1
  done
  [[ -f "$run_root/attachment-interruption-overlay.json" ]] || {
    printf 'Fictional attachment interruption overlay did not become ready.\n' >&2
    exit 1
  }
  backend_imap_port="$(node --input-type=module - "$run_root/attachment-interruption-overlay.json" "$attachment_interruption_binding_sha256" "$backend_imap_port" "$attachment_interruption_pid" <<'NODE'
import { lstat, readFile, realpath } from 'node:fs/promises';
const [path, expectedBindingSha256, expectedBackendPort, expectedPid] = process.argv.slice(2);
const info = await lstat(path);
if (!info.isFile() || info.isSymbolicLink() || (info.mode & 0o777) !== 0o600 || (process.getuid?.() !== undefined && info.uid !== process.getuid())) throw new Error('attachment interruption overlay metadata is unsafe');
if (await realpath(path) !== path) throw new Error('attachment interruption overlay metadata path is not canonical');
const report = JSON.parse(await readFile(path, 'utf8'));
if (report.schemaVersion !== 1 || report.mode !== 'fictional-attachment-interruption-inner-tls' || report.routeAccountId !== '62a9320c-04f5-47c2-b4fa-d4952f0b6121') throw new Error('attachment interruption overlay identity is invalid');
if (!Number.isInteger(report.port) || report.port < 1 || report.port > 65535 || report.backendPort !== Number(expectedBackendPort) || report.pid !== Number(expectedPid)) throw new Error('attachment interruption overlay process or ports are invalid');
if (report.bound?.accountId !== report.routeAccountId || report.bound?.uid !== 3 || report.bound?.attachmentSection !== '3' || report.bound?.attachmentId !== '62a9320c-04f5-47c2-b4fa-d4952f0b6121:INBOX:3:mime-v1:3') throw new Error('attachment interruption overlay binding is invalid');
if (!/^[a-f0-9]{48}$/.test(report.controlNonce) || expectedBindingSha256.length !== 64) throw new Error('attachment interruption overlay provenance is invalid');
process.stdout.write(String(report.port));
NODE
)"
fi
node "$repo_root/scripts/native-fictional-imap-proxy.mjs" \
  --root "$run_root" --backend-port "$backend_imap_port" >"$run_root/imap-proxy.log" 2>&1 &
proxy_pid=$!
for _ in $(seq 1 30); do
  if [[ -f "$run_root/imap-proxy.json" ]]; then break; fi
  if ! kill -0 "$proxy_pid" >/dev/null 2>&1; then break; fi
  sleep 1
done
[[ -f "$run_root/imap-proxy.json" ]] || {
  printf 'Fictional IMAP disconnect proxy did not become ready.\n' >&2
  exit 1
}
imap_port="$(node -e 'const fs = require("node:fs"); const report = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); if (!Number.isInteger(report.port) || report.port < 1 || report.port > 65535) process.exit(1); process.stdout.write(String(report.port));' "$run_root/imap-proxy.json")"

if [[ "$prepare_only" == "true" ]]; then
  printf 'Fictional provider fixture and attachment interruption proxies are ready.\n'
  exit 0
fi

export DAKIA_NATIVE_MAIL_SAFETY_FIXTURE=1
export DAKIA_NATIVE_MAIL_SAFETY_ROOT="$run_root"
export DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR="$profile_root"
export DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR="$downloads_root"
export DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST=127.0.0.1
export DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT="$imap_port"
export DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME=localhost
export DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST=127.0.0.1
export DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT="$smtp_port"
export DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME=localhost
export DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH="$certificate_root/tls.der"
export DAKIA_NATIVE_MAIL_SAFETY_PASSWORD=synthetic-only-password

web_dist="$run_root/web-dist"
private_binaries="$run_root/binaries"
if [[ "$packaged" != true ]]; then
  VITE_NATIVE_MAIL_SAFETY_FIXTURE=1 node "$repo_root/node_modules/vite/bin/vite.js" --config "$repo_root/apps/desktop/vite.config.ts" >"$run_root/vite.log" 2>&1 &
  vite_pid=$!
  for _ in $(seq 1 30); do
    if lsof -nP -iTCP:1420 -sTCP:LISTEN | rg -q '127\.0\.0\.1:1420'; then break; fi
    sleep 1
  done
  lsof -nP -iTCP:1420 -sTCP:LISTEN | rg -q '127\.0\.0\.1:1420' || {
    printf 'Vite did not bind the expected loopback address.\n' >&2
    exit 1
  }
else
  mkdir -p "$web_dist" "$private_binaries"
fi

tauri_config="$run_root/tauri.json"
asset_scope_json="$(node -e 'process.stdout.write(JSON.stringify(`${process.argv[1]}/translation-models/**`))' "$profile_root")"
data_store_identifier_evidence="$run_root/native-data-store-identifier.json"
if [[ "${reuse_approved_identity:-false}" == true ]]; then
  node --input-type=module - "$data_store_identifier_evidence" "$native_data_store_identifier" <<'NODE'
import { writeFileSync } from 'node:fs';
const [output, value] = process.argv.slice(2);
if (!/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(value)) throw new Error('native fixture data-store UUID is invalid');
const bytes = [...Buffer.from(value.replaceAll('-', ''), 'hex')];
if (bytes.length !== 16) throw new Error('native fixture data-store UUID did not decode to 16 bytes');
writeFileSync(output, `${JSON.stringify({ schemaVersion: 1, injection: 'runtime-context', identifier: value, bytes }, null, 2)}\n`, { mode: 0o600, flag: 'wx' });
NODE
  export DAKIA_NATIVE_MAIL_SAFETY_DATA_STORE_IDENTIFIER="$native_data_store_identifier"
else
  unset DAKIA_NATIVE_MAIL_SAFETY_DATA_STORE_IDENTIFIER
fi
if [[ "$packaged" == true ]]; then
  node --input-type=module - "$tauri_config" "$wrapper_identifier" "$profile_root/translation-models/**" "$web_dist" "$private_binaries/dakia" <<'NODE'
import { writeFileSync } from 'node:fs';
const [output, identifier, assetScope, frontendDist, externalBin] = process.argv.slice(2);
writeFileSync(output, `${JSON.stringify({
  productName: 'Dakia Fictional Provider Reader',
  identifier,
  build: { beforeBuildCommand: '', beforeDevCommand: '', devUrl: null, frontendDist },
  app: {
    windows: [{ label: 'main', title: 'Dakia Fictional Provider Reader', width: 1440, height: 900 }],
    security: { assetProtocol: { enable: true, scope: [assetScope] } },
  },
  bundle: {
    active: true,
    targets: ['app'],
    createUpdaterArtifacts: false,
    externalBin: [externalBin],
    macOS: { signingIdentity: null, hardenedRuntime: false },
  },
  plugins: { updater: { endpoints: [] } },
}, null, 2)}\n`, { mode: 0o600 });
NODE
else
  printf '%s\n' \
    '{' \
    '  "productName": "Dakia Fictional Provider Reader",' \
    "  \"identifier\": \"$wrapper_identifier\"," \
    '  "build": { "beforeDevCommand": "", "devUrl": "http://127.0.0.1:1420/" },' \
    "  \"app\": { \"windows\": [{ \"label\": \"main\", \"title\": \"Dakia Fictional Provider Reader\", \"width\": 1440, \"height\": 900 }], \"security\": { \"assetProtocol\": { \"enable\": true, \"scope\": [$asset_scope_json] } } }," \
    '  "bundle": { "active": false }' \
    '}' > "$tauri_config"
fi
cargo_wrapper="$run_root/cargo-build-only"
if [[ "$packaged" == true ]]; then
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'if [[ "$1" == "build" ]]; then shift; exec cargo build --locked "$@"; fi' \
    'exec cargo "$@"' > "$cargo_wrapper"
else
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'if [[ "$1" == "run" ]]; then shift; exec cargo build "$@"; fi' \
    'exec cargo "$@"' > "$cargo_wrapper"
fi
chmod 700 "$cargo_wrapper"

node --input-type=module - "$repo_root" "$run_root/source-before-build.json" <<'NODE'
import { writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
const { sourceSnapshot } = await import(pathToFileURL(`${process.argv[2]}/scripts/run-mail-safety.mjs`));
process.chdir(process.argv[2]);
const snapshot = sourceSnapshot();
if (!snapshot) throw new Error('Cannot identify native fixture build inputs');
await writeFile(process.argv[3], `${JSON.stringify(snapshot, null, 2)}\n`, { mode: 0o600 });
NODE
build_target="${DAKIA_NATIVE_MAIL_SAFETY_TARGET_DIR:-/private/tmp/dakia-native-mail-safety-final-20261017/target}"
mkdir -p "$build_target"
cli_target="aarch64-apple-darwin"
(cd "$repo_root" && CARGO_TARGET_DIR="$build_target" \
  cargo build --locked -p dakia-cli --bin dakia --target "$cli_target") >"$run_root/cli-build.log" 2>&1
cli_binary="$build_target/$cli_target/debug/dakia"
test -x "$cli_binary" || { printf 'Native fixture CLI was not built.\n' >&2; exit 1; }
file "$cli_binary" | rg -q 'Mach-O 64-bit executable arm64' || {
  printf 'Native fixture CLI is not an Apple Silicon executable.\n' >&2
  exit 1
}
expected_version="$(node -p "require('$repo_root/apps/desktop/src-tauri/tauri.conf.json').version")"
[[ "$("$cli_binary" --version)" == "dakia $expected_version" ]] || {
  printf 'Native fixture CLI version does not match the desktop app.\n' >&2
  exit 1
}
app_root="$run_root/Dakia Fictional Provider Reader.app"
build_mode_evidence="$run_root/native-build-mode.json"
node --input-type=module - "$build_mode_evidence" "$packaged" "$tauri_config" "$web_dist" "$private_binaries/dakia" <<'NODE'
import { writeFileSync } from 'node:fs';
const [output, packagedValue, config, frontendDist, externalBin] = process.argv.slice(2);
const packaged = packagedValue === 'true';
writeFileSync(output, `${JSON.stringify({
  schemaVersion: 1,
  mode: packaged ? 'packaged-custom-protocol' : 'development-server',
  config,
  frontend: packaged ? { command: 'vite build', output: frontendDist } : { command: 'vite dev', url: 'http://127.0.0.1:1420/' },
  desktop: packaged ? { command: 'tauri build', debug: true, target: 'aarch64-apple-darwin', bundle: 'app', noSign: true } : { command: 'tauri dev', noWatch: true },
  externalBin: packaged ? externalBin : null,
  releaseBoundary: packaged ? 'ad-hoc fixture-enabled debug app; not Developer ID signed, notarized, stapled, updater-signed, installed, or published' : 'fixture-enabled development wrapper',
}, null, 2)}\n`, { mode: 0o600 });
NODE
if [[ "$packaged" == true ]]; then
  (cd "$repo_root" && \
    VITE_NATIVE_MAIL_SAFETY_FIXTURE=1 TAURI_DEBUG=1 \
    node node_modules/vite/bin/vite.js build --config apps/desktop/vite.config.ts \
      --outDir "$web_dist" --emptyOutDir) >"$run_root/vite-build.log" 2>&1
  cp "$cli_binary" "$private_binaries/dakia-$cli_target"
  chmod 755 "$private_binaries/dakia-$cli_target"
  staged_cli_sha256="$(shasum -a 256 "$private_binaries/dakia-$cli_target" | awk '{print $1}')"
  generated_app="$build_target/$cli_target/debug/bundle/macos/Dakia Fictional Provider Reader.app"
  [[ ! -e "$generated_app" ]] || {
    printf 'The packaged fixture output path is not empty. Refusing to replace an unverified bundle.\n' >&2
    exit 73
  }
  (cd "$repo_root" && \
    ORT_LIB_LOCATION="$repo_root/apps/desktop/src-tauri/frameworks" \
    ORT_PREFER_DYNAMIC_LINK=1 \
    CARGO_TARGET_DIR="$build_target" \
    npx tauri build --debug --target "$cli_target" --bundles app --no-sign --ci \
      --features native-mail-safety-fixture --runner "$cargo_wrapper" \
      --config apps/desktop/src-tauri/tauri.conf.json --config "$tauri_config") >"$run_root/tauri-build.log" 2>&1
  verify_source_snapshot after-build
  [[ -d "$generated_app" ]] || {
    printf 'Tauri did not create the exact packaged fixture bundle.\n' >&2
    exit 1
  }
  mv "$generated_app" "$app_root"
  binary="$app_root/Contents/MacOS/dakia-desktop"
  bundled_cli="$app_root/Contents/MacOS/dakia"
  bundled_onnx="$app_root/Contents/Frameworks/libonnxruntime.1.23.2.dylib"
  bundled_classifier="$app_root/Contents/Resources/resources/email-classifier-v2"
  [[ -x "$binary" && -x "$bundled_cli" && -f "$bundled_onnx" && -d "$bundled_classifier" ]] || {
    printf 'Packaged fixture is missing its desktop, fresh CLI, ONNX runtime, or classifier resources.\n' >&2
    exit 1
  }
  [[ "$(shasum -a 256 "$bundled_cli" | awk '{print $1}')" == "$staged_cli_sha256" ]] || {
    printf 'Packaged fixture CLI does not match the fresh isolated build.\n' >&2
    exit 1
  }
  onnx_source="$repo_root/apps/desktop/src-tauri/frameworks/libonnxruntime.1.23.2.dylib"
  [[ "$(shasum -a 256 "$bundled_onnx" | awk '{print $1}')" == "$(shasum -a 256 "$onnx_source" | awk '{print $1}')" ]] || {
    printf 'Packaged fixture ONNX runtime does not match the prepared source asset.\n' >&2
    exit 1
  }
  node --input-type=module - "$repo_root/apps/desktop/src-tauri/resources/email-classifier-v2" "$bundled_classifier" "$run_root/packaged-classifier-inventory.json" <<'NODE'
import { createHash } from 'node:crypto';
import { lstat, readFile, readdir, writeFile } from 'node:fs/promises';
import { join, relative } from 'node:path';
const [sourceRoot, bundledRoot, output] = process.argv.slice(2);
async function inventory(root) {
  const files = [];
  async function walk(path) {
    const info = await lstat(path);
    if (info.isSymbolicLink() || (!info.isDirectory() && !info.isFile())) throw new Error(`unsupported classifier entry: ${path}`);
    if (info.isDirectory()) {
      for (const name of (await readdir(path)).sort()) await walk(join(path, name));
    } else {
      files.push({ path: relative(root, path), bytes: info.size, sha256: createHash('sha256').update(await readFile(path)).digest('hex') });
    }
  }
  await walk(root);
  return files;
}
const source = await inventory(sourceRoot);
const bundled = await inventory(bundledRoot);
if (JSON.stringify(source) !== JSON.stringify(bundled)) throw new Error('packaged classifier resources differ from the exact source inventory');
await writeFile(output, `${JSON.stringify({ schemaVersion: 1, status: 'passed', sourceRoot, bundledRoot, files: source }, null, 2)}\n`, { mode: 0o600 });
NODE
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_root/Contents/Info.plist")" == "$wrapper_identifier" ]] || exit 1
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleName' "$app_root/Contents/Info.plist")" == 'Dakia Fictional Provider Reader' ]] || exit 1
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app_root/Contents/Info.plist")" == 'dakia-desktop' ]] || exit 1
  for packaged_executable in "$binary" "$bundled_cli"; do
    if otool -L "$packaged_executable" | rg -q 'libonnxruntime' && \
      ! otool -l "$packaged_executable" | rg -F -A2 '@executable_path/../Frameworks' >/dev/null; then
      install_name_tool -add_rpath '@executable_path/../Frameworks' "$packaged_executable"
    fi
  done
  codesign --force --sign - "$bundled_onnx" >/dev/null
  codesign --force --sign - "$bundled_cli" >/dev/null
  codesign --force --sign - "$binary" >/dev/null
  file "$binary" | rg -q 'Mach-O 64-bit executable arm64' || exit 1
  [[ "$(lipo -archs "$binary")" == 'arm64' ]] || exit 1
  file "$bundled_cli" | rg -q 'Mach-O 64-bit executable arm64' || exit 1
  [[ "$(lipo -archs "$bundled_cli")" == 'arm64' ]] || exit 1
  lipo -archs "$bundled_onnx" | tr ' ' '\n' | rg -qx 'arm64' || exit 1
else
  (cd "$repo_root" && CARGO_TARGET_DIR="$build_target" \
    npx tauri dev --config apps/desktop/src-tauri/tauri.conf.json --config "$tauri_config" \
    --features native-mail-safety-fixture --no-watch --runner "$cargo_wrapper") >"$run_root/tauri-build.log" 2>&1
  verify_source_snapshot after-build
  binary="$build_target/debug/dakia-desktop"
  test -x "$binary" || { printf 'Native fixture binary was not built.\n' >&2; exit 1; }
  mkdir -p "$app_root/Contents/MacOS" "$app_root/Contents/Resources/resources" "$app_root/Contents/Frameworks"
  cp "$binary" "$app_root/Contents/MacOS/dakia-desktop"
  cp "$cli_binary" "$app_root/Contents/MacOS/dakia"
  chmod 755 "$app_root/Contents/MacOS/dakia"
  cp -R "$repo_root/apps/desktop/src-tauri/resources/email-classifier-v2" "$app_root/Contents/Resources/resources/"
  if otool -L "$binary" | rg -q 'libonnxruntime'; then
    onnx_source="$repo_root/apps/desktop/src-tauri/frameworks/libonnxruntime.1.23.2.dylib"
    test -f "$onnx_source" || { printf 'ONNX Runtime framework is missing.\n' >&2; exit 1; }
    cp "$onnx_source" "$app_root/Contents/Frameworks/"
    ln -s 'libonnxruntime.1.23.2.dylib' "$app_root/Contents/Frameworks/libonnxruntime.dylib"
    if ! otool -l "$app_root/Contents/MacOS/dakia-desktop" | rg -F -A2 '@executable_path/../Frameworks' >/dev/null; then
      install_name_tool -add_rpath '@executable_path/../Frameworks' "$app_root/Contents/MacOS/dakia-desktop"
    fi
    if ! otool -l "$app_root/Contents/MacOS/dakia" | rg -F -A2 '@executable_path/../Frameworks' >/dev/null; then
      install_name_tool -add_rpath '@executable_path/../Frameworks' "$app_root/Contents/MacOS/dakia"
    fi
  fi
  printf '%s\n' \
    '<?xml version="1.0" encoding="UTF-8"?>' \
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
    "<plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>dakia-desktop</string><key>CFBundleIdentifier</key><string>$wrapper_identifier</string><key>CFBundleName</key><string>Dakia Fictional Provider Reader</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>" > "$app_root/Contents/Info.plist"
fi
codesign --force --deep --sign - "$app_root" >/dev/null
codesign --verify --deep --strict --verbose=2 "$app_root"
if [[ "$packaged" == true ]]; then
  [[ "$("$app_root/Contents/MacOS/dakia" --version)" == "dakia $expected_version" ]] || {
    printf 'Signed packaged CLI version does not match the desktop app.\n' >&2
    exit 1
  }
fi
node --input-type=module - "$app_root" "$run_root/native-runtime-files.json" <<'NODE'
import { createHash } from 'node:crypto';
import { lstat, readdir, readFile, readlink, writeFile } from 'node:fs/promises';
import { join, relative } from 'node:path';
const root = process.argv[2];
const files = [];
async function walk(path) {
  const info = await lstat(path);
  if (info.isDirectory()) { for (const name of (await readdir(path)).sort()) await walk(join(path, name)); }
  else if (info.isSymbolicLink()) files.push({ path: relative(root, path), symlink: await readlink(path) });
  else if (info.isFile()) files.push({ path: relative(root, path), bytes: info.size, sha256: createHash('sha256').update(await readFile(path)).digest('hex') });
}
await walk(root);
await writeFile(process.argv[3], `${JSON.stringify({ schemaVersion: 1, files }, null, 2)}\n`, { mode: 0o600 });
NODE
app_sha256="$(shasum -a 256 "$app_root/Contents/MacOS/dakia-desktop" | awk '{print $1}')"
cli_sha256="$(shasum -a 256 "$app_root/Contents/MacOS/dakia" | awk '{print $1}')"
if [[ "$reuse_approved_identity" == true ]]; then
  verify_approved_bundle_signature "$app_root"
  approved_identity_process_probe="$(approved_identity_process_state)"
  case "$approved_identity_process_probe" in
    absent) ;;
    running)
      printf 'The exact approved fixture executable started unexpectedly before the guarded swap.\n' >&2
      exit 69
      ;;
    *)
      printf 'Could not prove the approved fixture executable is absent before the guarded swap.\n' >&2
      exit 69
      ;;
  esac
  approved_identity_transaction_started=true
  approved_identity_state begin "$approved_identity_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" "$run_root" "$app_root" "$approved_identity_identifier" "$approved_identity_desktop_sha256" "$approved_identity_cli_sha256" > "$run_root/approved-identity-begin.json"
  approved_identity_swapped=true
  chmod 600 "$run_root/approved-identity-begin.json"
  app_root="$approved_identity_app"
  verify_approved_bundle_signature "$app_root"
  [[ "$(shasum -a 256 "$app_root/Contents/MacOS/dakia-desktop" | awk '{print $1}')" == "$app_sha256" ]]
  [[ "$(shasum -a 256 "$app_root/Contents/MacOS/dakia" | awk '{print $1}')" == "$cli_sha256" ]]
  verify_source_snapshot before-launch-services-registration
  launch_services_registration_receipt="$(register_approved_identity_with_launch_services)"
  printf '%s\n' "$launch_services_registration_receipt" > "$run_root/launch-services-registration-path.txt"
  chmod 600 "$run_root/launch-services-registration-path.txt"
  case "$launch_services_registration_receipt" in
    "$run_root/launch-services-registration.json") ;;
    "$run_root/launch-services-registration-unavailable.json")
      launch_services_notification_unavailable=true
      ;;
    *)
      printf 'LaunchServices registration returned an unexpected private receipt.\n' >&2
      exit 1
      ;;
  esac
fi
"$app_root/Contents/MacOS/dakia-desktop" >"$run_root/app.log" 2>&1 &
app_pid=$!
if [[ "$launch_services_notification_unavailable" == true ]]; then
  notification_preflight_warning=false
  for _ in $(seq 1 20); do
    if notification_preflight_warning_is_exact "$run_root/app.log" "$approved_identity_identifier"; then
      notification_preflight_warning=true
      break
    fi
    if ! kill -0 "$app_pid" >/dev/null 2>&1; then break; fi
    sleep 1
  done
  if [[ "$notification_preflight_warning" != true ]]; then
    printf 'The unavailable LaunchServices fallback did not produce the required notification preflight warning.\n' >&2
    exit 1
  fi
fi
acceptance_plan="$run_root/native-acceptance-plan.json"
node --input-type=module - "$acceptance_plan" "$fixture_http_port" "$fixture_tls_probe_port" "$mailto_regressions" <<'NODE'
import { readFile, writeFile } from 'node:fs/promises';

const output = process.argv[2];
const fixtureHttpPort = Number(process.argv[3]);
const fixtureTlsProbePort = Number(process.argv[4]);
const mailtoRegressionsPath = process.argv[5];
if (!Number.isInteger(fixtureHttpPort) || !Number.isInteger(fixtureTlsProbePort) || fixtureHttpPort === fixtureTlsProbePort) {
  throw new Error('Fictional content fixture ports are invalid');
}
if (!mailtoRegressionsPath) throw new Error('Native mailto regression manifest path is missing');
const mailtoRegressionChecks = JSON.parse(await readFile(mailtoRegressionsPath, 'utf8'));
if (
  mailtoRegressionChecks.schemaVersion !== 1 ||
  mailtoRegressionChecks.sourceAccountId !== '62a9320c-04f5-47c2-b4fa-d4952f0b6121' ||
  mailtoRegressionChecks.sourceMessageSubject !== 'Remote content safety check' ||
  mailtoRegressionChecks.recipientInvariant !== 'fictional@example.test' ||
  mailtoRegressionChecks.neverSend !== true ||
  !Array.isArray(mailtoRegressionChecks.cases) ||
  mailtoRegressionChecks.cases.length !== 11
) {
  throw new Error('Native mailto regression manifest is invalid');
}
const forbiddenPaths = [
  '/srcset-image.svg',
  '/svg-external.svg',
  '/remote-style.css',
  '/background.svg',
  '/remote-font.woff2',
  '/remote-video.mp4',
  '/remote-poster.svg',
  '/remote-video-source.mp4',
  '/remote-audio.mp3',
  '/remote-audio-source.mp3',
  '/hostile-script.js',
  '/hostile-frame.html',
  '/form-submit',
  '/script-executed',
  '/frame-executed',
];
await writeFile(
  output,
  `${JSON.stringify(
    {
      schemaVersion: 3,
      messageSubject: 'Remote content safety check',
      listeners: {
        http: { host: '127.0.0.1', port: fixtureHttpPort },
        tlsProbe: { host: '127.0.0.1', port: fixtureTlsProbePort },
      },
      beforeConsent: {
        totalRequests: 0,
        tlsContactAttempts: 0,
        unexpectedHttpTlsContactAttempts: 0,
        totalContacts: 0,
        requirement: 'No HTTP request or TLS ClientHello reaches either owned listener before consent.',
        visibleMarkers: [
          'Native remote-resource matrix marker.',
          'Embedded image loaded locally',
          'Embedded SVG stays inert',
          'Load remote content',
        ],
      },
      afterConsent: {
        orderIndependent: true,
        totalRequests: 5,
        minimumTlsContactAttempts: 1,
        maximumTlsContactAttempts: 8,
        unexpectedHttpTlsContactAttempts: 0,
        completedResponseObservations: 5,
        responseRequirement: 'Every allowed HTTP response finishes without closing early; the redirect is 302 and every image response is 200.',
        settledCounterCheck: 'After transport settles, record HTTP and TLS counts, exercise unchanged parent updates and history cycles, then observe for at least 15 seconds. Both counts must remain exactly unchanged; any growth fails acceptance.',
        tlsRetryPolicy: 'Record WebKit transport retries separately; after settling, unchanged parent updates must not add HTTP requests or new iframe loads.',
        exactAllowedRequests: [
          { listener: 'http', path: '/remote-image.svg', query: '?via=direct' },
          { listener: 'http', path: '/redirect-image', query: '' },
          { listener: 'http', path: '/remote-image.svg', query: '?via=redirect' },
          { listener: 'http', path: '/remote-image.svg', query: '?via=encoded-html' },
          { listener: 'http', path: '/remote-image.svg', query: '?via=event-handler' },
        ],
        forbiddenPaths,
        visibleMarkers: [
          'Remote image loaded after consent',
          'Active-content canaries must remain inert before and after consent.',
        ],
      },
      protocolRelativeBoundary: {
        markup: `//127.0.0.1:${fixtureTlsProbePort}/remote-image.svg?via=protocol-relative`,
        isolatedFromHttpPort: true,
        expectedSanitizedSchemeAfterConsent: 'https',
        expectedEvidence:
          'The dedicated TLS probe listener cannot complete TLS. Record its ClientHello attempts and any WebKit transport retries after consent; require zero contacts on both listeners before consent. No successful HTTPS render is claimed.',
        renderingClaim:
          'No successful protocol-relative image rendering is claimed because the fixture intentionally has no trusted HTTPS endpoint.',
      },
      historySequence: [
        'expand',
        'collapse',
        'expand',
        'collapse',
        'expand',
      ],
      historyMarker:
        'Fictional quoted history remains inside the isolated history iframe.',
      messageSwitch: {
        sequence: [
          'open Remote content safety check',
          'grant consent',
          'open another fictional conversation',
          'return to Remote content safety check',
        ],
        expected:
          'Load remote content is offered again and returning adds no request before a new grant.',
      },
      mailtoRegressionChecks,
      twoAccountChecks: process.env.DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT === '1' ? {
        accountId: '62a9320c-04f5-47c2-b4fa-d4952f0b6122',
        address: 'fictional-reader-two@localhost',
        subject: 'Account two collision safety check',
        collidingUid: 3,
        collidingRfcMessageId: '<fictional-provider-reader-acceptance@localhost>',
        distinctBody: 'Only account two owns this distinct cached body and attachment.',
        distinctAttachment: 'account-two-notes.txt',
        required: 'Unified and scoped search, detached Reader, Reply and Forward must retain account two. Close compose without sending. Use the owned-root proxy control helper with action disconnect; it must acknowledge disabled=true and activeSockets=0 before offline checks. Use action resume, require disabled=false, then verify the actual watcher reconnect. Do not stop Docker; it owns in-memory mailbox data. Physical sleep is not part of this run.',
      } : null,
      attachmentChecks: {
        unsafeFilename: 'quarterly-report.pdf.command',
        safeFilename: 'meeting-notes.txt',
        required:
          'Keep the unsafe warning visible before and after individual save and Save all; verify exact bytes, mode 0600, no execute bits, quarantine, and path containment.',
      },
    },
    null,
    2,
  )}\n`,
  { mode: 0o600 },
);
NODE
launch_metadata="$run_root/sleep-wake-fixture.json"
approved_identity_record_json='null'
expected_main_webview_url='http://127.0.0.1:1420/'
if [[ "$packaged" == true ]]; then
  expected_main_webview_url='tauri://localhost/'
fi
if [[ "$reuse_approved_identity" == true ]]; then
  approved_identity_record_json="$(node -e 'process.stdout.write(JSON.stringify(process.argv[1]))' "$approved_identity_record")"
fi
printf '%s\n' \
  '{' \
  "  \"pid\": $app_pid," \
  "  \"appLog\": \"$run_root/app.log\", " \
  "  \"appExecutable\": \"$app_root/Contents/MacOS/dakia-desktop\", " \
  "  \"appSha256\": \"$app_sha256\", " \
  "  \"cli\": \"$app_root/Contents/MacOS/dakia\", " \
  "  \"cliSha256\": \"$cli_sha256\", " \
  "  \"approvedIdentityReuse\": $reuse_approved_identity," \
  "  \"approvedIdentityRecord\": $approved_identity_record_json," \
  "  \"packaged\": $packaged," \
  "  \"expectedMainWebviewUrl\": \"$expected_main_webview_url\"," \
  "  \"database\": \"$profile_root/dakia.db\", " \
  "  \"imapPort\": $imap_port," \
  "  \"smtpPort\": $smtp_port," \
  "  \"imapProxyPid\": $proxy_pid," \
  "  \"imapBackendPort\": $backend_imap_port" \
  '}' > "$launch_metadata.tmp"
chmod 600 "$launch_metadata.tmp"
mv "$launch_metadata.tmp" "$launch_metadata"

printf '\nFictional provider-to-Reader acceptance is ready.\n'
printf '  app PID: %s\n  app: %s\n  profile: %s\n  Downloads: %s\n' "$app_pid" "$app_root" "$profile_root" "$downloads_root"
printf '  desktop SHA-256: %s\n  bundled CLI: %s\n  bundled CLI SHA-256: %s\n' "$app_sha256" "$app_root/Contents/MacOS/dakia" "$cli_sha256"
if [[ "$reuse_approved_identity" == true ]]; then
  printf '  Approved reused application identity: %s\n' "$wrapper_identifier"
  printf '  Sealed identity record: %s\n' "$approved_identity_record"
  printf '  This run uses a separate macOS 14+ WKWebView data store. A rebuilt ad-hoc executable may still trigger a macOS control prompt.\n'
else
  printf '  Isolated application identity: %s\n' "$wrapper_identifier"
fi
if [[ "$packaged" == true ]]; then
  printf '  Packaged custom-protocol webview: %s\n' "$expected_main_webview_url"
  printf '  Packaged build evidence: %s\n' "$build_mode_evidence"
else
  printf '  Development-server webview: %s\n' "$expected_main_webview_url"
fi
printf '  IMAP: 127.0.0.1:%s with one-run CA %s\n' "$imap_port" "$certificate_root/tls.der"
printf '  Native acceptance plan: %s\n' "$acceptance_plan"
printf '  Native mailto regressions: %s\n' "$mailto_regressions"
printf '  Network counter: http://127.0.0.1:%s/requests (isolated TLS probe 127.0.0.1:%s)\n' "$fixture_http_port" "$fixture_tls_probe_port"
printf '  Sleep/wake observer metadata: %s\n' "$launch_metadata"
printf '  Controlled fictional IMAP: node %s/scripts/native-fictional-imap-proxy.mjs --control-root %s --action disconnect (or resume). Require the acknowledged state before continuing.\n' "$repo_root" "$run_root"
printf '  Bounded sync hold: node %s/scripts/native-fictional-imap-proxy.mjs --control-root %s --action hold --duration-ms 35000. Before clicking Sync, require held=true, holdDurationMs=35000, the exact lastCommandId, and a parseable future holdDeadlineAt with enough time remaining; use --action resume for early release.\n' "$repo_root" "$run_root"
if [[ "$attachment_interruption" == true ]]; then
  printf '  Single attachment interruption: node %s/scripts/native-fictional-attachment-interruption-proxy.mjs control --root %s --action arm-single. Require the acknowledged command ID before saving meeting-notes.txt.\n' "$repo_root" "$run_root"
  printf '  Save-all interruption: node %s/scripts/native-fictional-attachment-interruption-proxy.mjs control --root %s --action arm-all. Require the acknowledged command ID before Save all.\n' "$repo_root" "$run_root"
fi
shared_profile_identity_option=''
if [[ "$reuse_approved_identity" == true ]]; then
  printf -v shared_profile_identity_option ' --approved-identity-record %q' "$approved_identity_record"
fi
printf '  Shared desktop/CLI profile acceptance: node %s/scripts/native-shared-profile-acceptance.mjs --root %s --app-pid %s --app-executable %q --cli %q --profile %s --imap-port %s --imap-proxy-pid %s --expected-app-sha256 %s --expected-cli-sha256 %s --report %s/native-shared-profile-acceptance.json%s\n' "$repo_root" "$run_root" "$app_pid" "$app_root/Contents/MacOS/dakia-desktop" "$app_root/Contents/MacOS/dakia" "$profile_root" "$imap_port" "$proxy_pid" "$app_sha256" "$cli_sha256" "$run_root" "$shared_profile_identity_option"
printf '  The helper never signals a stored PID. Do not stop Docker or interrupt the Mac.\n'
printf 'Reset the counter, open "Remote content safety check", and prove zero requests before consent.\n'
printf 'After consent, compare every recorded path and query with the exact allowlist in the native acceptance plan.\n'
printf 'Expand, collapse, expand, collapse, and expand history; then switch away and back to prove consent resets.\n'
printf 'Open each visible native mailto regression anchor in Dakia compose, verify its recorded seed and safety state, then close without sending.\n'
printf 'Confirm the unsafe warning before and after individual save and Save all.\n'
printf 'Saved files must be %s/quarterly-report.pdf.command and %s/meeting-notes.txt with exact synthetic bytes.\n' "$downloads_root" "$downloads_root"
printf 'Use Ctrl-C after native verification to clean up. Set DAKIA_KEEP_NATIVE_PROVIDER_READER_ARTIFACTS=1 to retain evidence.\n'
if [[ "${DAKIA_NATIVE_MAIL_SAFETY_ACL_PROBE:-}" == "1" ]]; then
  acl_evidence="$run_root/native-acl-acceptance.json"
  acl_ready=false
  for _ in $(seq 1 240); do
    if [[ -f "$acl_evidence" ]]; then acl_ready=true; break; fi
    if ! kill -0 "$app_pid" >/dev/null 2>&1; then break; fi
    sleep 1
  done
  if [[ "$acl_ready" != "true" ]]; then
    printf 'Native ACL acceptance did not produce evidence before the deadline.\n' >&2
    exit 1
  fi
  node --input-type=module - "$acl_evidence" <<'NODE'
import { readFile } from 'node:fs/promises';
const report = JSON.parse(await readFile(process.argv[2], 'utf8'));
if (report.schemaVersion !== 1 || report.status !== 'passed') {
  console.error('Native ACL acceptance failed; inspect the retained report.');
  process.exit(1);
}
console.log(`Native ACL acceptance passed: ${process.argv[2]}`);
NODE
fi
app_exit_status=0
wait "$app_pid" || app_exit_status=$?
app_pid=""
printf '%s\n' "$app_exit_status" > "$run_root/app-exit-status.txt"
chmod 600 "$run_root/app-exit-status.txt"
if [[ "$app_exit_status" -ne 0 ]]; then
  printf 'Native fixture app exited with status %s; skipping the post-exit acceptance handoff.\n' "$app_exit_status" >&2
  exit "$app_exit_status"
fi
if [[ "$hold_approved_candidate_after_exit" == true ]]; then
  approved_identity_process_probe="$(approved_identity_process_state)"
  case "$approved_identity_process_probe" in
    absent) ;;
    running)
      printf 'The approved candidate still has an exact running process after the primary app exit.\n' >&2
      exit 1
      ;;
    *)
      approved_identity_process_probe_failed=true
      printf 'Could not prove the approved candidate is absent after the primary app exit.\n' >&2
      exit 1
      ;;
  esac
  post_exit_release="$run_root/.native-provider-reader-post-exit-complete"
  post_exit_handoff="$run_root/post-exit-acceptance-handoff.json"
  approved_identity_post_exit_hold_active=true
  node --input-type=module - "$post_exit_handoff" "$run_root" "$approved_identity_app" "$approved_identity_record" "$approved_identity_journal" "$app_sha256" "$cli_sha256" "$post_exit_release" <<'NODE'
import { chmod, open, rename } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
const [output, runRoot, app, record, journal, desktopSha256, cliSha256, release] = process.argv.slice(2);
const temporary = `${output}.tmp.${process.pid}.${randomUUID()}`;
const handle = await open(temporary, 'wx', 0o600);
try {
  await handle.writeFile(`${JSON.stringify({
    schemaVersion: 1,
    status: 'waiting-for-post-exit-acceptance',
    runRoot,
    app,
    record,
    journal,
    desktopSha256,
    cliSha256,
    release,
    expectedReleaseContent: 'dakia-native-provider-reader-post-exit-complete-v1\n',
  }, null, 2)}\n`);
  await handle.sync();
} finally {
  await handle.close();
}
await chmod(temporary, 0o600);
await rename(temporary, output);
NODE
  printf 'Primary packaged acceptance exited cleanly. The exact approved candidate remains installed for H016.\n'
  printf '  Post-exit handoff: %s\n' "$post_exit_handoff"
  printf '  After the H016 app exits, create the private release sentinel at: %s\n' "$post_exit_release"
  post_exit_released=false
  for _ in $(seq 1 3600); do
    if [[ -e "$post_exit_release" ]]; then
      post_exit_release_is_valid "$post_exit_release" || exit 1
      approved_identity_process_probe="$(approved_identity_process_state)"
      case "$approved_identity_process_probe" in
        absent) ;;
        running)
          printf 'The post-exit release sentinel appeared while the approved candidate was still running.\n' >&2
          exit 1
          ;;
        *)
          approved_identity_process_probe_failed=true
          printf 'Could not prove the approved candidate is absent at post-exit release.\n' >&2
          exit 1
          ;;
      esac
      post_exit_released=true
      break
    fi
    sleep 1
  done
  if [[ "$post_exit_released" != true ]]; then
    printf 'Timed out waiting for the guarded H016 post-exit release sentinel.\n' >&2
    exit 1
  fi
  approved_identity_post_exit_hold_active=false
  printf 'Post-exit H016 handoff completed; restoring the sealed original application identity.\n'
fi
