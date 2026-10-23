#!/usr/bin/env node
import { createHash } from "node:crypto";
import {
  closeSync,
  existsSync,
  lstatSync,
  openSync,
  readFileSync,
  readdirSync,
  readlinkSync,
  realpathSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, isAbsolute, join, relative, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";

const OWNER_MARKER = ".dakia-l013-owned-run.json";
const OWNER_PURPOSE = "dakia-native-l013-resource-v1";
const LIMITS_PURPOSE = "dakia-native-l013-limits-v1";
const UUID_RE =
  /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const SHA_RE = /^[a-f0-9]{64}$/;
const PROVIDER_BUNDLE_RE =
  /^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$/;
const PROVIDER_CONTRACT = "dakia-native-provider-reader-approved-identity-v1";
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
const PYTHON_LIBPROC = String.raw`
import ctypes, ctypes.util, json, os, re, struct, sys

PROC_PIDLISTFDS = 1
PROC_PIDTBSDINFO = 3
PROC_PIDTASKINFO = 4
BSD_INFO_SIZE = 136
TASK_INFO_SIZE = 96
PID_SIZE = ctypes.sizeof(ctypes.c_int)
WEBKIT_ROLES = {
    'com.apple.WebKit.WebContent',
    'com.apple.WebKit.Networking',
    'com.apple.WebKit.GPU',
    'com.apple.WebKit.Model',
}

lib = ctypes.CDLL(ctypes.util.find_library('proc') or '/usr/lib/libproc.dylib', use_errno=True)
lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
lib.proc_pidinfo.restype = ctypes.c_int
lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
lib.proc_pidpath.restype = ctypes.c_int
lib.proc_listallpids.argtypes = [ctypes.c_void_p, ctypes.c_int]
lib.proc_listallpids.restype = ctypes.c_int

def call_info(pid, flavor, size):
    buf = ctypes.create_string_buffer(size)
    got = lib.proc_pidinfo(pid, flavor, 0, buf, size)
    if got != size:
        raise OSError(ctypes.get_errno(), 'proc_pidinfo')
    return buf.raw

def identity(pid):
    vals = struct.unpack('<12I16s32s5IiQQ', call_info(pid, PROC_PIDTBSDINFO, BSD_INFO_SIZE))
    pathbuf = ctypes.create_string_buffer(4096)
    if lib.proc_pidpath(pid, pathbuf, len(pathbuf)) <= 0:
        raise OSError(ctypes.get_errno(), 'proc_pidpath')
    return {
        'pid': pid,
        'ppid': vals[4],
        'uid': vals[5],
        'comm': vals[12].split(b'\0', 1)[0].decode('utf-8', 'replace'),
        'name': vals[13].split(b'\0', 1)[0].decode('utf-8', 'replace'),
        'path': os.path.realpath(pathbuf.value.decode('utf-8')),
        'startTimeMicros': str(vals[-2] * 1000000 + vals[-1]),
    }

def fd_count(pid):
    needed = lib.proc_pidinfo(pid, PROC_PIDLISTFDS, 0, None, 0)
    if needed <= 0 or needed % 8 != 0:
        raise OSError(ctypes.get_errno(), 'PROC_PIDLISTFDS capacity')
    capacity = max(needed * 2, needed + 8192)
    for unused in range(4):
        buf = ctypes.create_string_buffer(capacity)
        used = lib.proc_pidinfo(pid, PROC_PIDLISTFDS, 0, buf, capacity)
        if used > 0 and used % 8 == 0 and used < capacity:
            return used // 8
        capacity *= 2
    raise OSError(ctypes.get_errno(), 'PROC_PIDLISTFDS truncated')

def full(pid, expected=None):
    first = identity(pid)
    if expected is not None and (first['pid'], first['startTimeMicros'], first['path']) != (expected['pid'], expected['startTimeMicros'], expected['path']):
        raise RuntimeError('matched_pid_reused_or_changed')
    task = struct.unpack('<6Q12i', call_info(pid, PROC_PIDTASKINFO, TASK_INFO_SIZE))
    result = dict(first)
    result.update({
        'rssBytes': task[1],
        'virtualBytes': task[0],
        'threadCount': task[-3],
        'openFileDescriptors': fd_count(pid),
    })
    last = identity(pid)
    if (last['pid'], last['startTimeMicros'], last['path']) != (first['pid'], first['startTimeMicros'], first['path']):
        raise RuntimeError('matched_pid_reused_or_changed')
    return result

def list_all_pids():
    estimate = lib.proc_listallpids(None, 0)
    if estimate <= 0:
        raise RuntimeError('pid_enumeration_failed')
    capacity = max(estimate * 2, estimate + 256)
    for unused in range(4):
        arr = (ctypes.c_int * capacity)()
        used = lib.proc_listallpids(arr, capacity * PID_SIZE)
        if 0 < used < capacity:
            raw = [int(arr[i]) for i in range(used)]
            if any(pid <= 0 for pid in raw) or len(set(raw)) != len(raw):
                raise RuntimeError('pid_enumeration_invalid')
            return sorted(raw)
        capacity *= 2
    raise RuntimeError('pid_enumeration_truncated')

def is_webkit(candidate):
    role = os.path.basename(candidate['path'])
    if role not in WEBKIT_ROLES or candidate['name'] != role or not candidate['comm'] or not role.startswith(candidate['comm']):
        return False
    escaped = re.escape(role)
    pattern = r'^/System/Library/Frameworks/WebKit\.framework/Versions/[^/]+/XPCServices/' + escaped + r'\.xpc/Contents/MacOS/' + escaped + r'$'
    return re.fullmatch(pattern, candidate['path']) is not None

def unavailable(reason, enumerated=0, matched=0):
    return {
        'availability': 'unavailable',
        'reason': reason,
        'enumeratedPidCount': enumerated,
        'responsibilityMatchedPidCount': matched,
        'acceptedWebKitProcessCount': 0,
        'processes': [],
    }

def scan(root, main):
    try:
        system = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
        responsible = getattr(system, 'responsibility_get_pid_responsible_for_pid')
        responsible.argtypes = [ctypes.c_int]
        responsible.restype = ctypes.c_int
    except Exception:
        return unavailable('private_api_unavailable')

    try:
        root_responsible = int(responsible(root))
    except Exception:
        return unavailable('private_api_call_failed')
    if root_responsible <= 0:
        return unavailable('private_api_call_failed')
    if root_responsible != root:
        return unavailable('main_responsibility_mismatch')

    last_enumerated = 0
    for unused in range(3):
        try:
            before = list_all_pids()
        except RuntimeError as error:
            return unavailable(str(error))
        last_enumerated = len(before)
        if root not in before:
            return unavailable('main_missing_from_pid_census', last_enumerated)
        matches = []
        lookup_failed = False
        for pid in before:
            if pid == root:
                continue
            try:
                owner = int(responsible(pid))
            except Exception:
                lookup_failed = True
                continue
            if owner <= 0:
                lookup_failed = True
            elif owner == root:
                matches.append(pid)
        try:
            middle = list_all_pids()
        except RuntimeError as error:
            return unavailable(str(error), last_enumerated, len(matches))
        if root not in middle:
            return unavailable('main_missing_from_pid_census', len(middle), len(matches))
        if before != middle:
            continue
        if lookup_failed:
            return unavailable('responsibility_lookup_incomplete', last_enumerated, len(matches))

        accepted = []
        try:
            for pid in matches:
                first = identity(pid)
                if first['uid'] != main['uid'] or not is_webkit(first):
                    continue
                measured = full(pid, first)
                owner_after_measurement = int(responsible(pid))
                final_identity = identity(pid)
                if owner_after_measurement != root or (final_identity['pid'], final_identity['startTimeMicros'], final_identity['path']) != (first['pid'], first['startTimeMicros'], first['path']):
                    raise RuntimeError('matched_pid_reused_or_responsibility_changed')
                measured['responsiblePid'] = root
                measured['responsibilityVerified'] = True
                measured['identityStable'] = True
                accepted.append(measured)
        except (OSError, ProcessLookupError, RuntimeError):
            try:
                after_failure = list_all_pids()
            except RuntimeError:
                return unavailable('pid_enumeration_failed', last_enumerated, len(matches))
            if root not in after_failure:
                return unavailable('main_missing_from_pid_census', len(after_failure), len(matches))
            if before != after_failure:
                continue
            return unavailable('matched_process_unreadable_or_changed', last_enumerated, len(matches))

        try:
            after = list_all_pids()
        except RuntimeError as error:
            return unavailable(str(error), last_enumerated, len(matches))
        if root not in after:
            return unavailable('main_missing_from_pid_census', len(after), len(matches))
        if before != after:
            continue
        return {
            'availability': 'lower_bound_only',
            'reason': None,
            'enumeratedPidCount': last_enumerated,
            'responsibilityMatchedPidCount': len(matches),
            'acceptedWebKitProcessCount': len(accepted),
            'processes': sorted(accepted, key=lambda item: item['pid']),
        }
    return unavailable('pid_census_unstable', last_enumerated, 0)

root = int(sys.argv[1])
mode = sys.argv[2] if len(sys.argv) > 2 else 'scan'
main = full(root)
if mode == 'main':
    print(json.dumps({'main': main}, separators=(',', ':')))
elif mode == 'scan':
    census = scan(root, main)
    final_main = identity(root)
    if (final_main['pid'], final_main['startTimeMicros'], final_main['path']) != (main['pid'], main['startTimeMicros'], main['path']):
        raise RuntimeError('main_pid_reused_or_changed')
    print(json.dumps({'main': main, 'responsibilityCensus': census}, separators=(',', ':')))
else:
    raise RuntimeError('invalid collector mode')
`;

function fail(message, code = 1) {
  const e = new Error(message);
  e.exitCode = code;
  throw e;
}
function requireValue(value, message, code = 1) {
  if (!value) fail(message, code);
}
function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}
function json(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}
function invalidOrAmbiguousPlist() {
  fail("Provider Reader application plist is invalid or ambiguous.");
}
function inside(root, child, allowSame = false) {
  const rel = relative(root, child);
  return (
    (allowSame && rel === "") ||
    (rel !== "" &&
      rel !== ".." &&
      !rel.startsWith(`..${sep}`) &&
      !isAbsolute(rel))
  );
}
function regular(path, label) {
  const s = lstatSync(path);
  requireValue(
    s.isFile() && !s.isSymbolicLink(),
    `${label} must be a regular file.`,
  );
}
function writeExclusive(path, value) {
  const parent = realpathSync(dirname(path));
  const fd = openSync(path, "wx", 0o600);
  try {
    writeFileSync(fd, `${JSON.stringify(value, null, 2)}\n`);
  } finally {
    closeSync(fd);
  }
  requireValue(
    realpathSync(dirname(path)) === parent,
    "Output parent changed while writing.",
  );
  requireValue(
    (statSync(path).mode & 0o777) === 0o600,
    "Output permissions are not 0600.",
  );
}
function parseOptions(args) {
  const o = {};
  for (let i = 0; i < args.length; i += 2) {
    requireValue(
      args[i]?.startsWith("--") && args[i + 1] !== undefined,
      "Every option needs a value.",
      64,
    );
    const k = args[i].slice(2);
    requireValue(o[k] === undefined, `Repeated option: --${k}`, 64);
    o[k] = args[i + 1];
  }
  return o;
}
function requireOptions(o, keys) {
  for (const k of keys) requireValue(o[k], `Missing --${k}.`, 64);
}

