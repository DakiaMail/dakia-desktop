#!/usr/bin/env node
import { createHash, randomUUID } from 'node:crypto';
import {
  chmodSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  openSync,
  closeSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  readdirSync,
  renameSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { dirname, isAbsolute, join, relative, sep } from 'node:path';
import { spawn, spawnSync } from 'node:child_process';

const SEALED_PROFILE = '/private/tmp/dakia-forward-upgrade-20261021/h016-faithful-pre-candidate-backup';
const EXPECTED_DB_SHA256 = 'ce9a8ed68a41013a09c3d25e6d867e3d97aa4e579018a183cf388d232fa73330';
const FAITHFUL_REPORT = '/private/tmp/dakia-forward-upgrade-20261021/h016-evidence/faithful-v044/h016-faithful-profile-report.json';
const EXPECTED_FAITHFUL_REPORT_SHA256 = '9373ddf1d36769d642668ebff46c7bca82871494fb8253a63838a01a20c6dd03';
const ACCOUNT_ID = '7c044000-0000-4000-8000-000000000044';
const MESSAGE_ID = 'fictional-v044-upgrade-message';
const SUBJECT = 'Fictional previous-stable upgrade evidence';
const BODY = 'Preserve this fictional message across the forward upgrade.';
const BODY_HTML = '<p>Preserve this fictional message across the forward upgrade.</p>';
const OWNER_MARKER = '.dakia-h016-packaged-gui-upgrade-owner.json';
const BUNDLE_RE = /^dev\.dakia\.fictional-packaged-acl\.[a-z0-9]+$/;
const UUID_RE = /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const HANDOFF_TIMEOUT_MS = 600_000;
const EXPECTED_ACCOUNT_DATA = {
  id: ACCOUNT_ID,
  email: 'upgrade-fixture@example.test',
  account_name: 'Fictional v0.4.4 upgrade',
  display_name: 'Fictional Upgrade',
  provider_id: 'generic',
  auth: { type: 'password', username: 'upgrade-fixture@example.test' },
  imap_host: '127.0.0.1',
  imap_port: 9,
  imap_security: 'tls',
  smtp_host: '127.0.0.1',
  smtp_port: 9,
  smtp_security: 'tls',
  archive_mailbox: 'Archive',
  spam_mailbox: 'Junk',
  enabled: true,
  created_at: '2026-10-21T00:00:00Z',
};
const EXPECTED_APP_META = [
  { key: 'attachment_presentation_cache_version', value: '3' },
  { key: 'catalogue_schema', value: '1' },
  { key: 'classification_policy_version', value: '2' },
  { key: 'profile_schema_version', value: '1' },
  { key: 'sent_correspondents_version', value: '1' },
];
const EXACT_SEALED_INVENTORY = new Map([
  ['.dakia-h016-faithful-v044-profile', '21553b591443dd2e74f12b34cd7b104b54ba12158b335321340f9d37a06469b6'],
  ['.dakia-h016-owned-profile', '24d999df7e2ce9fcf1f47504993bec8377f901cfebfe9aa24312353e91e2aa5f'],
  ['dakia.db', EXPECTED_DB_SHA256],
  ['dakia.db-shm', 'fd4c9fda9cd3f9ae7c962b0ddf37232294d55580e1aa165aa06129b8549389eb'],
  ['dakia.db-wal', 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'],
  ['vault.key', '3b14949d0db6a5036ac8c5f9f131117d04ac0f2370d8bcb2857e14741e74ce2c'],
]);

function fail(message, code = 1) {
  const error = new Error(message);
  error.exitCode = code;
  throw error;
}

function requireValue(condition, message, code = 1) {
  if (!condition) fail(message, code);
}

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600, flag: 'wx' });
}

function overwriteJson(path, value) {
  const temporary = `${path}.tmp.${process.pid}`;
  writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  renameSync(temporary, path);
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    ...options,
  });
  if (result.error) fail(`${command} could not run: ${result.error.message}`, 69);
  if (result.status !== 0) {
    const detail = String(result.stderr || result.stdout || '').trim();
    fail(`${command} failed${detail ? `: ${detail}` : ''}`, result.status || 1);
  }
  return String(result.stdout ?? '');
}

function parseOptions(argv, allowed) {
  const result = {};
  for (let index = 0; index < argv.length; index += 2) {
    const flag = argv[index];
    const value = argv[index + 1];
    requireValue(flag?.startsWith('--') && value !== undefined, 'Every option needs a value.', 64);
    const name = flag.slice(2);
    requireValue(allowed.includes(name) && result[name] === undefined, `Unknown or repeated option: ${flag}`, 64);
    result[name] = value;
  }
  return result;
}

function requireOptions(options, names) {
  for (const name of names) requireValue(options[name], `Missing --${name}.`, 64);
}

function isInside(root, child) {
  const rel = relative(root, child);
  return rel !== '' && !rel.startsWith(`..${sep}`) && rel !== '..' && !isAbsolute(rel);
}

function readPlist(plist, key) {
  return run('/usr/libexec/PlistBuddy', ['-c', `Print :${key}`, plist]).trim();
}

function assertBundleIdentifier(value) {
  requireValue(BUNDLE_RE.test(value), 'The candidate does not use a unique packaged-ACL bundle identifier.');
  return value;
}

function processRows() {
  return run('/bin/ps', ['-ww', '-axo', 'pid=,command=']).split('\n').flatMap((line) => {
    const match = line.match(/^\s*(\d+)\s+(.*)$/);
    return match ? [{ pid: Number(match[1]), command: match[2] }] : [];
  });
}

function commandOwnsExecutable(command, executable) {
  return command === executable || command.startsWith(`${executable} `);
}

function candidateProcesses(executable) {
  return processRows().filter(({ command }) => commandOwnsExecutable(command, executable));
}

function assertCandidateStopped(executable) {
  const live = candidateProcesses(executable);
  requireValue(live.length === 0, `Candidate executable is still running as PID(s): ${live.map(({ pid }) => pid).join(', ')}.`);
}

function requireRegular(path, label) {
  const info = lstatSync(path);
  requireValue(info.isFile() && !info.isSymbolicLink(), `${label} must be a regular file.`);
}

