import assert from 'node:assert/strict';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { chmodSync, cpSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  BODY,
  EXPECTED_DB_SHA256,
  SUBJECT,
  assertBundleIdentifier,
  applicationSupportRoot,
  assertControlIdentity,
  assertLogicalProfile,
  assertMarkerIdentity,
  assertSuccessfulExit,
  commandOwnsExecutable,
  databaseSnapshot,
  scrubDakiaEnvironment,
  validateSealedProfile,
} from './dakia-h016-faithful-upgrade.mjs';

test('accepts only the unique private packaged ACL identifier family', () => {
  assert.equal(assertBundleIdentifier('dev.dakia.fictional-packaged-acl.abc123'), 'dev.dakia.fictional-packaged-acl.abc123');
  for (const value of [
    'dev.dakia.mail',
    'dev.dakia.fictional-packaged-acl',
    'dev.dakia.fictional-packaged-acl.ABC',
    'dev.dakia.fictional-packaged-acl.abc/escape',
    'dev.dakia.fictional-packaged-acl.abc.extra',
  ]) assert.throws(() => assertBundleIdentifier(value), /unique packaged-ACL bundle identifier/);
});

test('scrubs every specified Dakia launch override while preserving HOME exactly', () => {
  const input = {
    HOME: '/Users/fictional',
    PATH: '/usr/bin:/bin',
    DAKIA_NATIVE_MAIL_SAFETY_FIXTURE: '1',
    DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR: '/private/tmp/wrong-profile',
    DAKIA_RELEASE_SMOKE_TEST: '1',
    DAKIA_RELEASE_SMOKE_OTHER: '1',
    DAKIA_DATA_DIR: '/private/tmp/wrong-profile',
    DAKIA_UNRELATED_SETTING: 'preserved',
  };
  assert.deepEqual(scrubDakiaEnvironment(input), {
    HOME: '/Users/fictional',
    PATH: '/usr/bin:/bin',
    DAKIA_UNRELATED_SETTING: 'preserved',
  });
});

test('normal Application Support root is bound to the current macOS account HOME', () => {
  assert.equal(applicationSupportRoot(), `${process.env.HOME}/Library/Application Support`);
});

test('process ownership matching is exact and cannot accept substrings', () => {
  const executable = '/private/tmp/Owned.app/Contents/MacOS/dakia-desktop';
  assert.equal(commandOwnsExecutable(executable, executable), true);
  assert.equal(commandOwnsExecutable(`${executable} -psn_0_123`, executable), true);
  assert.equal(commandOwnsExecutable(`/bin/sh ${executable}`, executable), false);
  assert.equal(commandOwnsExecutable(`${executable}-other`, executable), false);
  assert.equal(commandOwnsExecutable(`/tmp/prefix${executable}`, executable), false);
});

test('sealed fixture is exact and its logical v0.4.4 cache state is accepted', () => {
  const sealed = validateSealedProfile();
  const snapshot = databaseSnapshot(sealed, { immutable: true });
  assert.equal(snapshot.database.sha256, EXPECTED_DB_SHA256);
  assert.doesNotThrow(() => assertLogicalProfile(snapshot));
  assert.equal(snapshot.messages[0].subject, SUBJECT);
  assert.equal(snapshot.starredBodies[0].body_text, BODY);
});

test('logical oracle rejects lost body, changed account target, credentials, and premature read state', () => {
  const original = databaseSnapshot(validateSealedProfile(), { immutable: true });
  const mutate = (callback) => {
    const copy = structuredClone(original);
    callback(copy);
    return copy;
  };
  assert.throws(() => assertLogicalProfile(mutate((value) => { value.starredBodies[0].body_text = ''; })), /starred body changed/);
  assert.throws(() => assertLogicalProfile(mutate((value) => { value.accounts[0].data = value.accounts[0].data.replace('127.0.0.1', 'imap.example.test'); })), /account payload changed/);
  assert.throws(() => assertLogicalProfile(mutate((value) => { value.credentials[0].credential_count = 1; })), /contains credentials/);
  assert.throws(() => assertLogicalProfile(mutate((value) => { value.messages[0].is_read = 1; })), /unread state changed/);
  assert.doesNotThrow(() => assertLogicalProfile(mutate((value) => { value.messages[0].is_read = 1; }), { allowReadTransition: true }));
  assert.throws(() => assertLogicalProfile(mutate((value) => { value.appMeta = value.appMeta.filter(({ key }) => key !== 'profile_schema_version'); })), /migration markers/);
  assert.throws(() => assertLogicalProfile(mutate((value) => { value.mailboxCatalogue[0].uid_validity = 45; })), /UIDVALIDITY epoch/);
});