export function validateOwner(runRootInput) {
  const runRoot = realpathSync(runRootInput);
  requireValue(
    runRoot.startsWith("/private/tmp/") && runRoot !== "/private/tmp",
    "Run root must be a unique directory below /private/tmp.",
  );
  const markerPath = join(runRoot, OWNER_MARKER);
  const rootInfo = lstatSync(runRoot);
  requireValue(
    rootInfo.isDirectory() &&
      !rootInfo.isSymbolicLink() &&
      rootInfo.uid === process.getuid() &&
      (rootInfo.mode & 0o777) === 0o700,
    "Run root must be a private owned directory.",
  );
  privateOwnedRegular(markerPath, "Run owner marker");
  const marker = json(markerPath);
  requireValue(
    marker.schemaVersion === 1 &&
      marker.purpose === OWNER_PURPOSE &&
      UUID_RE.test(marker.runId),
    "Run owner marker identity is invalid.",
  );
  requireValue(
    marker.runRoot === runRoot && marker.ownerUid === process.getuid(),
    "Run owner marker does not bind this root and user.",
  );
  return { runRoot, markerPath, marker, markerSha256: sha256(markerPath) };
}

export function validateIdentity(
  owner,
  identityPathInput,
  expectedSha,
  observed,
  options = {},
) {
  const i = loadIdentityRecord(owner, identityPathInput, expectedSha, {
    ...options,
    live: true,
  });
  requireValue(
    i.pid === observed.pid,
    "PID does not match the approved identity.",
  );
  requireValue(
    i.startTimeMicros === observed.startTimeMicros,
    "PID start time changed; possible PID reuse.",
  );
  requireValue(
    i.executablePath === observed.path,
    "Runtime executable path changed.",
  );
  requireValue(
    observed.uid === process.getuid(),
    "Runtime process is not owned by the current user.",
  );
  return i;
}

function pinnedPrivate(ref, label) {
  requireValue(
    ref && isAbsolute(ref.path) && SHA_RE.test(ref.sha256),
    `${label} pin is invalid.`,
  );
  privateOwnedRegular(ref.path, label);
  requireValue(sha256(ref.path) === ref.sha256, `${label} hash changed.`);
  return json(ref.path);
}

function sourceSnapshot() {
  const module = new URL("./run-mail-safety.mjs", import.meta.url).href;
  const script =
    "const { sourceSnapshot } = await import(process.argv[1]); process.stdout.write(JSON.stringify(sourceSnapshot()));";
  const result = spawnSync(
    process.execPath,
    ["--input-type=module", "-e", script, module],
    {
      encoding: "utf8",
      timeout: 10_000,
      maxBuffer: 4096,
      stdio: ["ignore", "pipe", "ignore"],
    },
  );
  requireValue(
    !result.error && result.status === 0 && !result.signal,
    "Current governed source enumeration failed or exceeded its deadline.",
  );
  try {
    return JSON.parse(result.stdout);
  } catch {
    fail("Current governed source enumeration is invalid.");
  }
}