function validateSealedProfile() {
  const sealed = realpathSync(SEALED_PROFILE);
  requireValue(sealed === SEALED_PROFILE, 'The sealed v0.4.4 profile path resolved unexpectedly.');
  const names = readdirSync(sealed).sort();
  requireValue(names.join('\n') === [...EXACT_SEALED_INVENTORY.keys()].sort().join('\n'), 'The sealed v0.4.4 inventory changed.');
  for (const [name, expected] of EXACT_SEALED_INVENTORY) {
    const path = join(sealed, name);
    requireRegular(path, `Sealed fixture ${name}`);
    requireValue(sha256(path) === expected, `The sealed fixture hash changed for ${name}.`);
  }
  requireValue(readFileSync(join(sealed, '.dakia-h016-faithful-v044-profile'), 'utf8').trim() === 'dakia-h016-faithful-v044-profile-v1', 'The faithful-profile marker changed.');
  requireValue(readFileSync(join(sealed, '.dakia-h016-owned-profile'), 'utf8').trim() === 'dakia-h016-private-forward-upgrade-v1', 'The private-profile marker changed.');
  return sealed;
}

function validateFaithfulReport() {
  requireRegular(FAITHFUL_REPORT, 'Faithful v0.4.4 provenance report');
  requireValue(sha256(FAITHFUL_REPORT) === EXPECTED_FAITHFUL_REPORT_SHA256, 'The faithful v0.4.4 provenance report hash changed.');
  const report = json(FAITHFUL_REPORT);
  requireValue(report.schemaVersion === 1 && report.publishedSource?.tag === 'v0.4.4' && report.publishedSource?.commit === '8af5c397d81cc9aa60ddaca9b5eb15ecac9f6255', 'The faithful fixture source provenance changed.');
  requireValue(report.faithfulFixture?.sealed === SEALED_PROFILE && report.assertions?.faithfulSealedInputUnchangedDuringProbe === true, 'The faithful fixture report no longer binds the sealed profile.');
  return report;
}

function sqliteJson(db, statement, { immutable = false } = {}) {
  const uri = `file:${db}?${immutable ? 'immutable=1' : 'mode=ro'}`;
  const output = run('sqlite3', ['-readonly', '-cmd', '.timeout 5000', '-json', uri, statement]).trim();
  return output ? JSON.parse(output) : [];
}

function fileInventory(root) {
  const files = [];
  function walk(path) {
    const info = lstatSync(path);
    requireValue(!info.isSymbolicLink(), `Profile contains a symbolic link: ${path}`);
    if (info.isDirectory()) {
      for (const name of readdirSync(path).sort()) walk(join(path, name));
    } else if (info.isFile()) {
      files.push({ path: relative(root, path), bytes: info.size, mode: info.mode & 0o777, sha256: sha256(path) });
    } else {
      fail(`Profile contains an unsupported filesystem entry: ${path}`);
    }
  }
  walk(root);
  return files;
}

function databaseSnapshot(profile, { immutable = false } = {}) {
  const db = join(profile, 'dakia.db');
  requireRegular(db, 'Profile database');
  return {
    capturedAt: new Date().toISOString(),
    profile,
    database: { path: db, sha256: sha256(db), bytes: statSync(db).size },
    inventory: fileInventory(profile),
    sqliteReadMode: immutable ? 'immutable-sealed-fixture' : 'wal-aware-read-only',
    integrity: sqliteJson(db, 'PRAGMA integrity_check;', { immutable }),
    foreignKeys: sqliteJson(db, 'PRAGMA foreign_key_check;', { immutable }),
    userVersion: sqliteJson(db, 'PRAGMA user_version;', { immutable }),
    applicationId: sqliteJson(db, 'PRAGMA application_id;', { immutable }),
    accounts: sqliteJson(db, 'SELECT id,email,data,created_at FROM accounts ORDER BY id;', { immutable }),
    credentials: sqliteJson(db, 'SELECT count(*) AS credential_count FROM credentials;', { immutable }),
    messages: sqliteJson(db, 'SELECT id,account_id,mailbox,uid,subject,is_read,is_flagged,content_state,body_text,body_html FROM messages ORDER BY id;', { immutable }),
    starredBodies: sqliteJson(db, 'SELECT message_id,body_text,body_html,attachment_presentation_version,cached_at FROM starred_message_bodies ORDER BY message_id;', { immutable }),
    attachments: sqliteJson(db, 'SELECT count(*) AS attachment_count FROM attachments;', { immutable }),
    mailboxCatalogue: sqliteJson(db, 'SELECT account_id,mailbox,remote_name,uid_validity FROM mailbox_catalog_state ORDER BY account_id,mailbox;', { immutable }),
    mailboxSync: sqliteJson(db, 'SELECT account_id,mailbox,initialized_at,highest_uid,uid_validity FROM mailbox_sync_state ORDER BY account_id,mailbox;', { immutable }),
    sendAttempts: sqliteJson(db, 'SELECT count(*) AS send_attempt_count FROM send_attempts;', { immutable }),
    deletedAccountTombstones: sqliteJson(db, 'SELECT count(*) AS tombstone_count FROM deleted_account_tombstones;', { immutable }),
    appMeta: sqliteJson(db, 'SELECT key,value FROM app_meta ORDER BY key;', { immutable }),
  };
}

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}