test('control, marker, and exit identities reject traversal and failed runs', () => {
  const id = '12345678-1234-4123-8123-123456789abc';
  const support = '/Users/fictional/Library/Application Support';
  const control = {
    schemaVersion: 1,
    evidenceId: id,
    bundleIdentifier: 'dev.dakia.fictional-packaged-acl.abc123',
    destination: `${support}/dev.dakia.fictional-packaged-acl.abc123`,
    sealedProfile: '/private/tmp/dakia-forward-upgrade-20261021/h016-faithful-pre-candidate-backup',
    sealedDatabaseSha256: EXPECTED_DB_SHA256,
    executable: { path: '/private/tmp/Candidate.app/Contents/MacOS/dakia-desktop', sha256: 'a'.repeat(64) },
    provenanceSha256: 'b'.repeat(64), inventorySha256: 'c'.repeat(64), sourceDigest: 'd'.repeat(64),
  };
  const root = `/private/tmp/dakia-h016-packaged-upgrade.${id}`;
  assert.doesNotThrow(() => assertControlIdentity(root, control, support));
  assert.throws(() => assertControlIdentity(root, { ...control, destination: '/Users/fictional/Documents' }, support), /immediate Application Support child/);
  assert.throws(() => assertControlIdentity(root, { ...control, bundleIdentifier: '../escape' }, support), /unique packaged-ACL/);
  const marker = { schemaVersion: 1, evidenceId: id, bundleIdentifier: control.bundleIdentifier, destination: control.destination, executableSha256: control.executable.sha256, sealedDatabaseSha256: control.sealedDatabaseSha256, provenanceSha256: control.provenanceSha256, inventorySha256: control.inventorySha256, sourceDigest: control.sourceDigest };
  assert.doesNotThrow(() => assertMarkerIdentity(marker, control, control.destination));
  assert.throws(() => assertMarkerIdentity({ ...marker, sourceDigest: 'e'.repeat(64) }, control, control.destination), /complete evidence control identity/);
  const launch = { pid: 42, executable: control.executable };
  const exit = { schemaVersion: 1, evidenceId: id, pid: 42, executable: control.executable, code: 0, signal: null, error: null };
  assert.doesNotThrow(() => assertSuccessfulExit(control, launch, exit));
  assert.throws(() => assertSuccessfulExit(control, launch, { ...exit, code: 1 }), /failed, signaled/);
  assert.throws(() => assertSuccessfulExit(control, launch, { ...exit, signal: 'SIGTERM', code: null }), /failed, signaled/);
  assert.throws(() => assertSuccessfulExit(control, launch, { ...exit, pid: 43 }), /another run/);
});

test('cleanup ownership proof is independent of a missing or corrupted candidate', () => {
  const source = readFileSync('/private/tmp/dakia-h016-faithful-upgrade.mjs', 'utf8');
  const quarantineBody = source.match(/function quarantine\(options\) \{([\s\S]*?)\n\}/)?.[1] ?? '';
  const cleanupLoaderBody = source.match(/function loadControlForCleanup\(evidenceRoot\) \{([\s\S]*?)\n\}/)?.[1] ?? '';
  assert.match(quarantineBody, /loadControlForCleanup/);
  assert.doesNotMatch(quarantineBody, /loadControl\(/);
  assert.doesNotMatch(cleanupLoaderBody, /validateCandidate/);
  assert.match(cleanupLoaderBody, /assertControlIdentity/);
});

test('working profile snapshots observe WAL-only mutations that immutable reads miss', async () => {
  const root = mkdtempSync(join(tmpdir(), 'dakia-h016-wal-test.'));
  const profile = join(root, 'profile');
  cpSync(validateSealedProfile(), profile, { recursive: true });
  chmodSync(profile, 0o700);
  for (const name of ['dakia.db', 'dakia.db-shm', 'dakia.db-wal', 'vault.key', '.dakia-h016-faithful-v044-profile', '.dakia-h016-owned-profile']) chmodSync(join(profile, name), 0o600);
  const writer = spawn('sqlite3', [join(profile, 'dakia.db')], { stdio: ['pipe', 'pipe', 'pipe'] });
  let stdout = '';
  writer.stdout.on('data', (chunk) => { stdout += chunk; });
  writer.stdin.write("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE starred_message_bodies SET body_text='lost in wal' WHERE message_id='fictional-v044-upgrade-message';\n.print H016_READY\n");
  for (let index = 0; index < 100 && !stdout.includes('H016_READY'); index += 1) await new Promise((resolve) => setTimeout(resolve, 10));
  assert.match(stdout, /H016_READY/);
  const walAware = databaseSnapshot(profile);
  const staleImmutable = databaseSnapshot(profile, { immutable: true });
  assert.equal(walAware.starredBodies[0].body_text, 'lost in wal');
  assert.equal(staleImmutable.starredBodies[0].body_text, BODY);
  assert.throws(() => assertLogicalProfile(walAware), /starred body changed/);
  assert.doesNotThrow(() => assertLogicalProfile(staleImmutable));
  writer.stdin.end('.exit\n');
  await new Promise((resolve) => writer.once('exit', resolve));
  rmSync(root, { recursive: true, force: true });
});