export function validateSourceBinding(
  source,
  { live = false, snapshot = sourceSnapshot } = {},
) {
  requireValue(
    source &&
      isAbsolute(source.root) &&
      realpathSync(source.root) === source.root &&
      SHA_RE.test(source.digest) &&
      Number.isSafeInteger(source.fileCount) &&
      source.fileCount > 0,
    "Runtime source binding is invalid.",
  );
  const manifest = pinnedPrivate(source.manifest, "Source manifest");
  requireValue(
    manifest.algorithm === "sha256-path-and-content-manifest" &&
      manifest.digest === source.digest &&
      manifest.fileCount === source.fileCount &&
      Array.isArray(manifest.files) &&
      manifest.files.length === source.fileCount,
    "Source manifest binding changed.",
  );
  const digest = createHash("sha256");
  const paths = new Set();
  for (const row of [...manifest.files].sort((a, b) =>
    String(a.path) < String(b.path)
      ? -1
      : String(a.path) > String(b.path)
        ? 1
        : 0,
  )) {
    requireValue(
      typeof row.path === "string" &&
        row.path.length > 0 &&
        !isAbsolute(row.path) &&
        !row.path
          .split("/")
          .some((part) => !part || part === "." || part === "..") &&
        !paths.has(row.path),
      "Source manifest path is invalid.",
    );
    requireValue(
      ["present", "deleted"].includes(row.state) &&
        SHA_RE.test(row.sha256) &&
        Number.isSafeInteger(row.bytes) &&
        row.bytes >= 0,
      "Source manifest row is invalid.",
    );
    paths.add(row.path);
    digest.update(row.path).update("\0");
    if (row.state === "present") digest.update(Buffer.from(row.sha256, "hex"));
    else digest.update("deleted");
    digest.update("\0");
    if (live) {
      const path = join(source.root, row.path);
      if (row.state === "present") {
        regular(path, "Source file");
        requireValue(
          realpathSync(path) === path &&
            inside(source.root, path) &&
            statSync(path).size === row.bytes &&
            sha256(path) === row.sha256,
          "Source file changed.",
        );
      } else requireValue(!existsSync(path), "Deleted source file reappeared.");
    }
  }
  requireValue(
    digest.digest("hex") === source.digest,
    "Source manifest digest changed.",
  );
  if (live) {
    requireValue(
      realpathSync(process.cwd()) === source.root,
      "Runtime source root must be the current checkout.",
    );
    const current = snapshot();
    requireValue(
      current.digest === source.digest &&
        current.fileCount === source.fileCount,
      "Runtime source is no longer the current governed checkout.",
    );
  }
  return source;
}

// This pinned runtime record replaces the dated private identity format. The
// observer that recorded webviewEvidence is external and remains explicit.
export function loadIdentityRecord(
  owner,
  identityPathInput,
  expectedSha,
  {
    live = false,
    snapshot = sourceSnapshot,
    now = () => Date.now(),
    webviewEvidence = null,
  } = {},
) {
  requireValue(
    SHA_RE.test(expectedSha),
    "Expected runtime manifest SHA-256 is invalid.",
  );
  const identityPath = realpathSync(identityPathInput);
  requireValue(
    identityPath === identityPathInput && inside(owner.runRoot, identityPath),
    "Runtime manifest must be canonical and inside the owned run root.",
  );
  const r = pinnedPrivate(
    { path: identityPath, sha256: expectedSha },
    "Runtime manifest",
  );
  requireValue(
    r.schemaVersion === 1 &&
      r.purpose === "dakia-native-resource-runtime-v1" &&
      r.runId === owner.marker.runId &&
      r.runRoot === owner.runRoot &&
      r.ownerMarkerSha256 === owner.markerSha256,
    "Runtime manifest does not bind this owned run.",
  );
  validateSourceBinding(r.source, { live, snapshot });
  const approved = pinnedPrivate(
    r.approvedIdentity,
    "Approved identity record",
  );
  requireValue(
    approved.schemaVersion === 1 &&
      approved.contract === PROVIDER_CONTRACT &&
      approved.displayName === "Dakia Fictional Provider Reader" &&
      PROVIDER_BUNDLE_RE.test(approved.bundleIdentifier),
    "Approved identity record schema is invalid.",
  );
  requireValue(
    isAbsolute(approved.approvedRoot) &&
      realpathSync(approved.approvedRoot) === approved.approvedRoot &&
      approved.approvedApp ===
        join(approved.approvedRoot, "Dakia Fictional Provider Reader.app") &&
      r.approvedIdentity.path ===
        join(approved.approvedRoot, "approved-native-identity.json"),
    "Approved identity paths changed.",
  );
  requireValue(
    approved.inventory &&
      Array.isArray(approved.inventory.entries) &&
      SHA_RE.test(approved.inventory.digest) &&
      createHash("sha256")
        .update(JSON.stringify(approved.inventory.entries))
        .digest("hex") === approved.inventory.digest,
    "Approved inventory digest changed.",
  );
  const journal = pinnedPrivate(
    r.candidateTransaction,
    "Candidate transaction",
  );
  const activeJournalPath =
    r.candidateTransaction.activePath || r.candidateTransaction.path;
  requireValue(
    activeJournalPath ===
      join(approved.approvedRoot, ".approved-native-identity-swap.json") &&
      journal.schemaVersion === 1 &&
      journal.contract === PROVIDER_CONTRACT &&
      UUID_RE.test(journal.transactionId) &&
      journal.stage === "candidate-installed" &&
      journal.approvedRoot === approved.approvedRoot &&
      journal.approvedApp === approved.approvedApp &&
      journal.originalInventoryDigest === approved.inventory.digest &&
      SHA_RE.test(journal.candidateInventoryDigest),
    "Candidate transaction does not bind the approved application.",
  );
  const app = r.application;
  requireValue(
    app &&
      app.path === approved.approvedApp &&
      app.bundleIdentifier === approved.bundleIdentifier &&
      app.executable === join(app.path, "Contents/MacOS/dakia-desktop") &&
      SHA_RE.test(app.executableSha256) &&
      journal.candidateDesktopSha256 === app.executableSha256,
    "Candidate executable binding changed.",
  );
  const p = r.process;
  requireValue(
    p &&
      Number.isSafeInteger(p.pid) &&
      p.pid > 1 &&
      /^\d+$/.test(p.startTimeMicros),
    "Runtime process identity is invalid.",
  );
  requireValue(
    Number.isFinite(Date.parse(journal.createdAt)) &&
      BigInt(p.startTimeMicros) >=
        BigInt(Date.parse(journal.createdAt)) * 1000n,
    "Runtime process predates the candidate transaction.",
  );
  const webviewPin = webviewEvidence || r.webviewEvidence;
  const web = pinnedPrivate(webviewPin, "Native webview evidence");
  requireValue(
    web.schemaVersion === 1 &&
      web.method === "native-accessibility-tree" &&
      web.pid === p.pid &&
      web.startTimeMicros === p.startTimeMicros &&
      web.executablePath === app.executable &&
      web.executableSha256 === app.executableSha256 &&
      ["tauri://localhost/", "http://127.0.0.1:1420/"].includes(
        web.webviewUrl,
      ) &&
      Number.isFinite(Date.parse(web.observedAt)),
    "Native webview evidence identity changed.",
  );
  requireValue(
    Date.parse(web.observedAt) >= Number(BigInt(p.startTimeMicros) / 1000n),
    "Webview evidence predates the process.",
  );
  if (live) {
    privateOwnedRegular(activeJournalPath, "Active candidate transaction");
    requireValue(
      sha256(activeJournalPath) === r.candidateTransaction.sha256,
      "Active candidate transaction changed.",
    );
    const requireFreshWebview = () => {
      const timestamp = typeof now === "function" ? now() : now;
      requireValue(
        timestamp >= Date.parse(web.observedAt) &&
          timestamp - Date.parse(web.observedAt) <= 120_000,
        "Native webview evidence must be fresh within 120 seconds.",
      );
    };
    requireFreshWebview();
    validateCanonicalBundlePaths({
      root: approved.approvedRoot,
      app: app.path,
      executable: app.executable,
      plist: join(app.path, "Contents/Info.plist"),
    });
    requireValue(
      sha256(app.executable) === app.executableSha256,
      "Runtime executable bytes changed.",
    );
    parseApprovedBundleInfoPlist(
      join(app.path, "Contents/Info.plist"),
      app.bundleIdentifier,
      approved.displayName,
    );
    requireValue(
      bundleInventory(app.path).digest === journal.candidateInventoryDigest,
      "Live candidate bundle inventory changed.",
    );
    requireFreshWebview();
  }
  return {
    pid: p.pid,
    startTimeMicros: p.startTimeMicros,
    executablePath: app.executable,
    executableSha256: app.executableSha256,
    applicationRoot: app.path,
    bundleIdentifier: app.bundleIdentifier,
    provenance: {
      runtimeManifest: { path: identityPath, sha256: expectedSha },
      source: r.source,
      approvedIdentity: r.approvedIdentity,
      candidateTransaction: r.candidateTransaction,
      webviewEvidence: webviewPin,
      webviewUrl: web.webviewUrl,
      webviewObservedAt: web.observedAt,
      webviewMethod: web.method,
    },
  };
}