function assertLogicalProfile(snapshot, { allowReadTransition = false } = {}) {
  requireValue(snapshot.integrity.length === 1 && snapshot.integrity[0].integrity_check === 'ok', 'SQLite integrity check failed.');
  requireValue(snapshot.foreignKeys.length === 0, 'SQLite foreign-key check failed.');
  requireValue(snapshot.credentials.length === 1 && snapshot.credentials[0].credential_count === 0, 'The fictional profile unexpectedly contains credentials.');
  requireValue(snapshot.attachments.length === 1 && snapshot.attachments[0].attachment_count === 0, 'The fictional profile unexpectedly contains attachments.');
  requireValue(snapshot.accounts.length === 1, 'The fictional profile must contain exactly one account.');
  const account = snapshot.accounts[0];
  const data = JSON.parse(account.data);
  requireValue(account.id === ACCOUNT_ID && account.email === 'upgrade-fixture@example.test' && account.created_at === '2026-10-21T00:00:00Z', 'The fictional account identity changed.');
  requireValue(canonicalJson(data) === canonicalJson(EXPECTED_ACCOUNT_DATA), 'The fictional account payload changed.');
  requireValue(snapshot.messages.length === 1, 'The fictional message catalogue changed cardinality.');
  const message = snapshot.messages[0];
  requireValue(message.id === MESSAGE_ID && message.account_id === ACCOUNT_ID && message.mailbox === 'INBOX' && message.uid === 44, 'The fictional message identity changed.');
  requireValue(message.subject === SUBJECT && message.is_flagged === 1 && message.content_state === 'complete', 'The fictional subject, star, or content state changed.');
  requireValue(message.is_read === 0 || (allowReadTransition && message.is_read === 1), 'The unread state changed before UI interaction or to an invalid value.');
  requireValue(message.body_text === '' && message.body_html === null, 'The v0.4.4 transient catalogue-body shape changed.');
  requireValue(snapshot.starredBodies.length === 1, 'The v0.4.4 starred-body cache changed cardinality.');
  const starred = snapshot.starredBodies[0];
  requireValue(starred.message_id === MESSAGE_ID && starred.body_text === BODY && starred.body_html === BODY_HTML && starred.attachment_presentation_version === 3, 'The Reader-readable v0.4.4 starred body changed.');
  requireValue(canonicalJson(snapshot.appMeta) === canonicalJson(EXPECTED_APP_META), 'The profile migration markers are incomplete or unexpected.');
  requireValue(snapshot.mailboxCatalogue.length === 1 && snapshot.mailboxCatalogue[0].account_id === ACCOUNT_ID && snapshot.mailboxCatalogue[0].mailbox === 'INBOX' && snapshot.mailboxCatalogue[0].remote_name === 'INBOX' && snapshot.mailboxCatalogue[0].uid_validity === 44, 'The mailbox UIDVALIDITY epoch changed.');
  requireValue(snapshot.mailboxSync.length === 0, 'The fixture unexpectedly gained a mailbox sync claim.');
  requireValue(snapshot.sendAttempts.length === 1 && snapshot.sendAttempts[0].send_attempt_count === 0, 'The fixture unexpectedly gained a send attempt.');
  requireValue(snapshot.deletedAccountTombstones.length === 1 && snapshot.deletedAccountTombstones[0].tombstone_count === 0, 'The fixture unexpectedly gained an account tombstone.');
}

function applicationSupportRoot() {
  const home = process.env.HOME;
  requireValue(home && home.startsWith('/Users/'), 'The existing HOME is not a normal macOS user home.');
  const username = run('/usr/bin/id', ['-un']).trim();
  requireValue(/^[A-Za-z0-9._-]+$/.test(username), 'The current macOS username is invalid.');
  const directoryRecord = run('/usr/bin/dscacheutil', ['-q', 'user', '-a', 'name', username]).trim();
  const match = directoryRecord.match(/^dir:\s+(.+)$/m);
  requireValue(match && realpathSync(home) === realpathSync(match[1]), 'The inherited HOME does not match the current macOS account directory.');
  const root = realpathSync(join(home, 'Library', 'Application Support'));
  requireValue(root === join(realpathSync(home), 'Library', 'Application Support'), 'Application Support resolved outside the existing user home.');
  return root;
}

function validateBundleInventory(appRoot, inventoryFile) {
  const inventory = json(inventoryFile);
  requireValue(inventory.schemaVersion === 1 && realpathSync(inventory.root) === appRoot && Array.isArray(inventory.files), 'App inventory does not bind the candidate bundle.');
  const expectedPaths = new Set();
  for (const entry of inventory.files) {
    requireValue(typeof entry.path === 'string' && entry.path && !isAbsolute(entry.path) && !entry.path.split('/').includes('..'), 'App inventory contains an unsafe path.');
    const path = join(appRoot, entry.path);
    expectedPaths.add(entry.path);
    const info = lstatSync(path);
    if ('symlink' in entry) requireValue(info.isSymbolicLink() && readlinkSync(path) === entry.symlink, `Bundle symlink changed: ${entry.path}`);
    else requireValue(info.isFile() && info.size === entry.bytes && (info.mode & 0o777) === entry.mode && sha256(path) === entry.sha256, `Bundle file changed: ${entry.path}`);
  }
  const actualPaths = new Set();
  function walk(path) {
    const info = lstatSync(path);
    if (info.isDirectory()) for (const name of readdirSync(path).sort()) walk(join(path, name));
    else actualPaths.add(relative(appRoot, path));
  }
  walk(appRoot);
  requireValue(canonicalJson([...actualPaths].sort()) === canonicalJson([...expectedPaths].sort()), 'The candidate bundle inventory gained or lost entries.');
}

function validateCandidate(options, { destinationMustBeAbsent = true, candidateMustBeStopped = true } = {}) {
  const expectedSha = options['expected-app-sha'];
  const expectedIdentifier = options['expected-bundle-identifier'];
  const expectedProvenanceSha = options['expected-provenance-sha'];
  const expectedInventorySha = options['expected-inventory-sha'];
  const expectedSourceDigest = options['expected-source-digest'];
  for (const [value, label] of [[expectedSha, 'application'], [expectedProvenanceSha, 'provenance'], [expectedInventorySha, 'inventory'], [expectedSourceDigest, 'source']]) requireValue(/^[a-f0-9]{64}$/.test(value), `Expected ${label} SHA-256 is malformed.`, 64);
  assertBundleIdentifier(expectedIdentifier);
  const provenanceFile = realpathSync(options.provenance);
  requireRegular(provenanceFile, 'Artifact provenance');
  requireValue(sha256(provenanceFile) === expectedProvenanceSha, 'Artifact provenance hash does not match the independently captured value.');
  const inventoryFile = realpathSync(options.inventory);
  requireRegular(inventoryFile, 'App inventory');
  requireValue(dirname(inventoryFile) === dirname(provenanceFile) && sha256(inventoryFile) === expectedInventorySha, 'App inventory is not the independently captured sibling artifact.');
  const provenance = json(provenanceFile);
  requireValue(provenance.schemaVersion === 1, 'Unsupported artifact provenance schema.');
  const appRoot = realpathSync(provenance.appRoot);
  const executable = realpathSync(provenance.executable?.path);
  requireValue(isInside(appRoot, executable), 'The candidate executable is outside its application bundle.');
  requireValue(executable === join(appRoot, 'Contents', 'MacOS', 'dakia-desktop'), 'The candidate executable path is unexpected.');
  requireRegular(executable, 'Candidate executable');
  const actualSha = sha256(executable);
  requireValue(actualSha === expectedSha && provenance.executable.sha256 === expectedSha, 'The candidate executable hash does not match both supplied and recorded provenance.');
  const identifier = assertBundleIdentifier(provenance.bundleIdentifier);
  requireValue(identifier === expectedIdentifier, 'Bundle identifier does not match the independently captured compiled identifier.');
  requireValue(provenance.overlay?.identifier === identifier, 'Compiled overlay identifier disagrees with artifact provenance.');
  requireValue(provenance.source?.source?.digest === expectedSourceDigest, 'Candidate source snapshot digest disagrees with the independently captured value.');
  const plist = join(appRoot, 'Contents', 'Info.plist');
  requireRegular(plist, 'Candidate Info.plist');
  requireValue(readPlist(plist, 'CFBundleIdentifier') === identifier, 'Info.plist bundle identifier disagrees with provenance.');
  requireValue(readPlist(plist, 'CFBundleExecutable') === 'dakia-desktop', 'Info.plist executable disagrees with provenance.');
  requireValue(run('/usr/bin/lipo', ['-archs', executable]).trim() === 'arm64', 'The private packaged candidate is not arm64-only.');
  run('/usr/bin/codesign', ['--verify', '--deep', '--strict', appRoot]);
  validateBundleInventory(appRoot, inventoryFile);
  requireValue(String(provenance.releaseBoundary).includes('not Developer ID signed'), 'The helper only accepts the documented private ad-hoc fixture release boundary.');
  const supportRoot = applicationSupportRoot();
  const destination = join(supportRoot, identifier);
  requireValue(dirname(destination) === supportRoot, 'The computed profile destination escaped Application Support.');
  if (candidateMustBeStopped) assertCandidateStopped(executable);
  if (destinationMustBeAbsent) requireValue(!existsSync(destination), `The normal application-data destination already exists: ${destination}`);
  return { provenanceFile, provenanceSha256: expectedProvenanceSha, inventoryFile, inventorySha256: expectedInventorySha, sourceDigest: expectedSourceDigest, provenance, appRoot, executable, appSha256: actualSha, identifier, supportRoot, destination };
}

function preflight(options) {
  requireOptions(options, ['provenance', 'inventory', 'expected-app-sha', 'expected-provenance-sha', 'expected-inventory-sha', 'expected-bundle-identifier', 'expected-source-digest']);
  const sealed = validateSealedProfile();
  validateFaithfulReport();
  const before = databaseSnapshot(sealed, { immutable: true });
  assertLogicalProfile(before);
  requireValue(before.database.sha256 === EXPECTED_DB_SHA256, 'The sealed database hash changed.');
  const candidate = validateCandidate(options);
  return {
    schemaVersion: 1,
    status: 'passed',
    mode: 'read-only-preflight',
    candidate: {
      provenance: candidate.provenanceFile,
      provenanceSha256: candidate.provenanceSha256,
      inventory: candidate.inventoryFile,
      inventorySha256: candidate.inventorySha256,
      sourceDigest: candidate.sourceDigest,
      appRoot: candidate.appRoot,
      executable: candidate.executable,
      sha256: candidate.appSha256,
      bundleIdentifier: candidate.identifier,
    },
    sealedProfile: { path: sealed, dbSha256: before.database.sha256 },
    destination: candidate.destination,
    destinationAbsent: true,
    candidateStopped: true,
    releaseBoundary: 'Private ad-hoc fixture-enabled arm64 app. This is not Developer ID, notarization, stapling, updater signing, installation, or publication evidence.',
  };
}

function scrubDakiaEnvironment(environment) {
  return Object.fromEntries(Object.entries(environment).filter(([name]) =>
    !name.startsWith('DAKIA_NATIVE_MAIL_SAFETY_') &&
    !name.startsWith('DAKIA_RELEASE_SMOKE_') &&
    name !== 'DAKIA_DATA_DIR'));
}

function safeEvidenceRoot(path) {
  const resolved = realpathSync(path);
  const match = resolved.match(/^\/private\/tmp\/dakia-h016-packaged-upgrade\.([a-f0-9-]+)$/);
  requireValue(match && UUID_RE.test(match[1]), 'Evidence root is outside the exact owned private H016 UUID prefix.');
  requireValue(readFileSync(join(resolved, '.dakia-h016-evidence-owner'), 'utf8').trim() === 'dakia-h016-packaged-gui-upgrade-evidence-v1', 'Evidence ownership marker is missing.');
  return resolved;
}

function assertControlIdentity(root, control, supportRoot) {
  requireValue(control?.schemaVersion === 1 && UUID_RE.test(control.evidenceId) && root === `/private/tmp/dakia-h016-packaged-upgrade.${control.evidenceId}`, 'Evidence control identity is malformed.');
  assertBundleIdentifier(control.bundleIdentifier);
  requireValue(control.sealedProfile === SEALED_PROFILE && control.sealedDatabaseSha256 === EXPECTED_DB_SHA256, 'Evidence control no longer binds the faithful sealed profile.');
  requireValue(control.destination === join(supportRoot, control.bundleIdentifier) && dirname(control.destination) === supportRoot, 'Evidence control destination is not an immediate Application Support child.');
}

function assertMarkerIdentity(marker, control, expected) {
  requireValue(marker?.schemaVersion === 1 && marker.evidenceId === control.evidenceId && marker.bundleIdentifier === control.bundleIdentifier && marker.destination === expected && marker.executableSha256 === control.executable.sha256 && marker.sealedDatabaseSha256 === control.sealedDatabaseSha256 && marker.provenanceSha256 === control.provenanceSha256 && marker.inventorySha256 === control.inventorySha256 && marker.sourceDigest === control.sourceDigest, 'Owned application-data marker does not match the complete evidence control identity.');
}

function loadControlForCleanup(evidenceRoot) {
  const root = safeEvidenceRoot(evidenceRoot);
  const control = json(join(root, 'control.json'));
  const supportRoot = applicationSupportRoot();
  assertControlIdentity(root, control, supportRoot);
  return { root, control };
}

function loadControl(evidenceRoot) {
  const { root, control } = loadControlForCleanup(evidenceRoot);
  const candidate = validateCandidate({
    provenance: control.provenance,
    inventory: control.inventory,
    'expected-app-sha': control.executable?.sha256,
    'expected-provenance-sha': control.provenanceSha256,
    'expected-inventory-sha': control.inventorySha256,
    'expected-bundle-identifier': control.bundleIdentifier,
    'expected-source-digest': control.sourceDigest,
  }, { destinationMustBeAbsent: false, candidateMustBeStopped: false });
  requireValue(candidate.executable === control.executable.path && candidate.appRoot === control.appRoot, 'Evidence control candidate paths changed.');
  return { root, control };
}