function privateOwnedRegular(path, label) {
  regular(path, label);
  const info = statSync(path);
  requireValue(
    info.uid === process.getuid() && (info.mode & 0o777) === 0o600,
    `${label} must be owned by this user with mode 0600.`,
  );
  requireValue(realpathSync(path) === path, `${label} path is not canonical.`);
}

export function bundleInventory(root) {
  const entries = [];
  const started = Date.now();
  let totalBytes = 0;
  function walk(path) {
    requireValue(
      entries.length < 20_000 && Date.now() - started < 10_000,
      "Bundle inventory exceeded its bounded collection budget.",
    );
    const info = lstatSync(path);
    requireValue(
      info.uid === process.getuid(),
      "Bundle entry is not owned by this user.",
    );
    const item = {
      path: path === root ? "." : relative(root, path),
      mode: info.mode & 0o7777,
      uid: info.uid,
      gid: info.gid,
    };
    if (info.isDirectory()) {
      item.type = "directory";
      entries.push(item);
      for (const name of readdirSync(path).sort()) walk(join(path, name));
    } else if (info.isFile()) {
      totalBytes += info.size;
      requireValue(
        totalBytes <= 2 * 1024 * 1024 * 1024,
        "Bundle inventory exceeded its byte budget.",
      );
      item.type = "file";
      item.bytes = info.size;
      item.links = info.nlink;
      item.sha256 = sha256(path);
      entries.push(item);
    } else if (info.isSymbolicLink()) {
      item.type = "symlink";
      item.target = readlinkSync(path);
      entries.push(item);
    } else fail("Bundle contains an unsupported filesystem entry.");
  }
  walk(root);
  requireValue(
    Date.now() - started < 10_000,
    "Bundle inventory exceeded its bounded collection budget.",
  );
  return {
    digest: createHash("sha256").update(JSON.stringify(entries)).digest("hex"),
    entries,
  };
}

export function validateCanonicalBundlePaths({ root, app, executable, plist }) {
  const rootInfo = lstatSync(root);
  const appInfo = lstatSync(app);
  requireValue(
    rootInfo.isDirectory() &&
      !rootInfo.isSymbolicLink() &&
      realpathSync(root) === root,
    "Approved bundle root must be a canonical real directory.",
  );
  requireValue(
    appInfo.isDirectory() &&
      !appInfo.isSymbolicLink() &&
      realpathSync(app) === app &&
      dirname(app) === root,
    "Approved application must be a canonical direct child directory.",
  );
  regular(executable, "Approved desktop executable");
  regular(plist, "Approved application plist");
  requireValue(
    realpathSync(executable) === executable &&
      realpathSync(plist) === plist &&
      inside(app, executable) &&
      inside(app, plist),
    "Approved application files must be canonical regular children.",
  );
}

export function parseApprovedBundleInfoPlist(
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
    )
      invalidOrAmbiguousPlist();
    const source = readFileSync(plistPath);
    if (source.length !== info.size) invalidOrAmbiguousPlist();
    const result = spawnSync(
      "/usr/bin/python3",
      ["-c", BUNDLE_INFO_PLIST_PARSER],
      {
        input: source,
        encoding: "utf8",
        stdio: ["pipe", "pipe", "ignore"],
        timeout: 5_000,
        maxBuffer: MAX_PLIST_PARSER_OUTPUT_BYTES,
      },
    );
    if (
      result.error ||
      result.status !== 0 ||
      result.signal ||
      Buffer.byteLength(result.stdout || "", "utf8") >
        MAX_PLIST_PARSER_OUTPUT_BYTES
    )
      invalidOrAmbiguousPlist();
    const decoded = JSON.parse(result.stdout);
    if (
      Object.getPrototypeOf(decoded) !== Object.prototype ||
      Object.keys(decoded).length !== 2 ||
      !Object.hasOwn(decoded, "bundleIdentifier") ||
      !Object.hasOwn(decoded, "bundleName") ||
      typeof decoded.bundleIdentifier !== "string" ||
      typeof decoded.bundleName !== "string" ||
      decoded.bundleIdentifier !== expectedBundleIdentifier ||
      decoded.bundleName !== expectedBundleName
    )
      invalidOrAmbiguousPlist();
    return decoded;
  } catch {
    invalidOrAmbiguousPlist();
  }
}

export function readProcessCohort(pid) {
  requireValue(
    process.platform === "darwin",
    "Native resource collection requires macOS.",
    69,
  );
  const result = spawnSync(
    "/usr/bin/python3",
    ["-c", PYTHON_LIBPROC, String(pid), "scan"],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: 10_000,
      maxBuffer: 4 * 1024 * 1024,
    },
  );
  if (result.error)
    fail(`libproc collector failed: ${result.error.message}`, 69);
  if (result.status !== 0)
    fail(
      "libproc collector failed without disclosing process identities.",
      result.status || 1,
    );
  try {
    return JSON.parse(result.stdout);
  } catch {
    fail("libproc collector returned invalid JSON.", 69);
  }
}

export function readMainProcess(pid) {
  requireValue(
    process.platform === "darwin",
    "Native resource collection requires macOS.",
    69,
  );
  const result = spawnSync(
    "/usr/bin/python3",
    ["-c", PYTHON_LIBPROC, String(pid), "main"],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: 10_000,
      maxBuffer: 4 * 1024 * 1024,
    },
  );
  if (result.error)
    fail(`libproc main-process collector failed: ${result.error.message}`, 69);
  if (result.status !== 0)
    fail(
      "libproc main-process collector failed without disclosing process identities.",
      result.status || 1,
    );
  try {
    return JSON.parse(result.stdout);
  } catch {
    fail("libproc main-process collector returned invalid JSON.", 69);
  }
}