function validateOwnedDestination(control) {
  const supportRoot = applicationSupportRoot();
  assertBundleIdentifier(control.bundleIdentifier);
  const expected = join(supportRoot, control.bundleIdentifier);
  requireValue(control.destination === expected, 'Recorded destination is not the exact normal application-data path.');
  requireValue(existsSync(expected), 'Owned application-data profile is absent.');
  const info = lstatSync(expected);
  requireValue(info.isDirectory() && !info.isSymbolicLink(), 'Owned application-data profile is not a real directory.');
  const markerPath = join(expected, OWNER_MARKER);
  requireRegular(markerPath, 'Owned application-data marker');
  const marker = json(markerPath);
  assertMarkerIdentity(marker, control, expected);
  return expected;
}

function prepare(options) {
  const result = preflight(options);
  const candidate = validateCandidate(options);
  const sealed = validateSealedProfile();
  const evidenceId = randomUUID();
  const evidenceRoot = `/private/tmp/dakia-h016-packaged-upgrade.${evidenceId}`;
  mkdirSync(evidenceRoot, { mode: 0o700 });
  writeFileSync(join(evidenceRoot, '.dakia-h016-evidence-owner'), 'dakia-h016-packaged-gui-upgrade-evidence-v1\n', { mode: 0o600, flag: 'wx' });
  const stage = join(evidenceRoot, 'profile-stage');
  requireValue(!existsSync(stage), 'The unique staging directory already exists.');
  mkdirSync(stage, { mode: 0o700 });
  const marker = {
    schemaVersion: 1,
    evidenceId,
    bundleIdentifier: candidate.identifier,
    destination: candidate.destination,
    executableSha256: candidate.appSha256,
    sealedDatabaseSha256: EXPECTED_DB_SHA256,
    provenanceSha256: candidate.provenanceSha256,
    inventorySha256: candidate.inventorySha256,
    sourceDigest: candidate.sourceDigest,
    createdAt: new Date().toISOString(),
  };
  writeJson(join(stage, OWNER_MARKER), marker);
  try {
    for (const name of EXACT_SEALED_INVENTORY.keys()) copyFileSync(join(sealed, name), join(stage, name), 0);
    chmodSync(stage, 0o700);
    for (const name of [...EXACT_SEALED_INVENTORY.keys(), OWNER_MARKER]) chmodSync(join(stage, name), 0o600);
    const staged = databaseSnapshot(stage);
    assertLogicalProfile(staged);
    requireValue(staged.database.sha256 === EXPECTED_DB_SHA256, 'Prepared clone does not match the sealed v0.4.4 database.');
    const control = {
      schemaVersion: 1,
      evidenceId,
      preparedAt: new Date().toISOString(),
      provenance: candidate.provenanceFile,
      provenanceSha256: candidate.provenanceSha256,
      inventory: candidate.inventoryFile,
      inventorySha256: candidate.inventorySha256,
      sourceDigest: candidate.sourceDigest,
      appRoot: candidate.appRoot,
      executable: { path: candidate.executable, sha256: candidate.appSha256 },
      bundleIdentifier: candidate.identifier,
      destination: candidate.destination,
      sealedProfile: sealed,
      sealedDatabaseSha256: EXPECTED_DB_SHA256,
      userHomePreserved: process.env.HOME,
      environmentPolicy: 'Preserve the inherited environment and remove every DAKIA_NATIVE_MAIL_SAFETY_* and DAKIA_RELEASE_SMOKE_* variable plus DAKIA_DATA_DIR. HOME is never replaced.',
      releaseBoundary: result.releaseBoundary,
    };
    writeJson(join(evidenceRoot, 'control.json'), control);
    writeJson(join(evidenceRoot, 'preflight.json'), result);
    writeJson(join(evidenceRoot, 'profile-before-launch.json'), staged);
    assertCandidateStopped(candidate.executable);
    requireValue(!existsSync(candidate.destination), 'The application-data destination appeared during preparation.');
    renameSync(stage, candidate.destination);
    validateOwnedDestination(control);
  } catch (error) {
    const recovery = { schemaVersion: 1, status: 'preparation-failed', stage, destination: candidate.destination, evidenceRoot, error: error.message };
    overwriteJson(join(evidenceRoot, 'preparation-failure.json'), recovery);
    throw error;
  }
  return { schemaVersion: 1, status: 'prepared', evidenceRoot, destination: candidate.destination, executable: candidate.executable, executableSha256: candidate.appSha256, bundleIdentifier: candidate.identifier };
}

function readLaunch(root) {
  const launch = json(join(root, 'launch.json'));
  requireValue(Number.isInteger(launch.pid) && launch.pid > 1 && UUID_RE.test(launch.evidenceId), 'Launch evidence has no valid identity.');
  return launch;
}

function validTime(value) {
  const time = Date.parse(value);
  requireValue(Number.isFinite(time), 'Evidence timestamp is invalid.');
  return time;
}

function assertLaunchBinding(control, launch) {
  requireValue(launch.evidenceId === control.evidenceId && launch.executable?.path === control.executable.path && launch.executable?.sha256 === control.executable.sha256 && launch.bundleIdentifier === control.bundleIdentifier && launch.normalApplicationDataPath === control.destination, 'Launch evidence does not bind the control identity.');
  validTime(launch.startedAt);
}

function assertSuccessfulExit(control, launch, exit) {
  requireValue(exit?.schemaVersion === 1 && exit.evidenceId === control.evidenceId && exit.pid === launch.pid && exit.executable?.path === control.executable.path && exit.executable?.sha256 === control.executable.sha256 && exit.code === 0 && exit.signal === null && exit.error === null, 'Exit evidence is absent, failed, signaled, or belongs to another run.');
}

function processForPid(pid) {
  return processRows().find((entry) => entry.pid === pid);
}

function assertLiveCandidate(control, pid) {
  const row = processForPid(pid);
  requireValue(row && commandOwnsExecutable(row.command, control.executable.path), 'The recorded PID is not the exact private candidate executable.');
  requireValue(sha256(control.executable.path) === control.executable.sha256, 'The running candidate executable hash changed.');
  return row;
}

function internetSockets(pid) {
  const result = spawnSync('/usr/sbin/lsof', ['-nP', '-a', '-p', String(pid), '-i', '-F', 'pcnT'], { encoding: 'utf8' });
  if (result.error) fail(`lsof could not run: ${result.error.message}`, 69);
  if (![0, 1].includes(result.status)) fail(`lsof failed: ${String(result.stderr).trim()}`);
  return String(result.stdout || '').split('\n').filter(Boolean);
}

function profileHandles(pid, destination) {
  const result = spawnSync('/usr/sbin/lsof', ['-nP', '-a', '-p', String(pid), '+D', destination, '-F', 'pcn'], { encoding: 'utf8' });
  if (result.error) fail(`lsof could not inspect the profile: ${result.error.message}`, 69);
  if (![0, 1].includes(result.status)) fail(`lsof profile inspection failed: ${String(result.stderr).trim()}`);
  return String(result.stdout || '').split('\n').filter(Boolean);
}

function allProfileHandles(destination) {
  const result = spawnSync('/usr/sbin/lsof', ['-nP', '+D', destination, '-F', 'pcn'], { encoding: 'utf8' });
  if (result.error) fail(`lsof could not inspect profile ownership: ${result.error.message}`, 69);
  if (![0, 1].includes(result.status)) fail(`lsof profile ownership check failed: ${String(result.stderr).trim()}`);
  return String(result.stdout || '').split('\n').filter(Boolean);
}

function attestRuntime(options) {
  requireOptions(options, ['evidence-root']);
  const { root, control } = loadControl(options['evidence-root']);
  const destination = validateOwnedDestination(control);
  const launch = readLaunch(root);
  assertLaunchBinding(control, launch);
  const row = assertLiveCandidate(control, launch.pid);
  const sockets = internetSockets(launch.pid);
  const socketNames = sockets.filter((line) => line.startsWith('n')).map((line) => line.slice(1));
  for (const name of socketNames) {
    requireValue(/(?:^|->)127\.0\.0\.1:9(?:$|\s)/.test(name) || /(?:^|->)\[::1\]:9(?:$|\s)/.test(name), `Candidate used an unexpected Internet socket: ${name}`);
  }
  const handles = profileHandles(launch.pid, destination);
  requireValue(handles.some((line) => line === `n${join(destination, 'dakia.db')}` || line === `n${join(destination, 'dakia.db-wal')}`), 'The exact candidate PID has not opened the owned profile database.');
  const report = {
    schemaVersion: 1,
    status: 'passed',
    capturedAt: new Date().toISOString(),
    evidenceId: control.evidenceId,
    pid: launch.pid,
    command: row.command,
    executable: control.executable,
    bundleIdentifier: control.bundleIdentifier,
    destination,
    profileHandles: handles,
    internetSockets: socketNames,
    networkPolicy: 'No Internet socket is allowed except closed fictional account endpoint 127.0.0.1:9 or ::1:9.',
  };
  overwriteJson(join(root, 'runtime-attestation.json'), report);
  return report;
}

function recordCua(options) {
  requireOptions(options, ['evidence-root', 'observed-url', 'visible-subject', 'visible-body', 'unread-visible', 'star-visible']);
  const { root, control } = loadControl(options['evidence-root']);
  const launch = readLaunch(root);
  assertLaunchBinding(control, launch);
  assertLiveCandidate(control, launch.pid);
  const runtime = json(join(root, 'runtime-attestation.json'));
  requireValue(runtime.status === 'passed' && runtime.evidenceId === control.evidenceId && runtime.pid === launch.pid && runtime.executable?.path === control.executable.path && runtime.executable?.sha256 === control.executable.sha256 && runtime.bundleIdentifier === control.bundleIdentifier && runtime.destination === control.destination && validTime(runtime.capturedAt) >= validTime(launch.startedAt), 'Runtime evidence is stale or belongs to another run.');
  requireValue(options['observed-url'] === 'tauri://localhost/', 'CUA did not observe the required packaged WebKit URL.');
  requireValue(options['visible-subject'] === SUBJECT, 'CUA subject observation is not exact.');
  requireValue(options['visible-body'] === BODY, 'CUA cached-body observation is not exact.');
  requireValue(options['unread-visible'] === 'yes' && options['star-visible'] === 'yes', 'CUA did not observe the preserved unread and starred state.');
  const report = {
    schemaVersion: 1,
    status: 'recorded-parent-observation',
    capturedAt: new Date().toISOString(),
    evidenceId: control.evidenceId,
    limitation: 'This record validates the parent-supplied CUA observation. The parent must retain the actual accessibility/screenshot evidence and exact PID/path binding.',
    pid: launch.pid,
    executable: control.executable,
    bundleIdentifier: control.bundleIdentifier,
    webviewUrl: options['observed-url'],
    visible: { subject: options['visible-subject'], body: options['visible-body'], unread: true, starred: true },
  };
  overwriteJson(join(root, 'cua-observation.json'), report);
  return report;
}

function moveOwnedProfile(root, control, status) {
  const destination = validateOwnedDestination(control);
  assertCandidateStopped(control.executable.path);
  const openHandles = allProfileHandles(destination);
  requireValue(openHandles.length === 0, `Refusing to move the owned profile while a process still has it open: ${openHandles.join(' ')}`);
  const marker = json(join(destination, OWNER_MARKER));
  requireValue(marker.evidenceId === control.evidenceId && marker.bundleIdentifier === control.bundleIdentifier, 'Refusing to move a profile without the exact ownership identity.');
  const target = join(root, status === 'passed' ? 'profile-after' : 'profile-incomplete');
  requireValue(!existsSync(target), 'Evidence already contains a moved profile.');
  renameSync(destination, target);
  requireValue(!existsSync(destination) && existsSync(target), 'Owned profile move did not complete atomically.');
  return target;
}