function webKitIdentity(p, main) {
  const exactName = /^com\.apple\.WebKit\.(WebContent|Networking|GPU|Model)$/;
  const role = basename(p.path);
  const pathOk =
    exactName.test(role) &&
    new RegExp(
      `^/System/Library/Frameworks/WebKit\\.framework/Versions/[^/]+/XPCServices/${role.replaceAll(".", "\\.")}\\.xpc/Contents/MacOS/${role.replaceAll(".", "\\.")}$`,
    ).test(p.path);
  const labelOk =
    p.name === role &&
    typeof p.comm === "string" &&
    p.comm.length > 0 &&
    role.startsWith(p.comm);
  const sameUid = p.uid === main.uid;
  const createdForRun =
    /^\d+$/.test(p.startTimeMicros) &&
    BigInt(p.startTimeMicros) >= BigInt(main.startTimeMicros);
  const responsible =
    p.responsiblePid === main.pid && p.responsibilityVerified === true;
  const stableIdentity = p.identityStable === true;
  const reasons = [];
  if (!pathOk) reasons.push("unapproved_path");
  if (!labelOk) reasons.push("unapproved_name");
  if (!sameUid) reasons.push("uid_mismatch");
  if (!createdForRun) reasons.push("predates_main");
  if (!responsible) reasons.push("responsibility_mismatch");
  if (!stableIdentity) reasons.push("unstable_process_identity");
  return { accepted: reasons.length === 0, reasons };
}
const CENSUS_UNAVAILABLE_REASONS = new Set([
  "private_api_unavailable",
  "private_api_call_failed",
  "main_responsibility_mismatch",
  "pid_enumeration_failed",
  "pid_enumeration_invalid",
  "pid_enumeration_truncated",
  "main_missing_from_pid_census",
  "responsibility_lookup_incomplete",
  "matched_process_unreadable_or_changed",
  "pid_census_unstable",
]);
function validateResponsibilityCensus(census, main) {
  requireValue(
    census && ["lower_bound_only", "unavailable"].includes(census.availability),
    "Responsibility census availability is invalid.",
  );
  for (const key of [
    "enumeratedPidCount",
    "responsibilityMatchedPidCount",
    "acceptedWebKitProcessCount",
  ]) {
    requireValue(
      Number.isSafeInteger(census[key]) && census[key] >= 0,
      `Responsibility census ${key} is invalid.`,
    );
  }
  requireValue(
    census.acceptedWebKitProcessCount <= census.responsibilityMatchedPidCount &&
      census.responsibilityMatchedPidCount <= census.enumeratedPidCount,
    "Responsibility census counts are inconsistent.",
  );
  requireValue(
    Array.isArray(census.processes),
    "Responsibility census process list is invalid.",
  );
  if (census.availability === "unavailable") {
    requireValue(
      CENSUS_UNAVAILABLE_REASONS.has(census.reason) &&
        census.processes.length === 0 &&
        census.acceptedWebKitProcessCount === 0,
      "Unavailable responsibility census is invalid.",
    );
  } else {
    requireValue(
      census.reason === null &&
        census.enumeratedPidCount > 0 &&
        census.acceptedWebKitProcessCount === census.processes.length,
      "Available responsibility census counts are invalid.",
    );
    requireValue(
      census.processes.every((p) => webKitIdentity(p, main).accepted),
      "Responsibility census contains an invalid WebKit process.",
    );
    requireValue(
      new Set(census.processes.map((p) => p.pid)).size ===
        census.processes.length,
      "Responsibility census contains duplicate WebKit PIDs.",
    );
    requireValue(
      census.processes.every(
        (p, index) => index === 0 || census.processes[index - 1].pid < p.pid,
      ),
      "Responsibility census WebKit PIDs are not strictly sorted.",
    );
  }
  return census;
}
function finiteLimit(v, key) {
  requireValue(
    v === null || (Number.isSafeInteger(v) && v > 0),
    `${key} must be null or a positive safe integer.`,
  );
  return v;
}
export function validateLimits(owner, pathInput, expectedSha) {
  requireValue(SHA_RE.test(expectedSha), "Expected limits SHA-256 is invalid.");
  const path = realpathSync(pathInput);
  requireValue(
    inside(owner.runRoot, path),
    "Limits file must be inside the owned run root.",
  );
  regular(path, "Limits file");
  requireValue(sha256(path) === expectedSha, "Limits file hash changed.");
  const l = json(path);
  requireValue(
    l.schemaVersion === 1 &&
      l.purpose === LIMITS_PURPOSE &&
      l.runId === owner.marker.runId,
    "Limits do not bind this run.",
  );
  for (const key of [
    "maxMainRssBytes",
    "maxMainOpenFileDescriptors",
    "maxMappedWebKitAggregateRssBytes",
    "maxMappedWebKitAggregateOpenFileDescriptors",
    "maxMappedWebKitChildren",
  ])
    finiteLimit(l[key], key);
  requireValue(
    l.regressionThresholdPercent === 20,
    "Regression threshold must be exactly 20 percent.",
  );
  return { path, sha256: expectedSha, value: l };
}
function completeThreshold(name, value, limit) {
  if (limit === null)
    return { name, value, limit, status: "observed_no_limit" };
  return { name, value, limit, status: value <= limit ? "passed" : "failed" };
}
function incompleteLowerBoundThreshold(name, value, limit, hasPositiveMatches) {
  if (!hasPositiveMatches) {
    return {
      name,
      value: null,
      limit,
      status: "unavailable",
      interpretation:
        "no_positively_responsibility_mapped_webkit_processes_and_full_cohort_not_identifiable",
    };
  }
  if (limit === null)
    return {
      name,
      value,
      limit,
      status: "observed_lower_bound_no_limit",
      interpretation: "lower_bound_only",
    };
  return {
    name,
    value,
    limit,
    status: value > limit ? "failed" : "unassessable_below_ceiling",
    interpretation: "lower_bound_only",
  };
}
function completeStatus(checks) {
  if (checks.some((check) => check.status === "failed")) return "failed";
  if (checks.every((check) => check.status === "passed")) return "passed";
  return "observed_no_limit";
}
function incompleteStatus(checks) {
  if (checks.some((check) => check.status === "failed")) return "failed";
  if (checks.every((check) => check.status === "unavailable"))
    return "unavailable";
  return "unassessable";
}

export function buildSample({
  owner,
  identity,
  identitySha,
  limits,
  phase,
  before,
  after,
}) {
  const capturedAt = new Date().toISOString();
  const observationAge =
    Date.parse(capturedAt) - Date.parse(identity.provenance?.webviewObservedAt);
  requireValue(
    observationAge >= 0 && observationAge <= 120_000,
    "Native webview evidence expired before the sample was sealed.",
  );
  requireValue(
    before.main.pid === after.main.pid &&
      before.main.startTimeMicros === after.main.startTimeMicros &&
      before.main.path === after.main.path,
    "Runtime identity changed during sampling.",
  );
  const census = validateResponsibilityCensus(
    after.responsibilityCensus,
    after.main,
  );
  const mapped =
    census.availability === "lower_bound_only" ? census.processes : [];
  const hasPositiveMatches =
    census.availability === "lower_bound_only" && mapped.length > 0;
  const aggregate = hasPositiveMatches
    ? mapped.reduce(
        (a, p) => ({
          rssBytes: Math.max(a.rssBytes, p.rssBytes),
          openFileDescriptors: Math.max(
            a.openFileDescriptors,
            p.openFileDescriptors,
          ),
        }),
        { rssBytes: 0, openFileDescriptors: 0 },
      )
    : { rssBytes: null, openFileDescriptors: null };
  const mainChecks = [
    completeThreshold(
      "mainRssBytes",
      after.main.rssBytes,
      limits.value.maxMainRssBytes,
    ),
    completeThreshold(
      "mainOpenFileDescriptors",
      after.main.openFileDescriptors,
      limits.value.maxMainOpenFileDescriptors,
    ),
  ];
  const mappedWebKitChecks = [
    incompleteLowerBoundThreshold(
      "mappedWebKitAggregateRssBytes",
      aggregate.rssBytes,
      limits.value.maxMappedWebKitAggregateRssBytes,
      hasPositiveMatches,
    ),
    incompleteLowerBoundThreshold(
      "mappedWebKitAggregateOpenFileDescriptors",
      aggregate.openFileDescriptors,
      limits.value.maxMappedWebKitAggregateOpenFileDescriptors,
      hasPositiveMatches,
    ),
    incompleteLowerBoundThreshold(
      "mappedWebKitChildren",
      hasPositiveMatches ? 1 : null,
      limits.value.maxMappedWebKitChildren,
      hasPositiveMatches,
    ),
  ];
  const checks = [...mainChecks, ...mappedWebKitChecks];
  const mainStatus = completeStatus(mainChecks);
  const mappedWebKitStatus = incompleteStatus(mappedWebKitChecks);
  return {
    schemaVersion: 1,
    purpose: "dakia-native-l013-resource-sample-v1",
    metricDefinitionVersion: 4,
    capturedAt,
    phase,
    run: {
      runId: owner.marker.runId,
      runRoot: owner.runRoot,
      ownerMarkerSha256: owner.markerSha256,
    },
    provenance: identity.provenance,
    identity: {
      recordSha256: identitySha,
      pid: identity.pid,
      startTimeMicros: identity.startTimeMicros,
      executablePath: identity.executablePath,
      executableSha256: identity.executableSha256,
      bundleIdentifier: identity.bundleIdentifier,
    },
    collection: {
      api: "macOS libproc PID census plus private responsible-process SPI via ctypes",
      allPidsEnumerated: census.availability === "lower_bound_only",
      argvEnumerated: false,
      environmentEnumerated: false,
      unrelatedProcessFilesRead: false,
      unrelatedProcessIdentitiesPersisted: false,
      uiAccessUsed: false,
      signalsSent: false,
    },
    main: after.main,
    mappedWebKit: {
      mapping:
        "bounded all-PID census filtered by private responsibility_get_pid_responsible_for_pid, exact WebKit service identity, UID, and stable PID start identity",
      coverage: "responsibility_mapped_sample_window_peak_lower_bound",
      allWebKitMemoryClaimed: false,
      limitation:
        "Per-process measurements are sequential. Aggregate fields use maxima of individual observations, so they bound the sample-window peak without summing mutually exclusive process peaks. The child-count lower bound is one when any process is observed. The private responsibility SPI has no supported completeness contract. Stable numeric PID snapshots cannot exclude an unmatched PID being reused between checks. Positive matches remain lower bounds and never establish a full WebKit cohort pass.",
      metricAvailability: hasPositiveMatches
        ? "lower_bound_only"
        : "unavailable",
      processes: mapped,
      census: {
        availability: census.availability,
        reason: census.reason,
        enumeratedPidCount: census.enumeratedPidCount,
        responsibilityMatchedPidCount: census.responsibilityMatchedPidCount,
        acceptedWebKitProcessCount: census.acceptedWebKitProcessCount,
        unrelatedProcessIdentitiesPersisted: false,
      },
      aggregation:
        "maximum_individual_process_observation_bounds_sample_window_peak",
      aggregate,
    },
    limits: {
      recordSha256: limits.sha256,
      values: limits.value,
      checks,
      mainStatus,
      mappedWebKitStatus,
      status:
        mainStatus === "failed" || mappedWebKitStatus === "failed"
          ? "failed"
          : mappedWebKitStatus,
    },
    regression: { thresholdPercent: 20, status: "not_evaluated_in_raw_sample" },
  };
}