function sealAfterExit(root, control) {
  assertCandidateStopped(control.executable.path);
  validateCandidate({ provenance: control.provenance, inventory: control.inventory, 'expected-app-sha': control.executable.sha256, 'expected-provenance-sha': control.provenanceSha256, 'expected-inventory-sha': control.inventorySha256, 'expected-bundle-identifier': control.bundleIdentifier, 'expected-source-digest': control.sourceDigest }, { destinationMustBeAbsent: false });
  const launch = readLaunch(root);
  assertLaunchBinding(control, launch);
  const exit = json(join(root, 'exit.json'));
  assertSuccessfulExit(control, launch, exit);
  const startedAt = validTime(launch.startedAt);
  const exitedAt = validTime(exit.exitedAt);
  requireValue(exitedAt >= startedAt, 'Exit evidence predates launch.');
  const runtime = json(join(root, 'runtime-attestation.json'));
  const cua = json(join(root, 'cua-observation.json'));
  requireValue(runtime.status === 'passed' && runtime.evidenceId === control.evidenceId && runtime.pid === launch.pid && runtime.executable?.path === control.executable.path && runtime.executable?.sha256 === control.executable.sha256 && runtime.bundleIdentifier === control.bundleIdentifier && runtime.destination === control.destination, 'Runtime evidence is stale or belongs to another run.');
  requireValue(cua.status === 'recorded-parent-observation' && cua.evidenceId === control.evidenceId && cua.pid === launch.pid && cua.executable?.path === control.executable.path && cua.executable?.sha256 === control.executable.sha256 && cua.bundleIdentifier === control.bundleIdentifier && cua.webviewUrl === 'tauri://localhost/', 'CUA evidence is stale or belongs to another run.');
  for (const captured of [runtime.capturedAt, cua.capturedAt]) {
    const at = validTime(captured);
    requireValue(at >= startedAt && at <= exitedAt, 'Runtime or CUA evidence lies outside the launch interval.');
  }
  const startup = json(join(root, 'profile-after-startup-before-cua.json'));
  assertLogicalProfile(startup);
  const destination = validateOwnedDestination(control);
  const after = databaseSnapshot(destination);
  assertLogicalProfile(after, { allowReadTransition: true });
  writeJson(join(root, 'profile-after-exit.json'), after);
  const faithfulReport = validateFaithfulReport();
  validateSealedProfile();
  const moved = moveOwnedProfile(root, control, 'passed');
  const final = {
    schemaVersion: 1,
    status: 'passed',
    sealedAt: new Date().toISOString(),
    candidate: { executable: control.executable, bundleIdentifier: control.bundleIdentifier },
    sourceRelease: faithfulReport.publishedSource,
    faithfulInput: { path: control.sealedProfile, dbSha256: control.sealedDatabaseSha256, provenanceReport: FAITHFUL_REPORT, provenanceReportSha256: EXPECTED_FAITHFUL_REPORT_SHA256, completeInventoryRevalidated: true },
    profile: { normalApplicationDataPath: control.destination, removedFromApplicationSupport: !existsSync(control.destination), preservedAt: moved },
    logicalState: {
      subject: SUBJECT,
      cachedBody: BODY,
      starred: true,
      unreadPreservedBeforeCua: json(join(root, 'profile-after-startup-before-cua.json')).messages[0].is_read === 0,
      readAfterCua: after.messages[0].is_read === 1,
    },
    runtimeEvidence: { path: 'runtime-attestation.json', sha256: sha256(join(root, 'runtime-attestation.json')) },
    cuaObservation: { path: 'cua-observation.json', sha256: sha256(join(root, 'cua-observation.json')), limitation: cua.limitation },
    exit,
    releaseBoundary: control.releaseBoundary,
    limitations: [
      'The CLI show command is provider-backed and is not used as an offline cache oracle.',
      'This private ad-hoc fixture app is not Developer ID, notarization, stapling, updater signing, installation, or publication evidence.',
      'The CUA record is parent-supplied and must be paired with the parent run actual accessibility or screenshot evidence.',
      'The runtime socket inventory is point-in-time evidence. It cannot prove that no short-lived connection completed before capture; the fixture additionally has no credentials, a loopback-only port-9 account, and an updater overlay with no endpoints.',
    ],
  };
  writeJson(join(root, 'h016-faithful-packaged-gui-upgrade.json'), final);
  return final;
}

function quarantine(options) {
  requireOptions(options, ['evidence-root']);
  const { root, control } = loadControlForCleanup(options['evidence-root']);
  assertCandidateStopped(control.executable.path);
  const destination = validateOwnedDestination(control);
  let snapshotError = null;
  try {
    const after = databaseSnapshot(destination);
    writeJson(join(root, 'profile-incomplete-before-move.json'), after);
  } catch (error) {
    snapshotError = error.message;
    writeJson(join(root, 'profile-incomplete-snapshot-error.json'), { schemaVersion: 1, status: 'snapshot-failed', error: error.message });
  }
  const moved = moveOwnedProfile(root, control, 'incomplete');
  const report = { schemaVersion: 1, status: 'incomplete-and-quarantined', capturedAt: new Date().toISOString(), destinationRemoved: !existsSync(control.destination), preservedAt: moved, snapshotError };
  writeJson(join(root, 'h016-incomplete-run.json'), report);
  return report;
}