function sampleCommand(o) {
  requireOptions(o, [
    "run-root",
    "runtime-manifest",
    "runtime-manifest-sha",
    "limits",
    "limits-sha",
    "phase",
    "output",
  ]);
  requireValue(
    /^[a-z0-9][a-z0-9._-]{0,79}$/.test(o.phase),
    "Phase is invalid.",
  );
  const owner = validateOwner(o["run-root"]);
  const outputParent = realpathSync(dirname(o.output));
  requireValue(
    inside(owner.runRoot, outputParent, true),
    "Output must be below the owned run root.",
  );
  requireValue(
    !existsSync(o.output),
    "Output already exists; samples are exclusive-write.",
  );
  const observation = {
    webviewEvidence: o["webview-evidence"]
      ? { path: o["webview-evidence"], sha256: o["webview-evidence-sha"] }
      : null,
  };
  const approvedIdentity = loadIdentityRecord(
    owner,
    o["runtime-manifest"],
    o["runtime-manifest-sha"],
    { ...observation, live: true },
  );
  const first = readMainProcess(approvedIdentity.pid);
  const identity = validateIdentity(
    owner,
    o["runtime-manifest"],
    o["runtime-manifest-sha"],
    first.main,
    observation,
  );
  const limits = validateLimits(owner, o.limits, o["limits-sha"]);
  const second = readProcessCohort(identity.pid);
  validateIdentity(
    owner,
    o["runtime-manifest"],
    o["runtime-manifest-sha"],
    second.main,
    observation,
  );
  const sample = buildSample({
    owner,
    identity,
    identitySha: o["runtime-manifest-sha"],
    limits,
    phase: o.phase,
    before: first,
    after: second,
  });
  writeExclusive(o.output, sample);
  process.stdout.write(`${o.output}\n`);
  if (sample.limits.status === "failed") process.exitCode = 2;
}

function pinnedJson(pathInput, expectedSha, label) {
  requireValue(SHA_RE.test(expectedSha), `${label} SHA-256 is invalid.`);
  const path = realpathSync(pathInput);
  regular(path, label);
  requireValue(sha256(path) === expectedSha, `${label} hash changed.`);
  return { path, sha256: expectedSha, value: json(path) };
}

export function buildSummary(owner, listRecord) {
  const list = listRecord.value;
  requireValue(
    list.schemaVersion === 1 &&
      list.purpose === "dakia-native-l013-sample-list-v1" &&
      list.runId === owner.marker.runId,
    "Sample list does not bind this run.",
  );
  requireValue(
    Array.isArray(list.samples) &&
      list.samples.length > 0 &&
      list.samples.length <= 200,
    "Sample list must contain 1 to 200 pinned samples.",
  );
  requireValue(
    Array.isArray(list.phasePlan) &&
      list.phasePlan.length > 0 &&
      list.phasePlan.length <= 40,
    "Sample list requires an explicit phase plan.",
  );
  const phasePlan = new Map();
  for (const item of list.phasePlan) {
    requireValue(
      /^[a-z0-9][a-z0-9._-]{0,79}$/.test(item?.phase) &&
        Number.isSafeInteger(item.exactSamples) &&
        item.exactSamples > 0 &&
        !phasePlan.has(item.phase),
      "Phase plan entries must have a unique valid phase and positive exactSamples.",
    );
    phasePlan.set(item.phase, item.exactSamples);
  }
  requireValue(
    typeof list.identity?.path === "string" &&
      SHA_RE.test(list.identity.sha256),
    "Sample list requires a pinned approved identity record.",
  );
  requireValue(
    typeof list.limits?.path === "string" && SHA_RE.test(list.limits.sha256),
    "Sample list requires a pinned limits record.",
  );
  const identity = loadIdentityRecord(
    owner,
    list.identity.path,
    list.identity.sha256,
  );
  const limits = validateLimits(owner, list.limits.path, list.limits.sha256);
  const seenPins = new Set();
  function safeInteger(value, label, { positive = false } = {}) {
    requireValue(
      Number.isSafeInteger(value) && value >= (positive ? 1 : 0),
      `${label} is invalid.`,
    );
  }
  function validateProcess(p, label) {
    safeInteger(p?.pid, `${label} PID`, { positive: true });
    safeInteger(p?.ppid, `${label} PPID`);
    safeInteger(p?.uid, `${label} UID`);
    requireValue(
      typeof p.comm === "string" &&
        typeof p.name === "string" &&
        isAbsolute(p.path),
      `${label} strings are invalid.`,
    );
    requireValue(
      /^\d+$/.test(p.startTimeMicros),
      `${label} start time is invalid.`,
    );
    for (const key of [
      "rssBytes",
      "virtualBytes",
      "threadCount",
      "openFileDescriptors",
    ])
      safeInteger(p[key], `${label} ${key}`);
  }
  const samples = list.samples.map((entry, index) => {
    requireValue(
      typeof entry?.path === "string" && SHA_RE.test(entry.sha256),
      `Sample ${index} pin is invalid.`,
    );
    const pinned = pinnedJson(entry.path, entry.sha256, `Sample ${index}`);
    requireValue(
      inside(owner.runRoot, pinned.path),
      `Sample ${index} must be inside the owned run root.`,
    );
    requireValue(
      !seenPins.has(pinned.path),
      `Sample ${index} path is duplicated.`,
    );
    seenPins.add(pinned.path);
    const s = pinned.value;
    requireValue(
      s.schemaVersion === 1 &&
        s.purpose === "dakia-native-l013-resource-sample-v1" &&
        s.metricDefinitionVersion === 4 &&
        s.run?.runId === owner.marker.runId &&
        s.run?.runRoot === owner.runRoot &&
        s.run?.ownerMarkerSha256 === owner.markerSha256,
      `Sample ${index} does not bind this run.`,
    );
    requireValue(
      /^[a-z0-9][a-z0-9._-]{0,79}$/.test(s.phase) && phasePlan.has(s.phase),
      `Sample ${index} phase is outside the declared plan.`,
    );
    requireValue(
      typeof s.capturedAt === "string" &&
        Number.isFinite(Date.parse(s.capturedAt)),
      `Sample ${index} timestamp is invalid.`,
    );
    const sampleIdentity = loadIdentityRecord(
      owner,
      list.identity.path,
      list.identity.sha256,
      { webviewEvidence: s.provenance?.webviewEvidence },
    );
    const observationAge =
      Date.parse(s.capturedAt) -
      Date.parse(sampleIdentity.provenance.webviewObservedAt);
    requireValue(
      observationAge >= 0 && observationAge <= 120_000,
      `Sample ${index} webview evidence is stale or newer than the sample.`,
    );
    requireValue(
      JSON.stringify(s.provenance) ===
        JSON.stringify(sampleIdentity.provenance),
      `Sample ${index} runtime provenance changed.`,
    );
    requireValue(
      s.identity?.recordSha256 === list.identity.sha256 &&
        s.identity.pid === identity.pid &&
        s.identity.startTimeMicros === identity.startTimeMicros &&
        s.identity.executablePath === identity.executablePath &&
        s.identity.executableSha256 === identity.executableSha256 &&
        s.identity.bundleIdentifier === identity.bundleIdentifier,
      `Sample ${index} identity is forged or stale.`,
    );
    requireValue(
      s.collection?.api ===
        "macOS libproc PID census plus private responsible-process SPI via ctypes" &&
        s.collection.allPidsEnumerated ===
          (s.mappedWebKit?.census?.availability === "lower_bound_only") &&
        s.collection.argvEnumerated === false &&
        s.collection.environmentEnumerated === false &&
        s.collection.unrelatedProcessFilesRead === false &&
        s.collection.unrelatedProcessIdentitiesPersisted === false &&
        s.collection.uiAccessUsed === false &&
        s.collection.signalsSent === false,
      `Sample ${index} collection contract is invalid.`,
    );
    validateProcess(s.main, `Sample ${index} main`);
    requireValue(
      s.main.pid === identity.pid &&
        s.main.startTimeMicros === identity.startTimeMicros &&
        s.main.path === identity.executablePath &&
        s.main.uid === process.getuid(),
      `Sample ${index} main process does not match the approved identity.`,
    );
    requireValue(
      s.mappedWebKit?.allWebKitMemoryClaimed === false &&
        s.mappedWebKit?.coverage ===
          "responsibility_mapped_sample_window_peak_lower_bound",
      `Sample ${index} overclaims WebKit coverage.`,
    );
    requireValue(
      Array.isArray(s.mappedWebKit.processes) &&
        !Object.hasOwn(s.mappedWebKit, "observedDescendants") &&
        !Object.hasOwn(s.mappedWebKit, "excludedCandidates"),
      `Sample ${index} persists unapproved process evidence.`,
    );
    const persistedCensus = s.mappedWebKit.census;
    requireValue(
      persistedCensus &&
        ["lower_bound_only", "unavailable"].includes(
          persistedCensus.availability,
        ) &&
        Number.isSafeInteger(persistedCensus.enumeratedPidCount) &&
        persistedCensus.enumeratedPidCount >= 0 &&
        Number.isSafeInteger(persistedCensus.responsibilityMatchedPidCount) &&
        persistedCensus.responsibilityMatchedPidCount >= 0 &&
        Number.isSafeInteger(persistedCensus.acceptedWebKitProcessCount) &&
        persistedCensus.acceptedWebKitProcessCount >= 0 &&
        persistedCensus.acceptedWebKitProcessCount <=
          persistedCensus.responsibilityMatchedPidCount &&
        persistedCensus.responsibilityMatchedPidCount <=
          persistedCensus.enumeratedPidCount &&
        persistedCensus.acceptedWebKitProcessCount ===
          s.mappedWebKit.processes.length &&
        persistedCensus.unrelatedProcessIdentitiesPersisted === false,
      `Sample ${index} responsibility census evidence is invalid.`,
    );
    if (persistedCensus.availability === "unavailable")
      requireValue(
        CENSUS_UNAVAILABLE_REASONS.has(persistedCensus.reason) &&
          s.mappedWebKit.processes.length === 0,
        `Sample ${index} unavailable census is invalid.`,
      );
    else
      requireValue(
        persistedCensus.reason === null &&
          persistedCensus.enumeratedPidCount > 0,
        `Sample ${index} available census reason or count is invalid.`,
      );
    const mappedPids = new Set();
    for (const [pIndex, p] of s.mappedWebKit.processes.entries()) {
      validateProcess(p, `Sample ${index} WebKit process ${pIndex}`);
      safeInteger(
        p.responsiblePid,
        `Sample ${index} WebKit process ${pIndex} responsible PID`,
        { positive: true },
      );
      requireValue(
        p.pid !== s.main.pid &&
          !mappedPids.has(p.pid) &&
          webKitIdentity(p, s.main).accepted,
        `Sample ${index} contains an invalid mapped WebKit process.`,
      );
      mappedPids.add(p.pid);
    }
    const hasPositiveMatches = s.mappedWebKit.processes.length > 0;
    const aggregate = hasPositiveMatches
      ? s.mappedWebKit.processes.reduce(
          (a, p) => ({
            rssBytes: Math.max(a.rssBytes, p.rssBytes),
            openFileDescriptors: Math.max(
              a.openFileDescriptors,
              p.openFileDescriptors,
            ),
          }),
          { rssBytes: 0, openFileDescriptors: 0 },
        )
      : { rssBytes: null, openFileDescriptors: null };
    requireValue(
      s.mappedWebKit.metricAvailability ===
        (hasPositiveMatches ? "lower_bound_only" : "unavailable"),
      `Sample ${index} WebKit metric availability is invalid.`,
    );
    requireValue(
      s.mappedWebKit.aggregation ===
        "maximum_individual_process_observation_bounds_sample_window_peak",
      `Sample ${index} WebKit aggregation is invalid.`,
    );
    requireValue(
      s.mappedWebKit.aggregate?.rssBytes === aggregate.rssBytes &&
        s.mappedWebKit.aggregate?.openFileDescriptors ===
          aggregate.openFileDescriptors,
      `Sample ${index} WebKit aggregate is forged.`,
    );
    requireValue(
      s.limits?.recordSha256 === list.limits.sha256 &&
        JSON.stringify(s.limits.values) === JSON.stringify(limits.value),
      `Sample ${index} limits record is forged or stale.`,
    );
    const mainChecks = [
      completeThreshold(
        "mainRssBytes",
        s.main.rssBytes,
        limits.value.maxMainRssBytes,
      ),
      completeThreshold(
        "mainOpenFileDescriptors",
        s.main.openFileDescriptors,
        limits.value.maxMainOpenFileDescriptors,
      ),
    ];
    const mappedWebKitChecks = [
      incompleteLowerBoundThreshold(
        "mappedWebKitAggregateRssBytes",
        aggregate.rssBytes,
        limits.value.maxMappedWebKitAggregateRssBytes,
        hasPositiveMatches,
      ),
      incompleteLowerBoundThreshold(
        "mappedWebKitAggregateOpenFileDescriptors",
        aggregate.openFileDescriptors,
        limits.value.maxMappedWebKitAggregateOpenFileDescriptors,
        hasPositiveMatches,
      ),
      incompleteLowerBoundThreshold(
        "mappedWebKitChildren",
        hasPositiveMatches ? 1 : null,
        limits.value.maxMappedWebKitChildren,
        hasPositiveMatches,
      ),
    ];
    const recomputedChecks = [...mainChecks, ...mappedWebKitChecks];
    const mainStatus = completeStatus(mainChecks);
    const mappedWebKitStatus = incompleteStatus(mappedWebKitChecks);
    const recomputedStatus =
      mainStatus === "failed" || mappedWebKitStatus === "failed"
        ? "failed"
        : mappedWebKitStatus;
    requireValue(
      JSON.stringify(s.limits.checks) === JSON.stringify(recomputedChecks) &&
        s.limits.mainStatus === mainStatus &&
        s.limits.mappedWebKitStatus === mappedWebKitStatus &&
        s.limits.status === recomputedStatus,
      `Sample ${index} limit result is forged.`,
    );
    requireValue(
      s.regression?.thresholdPercent === 20 &&
        s.regression?.status === "not_evaluated_in_raw_sample",
      `Sample ${index} raw regression state is invalid.`,
    );
    return { ...pinned, value: s };
  });
  const actualPhaseCounts = new Map();
  for (const s of samples)
    actualPhaseCounts.set(
      s.value.phase,
      (actualPhaseCounts.get(s.value.phase) || 0) + 1,
    );
  requireValue(
    actualPhaseCounts.size === phasePlan.size &&
      [...phasePlan].every(
        ([phase, count]) => actualPhaseCounts.get(phase) === count,
      ),
    "Raw samples do not exactly satisfy the declared phase plan.",
  );
  const identityShas = new Set(
    samples.map((s) => s.value.identity.recordSha256),
  );
  const limitShas = new Set(samples.map((s) => s.value.limits.recordSha256));
  requireValue(
    identityShas.size === 1 &&
      limitShas.size === 1 &&
      [...identityShas][0] === list.identity.sha256 &&
      [...limitShas][0] === list.limits.sha256,
    "Samples do not share the pinned approved identity and limits record.",
  );
  const positivelyMappedSamples = samples.filter(
    (s) => s.value.mappedWebKit.processes.length > 0,
  );
  const peakMetrics = {
    mainRssBytes: Math.max(...samples.map((s) => s.value.main.rssBytes)),
    mainOpenFileDescriptors: Math.max(
      ...samples.map((s) => s.value.main.openFileDescriptors),
    ),
    mappedWebKitAggregateRssBytes:
      positivelyMappedSamples.length > 0
        ? Math.max(
            ...positivelyMappedSamples.map(
              (s) => s.value.mappedWebKit.aggregate.rssBytes,
            ),
          )
        : null,
    mappedWebKitAggregateOpenFileDescriptors:
      positivelyMappedSamples.length > 0
        ? Math.max(
            ...positivelyMappedSamples.map(
              (s) => s.value.mappedWebKit.aggregate.openFileDescriptors,
            ),
          )
        : null,
    mappedWebKitChildren: positivelyMappedSamples.length > 0 ? 1 : null,
  };
  requireValue(
    list.historicalBaseline === null || list.historicalBaseline === undefined,
    "Historical regression comparison is disabled until a separately approved compatible-baseline receipt exists.",
  );
  const regression = {
    thresholdPercent: 20,
    status: "not_evaluated_no_compatible_baseline",
    claim:
      "Historical baseline comparison was not performed. No new or reset baseline and no 20 percent regression pass is claimed.",
    assessedMetrics: [],
    excludedMetrics: [
      "historical comparison lacks an approved compatible-baseline receipt",
      "mapped WebKit metrics because private responsible-process mapping is not a complete cohort contract",
    ],
  };
  return {
    schemaVersion: 1,
    purpose: "dakia-native-l013-resource-summary-v1",
    metricDefinitionVersion: 4,
    createdAt: new Date().toISOString(),
    run: {
      runId: owner.marker.runId,
      runRoot: owner.runRoot,
      ownerMarkerSha256: owner.markerSha256,
    },
    provenance: identity.provenance,
    sampleList: { path: listRecord.path, sha256: listRecord.sha256 },
    samples: samples.map((s) => ({
      path: s.path,
      sha256: s.sha256,
      phase: s.value.phase,
      capturedAt: s.value.capturedAt,
    })),
    peakMetrics,
    absoluteLimits: {
      status: samples.some((s) => s.value.limits.status === "failed")
        ? "failed"
        : positivelyMappedSamples.length > 0
          ? "unassessable"
          : "unavailable",
      mainStatus: samples.some((s) => s.value.limits.mainStatus === "failed")
        ? "failed"
        : samples.every((s) => s.value.limits.mainStatus === "passed")
          ? "passed"
          : "observed_no_limit",
      mappedWebKitStatus: samples.some(
        (s) => s.value.limits.mappedWebKitStatus === "failed",
      )
        ? "failed"
        : positivelyMappedSamples.length > 0
          ? "unassessable"
          : "unavailable",
      limitsRecordSha256: [...limitShas][0],
      claim:
        "Main-process RSS and file-descriptor ceilings are complete. Mapped WebKit values never establish a full WebKit resource pass.",
    },
    regression,
    webKitScope: {
      coverage: "responsibility_mapped_sample_window_peak_lower_bound",
      allWebKitMemoryClaimed: false,
      metricAvailability:
        positivelyMappedSamples.length > 0 ? "lower_bound_only" : "unavailable",
      limitation:
        "Private responsible-process mapping improves attribution for observed candidates but does not prove a complete WebKit/XPC cohort. Below-ceiling lower bounds are unassessable; above-ceiling lower bounds fail.",
    },
  };
}