async function launch(options) {
  requireOptions(options, ['evidence-root']);
  const { root, control } = loadControlForCleanup(options['evidence-root']);
  let child = null;
  let exitPromise = null;
  let timeoutId = null;
  try {
    validateCandidate({ provenance: control.provenance, inventory: control.inventory, 'expected-app-sha': control.executable.sha256, 'expected-provenance-sha': control.provenanceSha256, 'expected-inventory-sha': control.inventorySha256, 'expected-bundle-identifier': control.bundleIdentifier, 'expected-source-digest': control.sourceDigest }, { destinationMustBeAbsent: false });
    validateOwnedDestination(control);
    assertCandidateStopped(control.executable.path);
    requireValue(!existsSync(join(root, 'launch.json')), 'This prepared evidence root was already launched.');
    const env = scrubDakiaEnvironment(process.env);
    requireValue(env.HOME === process.env.HOME, 'The launch environment must preserve HOME exactly.');
    const removed = Object.keys(process.env).filter((name) => !(name in env));
    const appLog = openSync(join(root, 'app.log'), 'wx', 0o600);
    child = spawn(control.executable.path, [], { env, stdio: ['ignore', appLog, appLog] });
    closeSync(appLog);
    exitPromise = new Promise((resolvePromise) => {
      child.once('error', (error) => resolvePromise({ code: null, signal: null, error: error.message }));
      child.once('exit', (code, signal) => resolvePromise({ code, signal, error: null }));
    });
    requireValue(Number.isInteger(child.pid) && child.pid > 1, 'The private candidate did not receive a PID.');
    const launchReport = {
      schemaVersion: 1,
      evidenceId: control.evidenceId,
      startedAt: new Date().toISOString(),
      pid: child.pid,
      executable: control.executable,
      bundleIdentifier: control.bundleIdentifier,
      expectedWebviewUrl: 'tauri://localhost/',
      normalApplicationDataPath: control.destination,
      removedEnvironmentVariables: removed.sort(),
      homePreserved: env.HOME,
      handoffTimeoutMs: HANDOFF_TIMEOUT_MS,
    };
    writeJson(join(root, 'launch.json'), launchReport);
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 2000));
    if (!processForPid(child.pid)) fail('The private candidate exited before the startup-state snapshot.');
    assertLiveCandidate(control, child.pid);
    const startup = databaseSnapshot(control.destination);
    assertLogicalProfile(startup);
    writeJson(join(root, 'profile-after-startup-before-cua.json'), startup);
    process.stdout.write(`${JSON.stringify({ status: 'waiting-for-cua', evidenceRoot: root, pid: child.pid, executable: control.executable.path, executableSha256: control.executable.sha256, bundleIdentifier: control.bundleIdentifier, requiredWebviewUrl: 'tauri://localhost/', deadlineSeconds: HANDOFF_TIMEOUT_MS / 1000, runtimeCommand: `${process.argv[1]} attest-runtime --evidence-root ${root}`, cuaRecordCommand: `${process.argv[1]} record-cua --evidence-root ${root} --observed-url tauri://localhost/ --visible-subject '${SUBJECT}' --visible-body '${BODY}' --unread-visible yes --star-visible yes` }, null, 2)}\n`);
    const timeoutPromise = new Promise((resolvePromise) => { timeoutId = setTimeout(() => resolvePromise({ timedOut: true }), HANDOFF_TIMEOUT_MS); });
    const outcome = await Promise.race([exitPromise, timeoutPromise]);
    if (outcome?.timedOut) fail('CUA handoff exceeded its bounded 600-second deadline.');
    clearTimeout(timeoutId);
    timeoutId = null;
    const exit = { schemaVersion: 1, evidenceId: control.evidenceId, pid: child.pid, executable: control.executable, exitedAt: new Date().toISOString(), ...outcome };
    overwriteJson(join(root, 'exit.json'), exit);
    requireValue(outcome.code === 0 && outcome.signal === null && outcome.error === null, `Private candidate failed or was signaled: ${JSON.stringify(outcome)}.`);
    const final = sealAfterExit(root, control);
    process.stdout.write(`${JSON.stringify({ status: 'sealed', evidenceRoot: root, finalReport: join(root, 'h016-faithful-packaged-gui-upgrade.json'), final }, null, 2)}\n`);
    return final;
  } catch (error) {
    if (timeoutId) clearTimeout(timeoutId);
    if (child && Number.isInteger(child.pid)) {
      let row = processForPid(child.pid);
      if (row && commandOwnsExecutable(row.command, control.executable.path)) {
        child.kill('SIGTERM');
        await Promise.race([exitPromise, new Promise((resolvePromise) => setTimeout(resolvePromise, 5000))]);
        row = processForPid(child.pid);
        if (row && commandOwnsExecutable(row.command, control.executable.path)) {
          process.kill(child.pid, 'SIGKILL');
          await exitPromise;
        }
      }
    }
    if (!existsSync(join(root, 'exit.json')) && child) overwriteJson(join(root, 'exit.json'), { schemaVersion: 1, evidenceId: control.evidenceId, pid: child.pid ?? null, executable: control.executable, exitedAt: new Date().toISOString(), code: child.exitCode, signal: child.signalCode, error: error.message });
    overwriteJson(join(root, 'launch-failure.json'), { schemaVersion: 1, status: 'failed', capturedAt: new Date().toISOString(), error: error.message });
    try { quarantine({ 'evidence-root': root }); } catch (quarantineError) { overwriteJson(join(root, 'quarantine-failure.json'), { schemaVersion: 1, status: 'failed', error: quarantineError.message }); }
    throw error;
  }
}

function usage() {
  process.stderr.write(`Usage:\n  ${process.argv[1]} preflight --provenance PATH --inventory PATH --expected-app-sha SHA256 --expected-provenance-sha SHA256 --expected-inventory-sha SHA256 --expected-bundle-identifier ID --expected-source-digest SHA256\n  ${process.argv[1]} prepare --provenance PATH --inventory PATH --expected-app-sha SHA256 --expected-provenance-sha SHA256 --expected-inventory-sha SHA256 --expected-bundle-identifier ID --expected-source-digest SHA256\n  ${process.argv[1]} launch --evidence-root PATH\n  ${process.argv[1]} attest-runtime --evidence-root PATH\n  ${process.argv[1]} record-cua --evidence-root PATH --observed-url tauri://localhost/ --visible-subject TEXT --visible-body TEXT --unread-visible yes --star-visible yes\n  ${process.argv[1]} quarantine --evidence-root PATH\n`);
  process.exit(64);
}

export {
  ACCOUNT_ID,
  BODY,
  BODY_HTML,
  BUNDLE_RE,
  EXPECTED_DB_SHA256,
  MESSAGE_ID,
  SEALED_PROFILE,
  SUBJECT,
  assertBundleIdentifier,
  applicationSupportRoot,
  assertControlIdentity,
  assertMarkerIdentity,
  assertSuccessfulExit,
  assertLogicalProfile,
  commandOwnsExecutable,
  databaseSnapshot,
  scrubDakiaEnvironment,
  validateSealedProfile,
  validateFaithfulReport,
};

if (import.meta.url === new URL(`file://${process.argv[1]}`).href) {
  try {
    const command = process.argv[2];
    if (!command) usage();
    const allowed = command === 'record-cua'
      ? ['evidence-root', 'observed-url', 'visible-subject', 'visible-body', 'unread-visible', 'star-visible']
      : command === 'preflight' || command === 'prepare'
        ? ['provenance', 'inventory', 'expected-app-sha', 'expected-provenance-sha', 'expected-inventory-sha', 'expected-bundle-identifier', 'expected-source-digest']
        : ['evidence-root'];
    const options = parseOptions(process.argv.slice(3), allowed);
    let result;
    if (command === 'preflight') result = preflight(options);
    else if (command === 'prepare') result = prepare(options);
    else if (command === 'launch') result = await launch(options);
    else if (command === 'attest-runtime') result = attestRuntime(options);
    else if (command === 'record-cua') result = recordCua(options);
    else if (command === 'quarantine') result = quarantine(options);
    else usage();
    if (command !== 'launch') process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exit(error.exitCode || 1);
  }
}