function summarizeCommand(o) {
  requireOptions(o, ["run-root", "sample-list", "sample-list-sha", "output"]);
  const owner = validateOwner(o["run-root"]);
  const outputParent = realpathSync(dirname(o.output));
  requireValue(
    inside(owner.runRoot, outputParent, true),
    "Summary output must be below the owned run root.",
  );
  requireValue(
    !existsSync(o.output),
    "Summary output already exists; evidence is exclusive-write.",
  );
  const listRecord = pinnedJson(
    o["sample-list"],
    o["sample-list-sha"],
    "Sample list",
  );
  requireValue(
    inside(owner.runRoot, listRecord.path),
    "Sample list must be inside the owned run root.",
  );
  const summary = buildSummary(owner, listRecord);
  writeExclusive(o.output, summary);
  process.stdout.write(`${o.output}\n`);
  process.exitCode = summaryExitCode(summary);
}

export function summaryExitCode(summary) {
  requireValue(
    ["failed", "unavailable", "unassessable"].includes(
      summary?.absoluteLimits?.status,
    ),
    "Summary absolute-limit status is invalid.",
  );
  if (
    summary.absoluteLimits.status === "failed" ||
    summary.regression?.status === "regressed"
  )
    return 2;
  return 3;
}

function main() {
  const [command, ...rest] = process.argv.slice(2);
  const options = parseOptions(rest);
  if (command === "sample") sampleCommand(options);
  else if (command === "summarize") summarizeCommand(options);
  else fail("Usage: collector sample|summarize [options]", 64);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  try {
    main();
  } catch (e) {
    process.stderr.write(`${e.message}\n`);
    process.exitCode = e.exitCode || 1;
  }
}
