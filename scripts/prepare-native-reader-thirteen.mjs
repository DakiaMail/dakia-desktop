#!/usr/bin/env node
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { chmod, lstat, readFile, realpath, writeFile } from "node:fs/promises";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { FIXTURES } from "./native-reader-thirteen-overlay.mjs";

const ownedRoot = dirname(new URL(import.meta.url).pathname);
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const runtimeHelperPaths = [
  "scripts/native-reader-thirteen-overlay.mjs",
  "scripts/seed-native-reader-thirteen-fixtures.mjs",
  "scripts/native-fictional-imap-proxy.mjs",
  "scripts/prepare-native-reader-thirteen.mjs",
  "scripts/seal-native-reader-thirteen.mjs",
];

function requireManifestEntry(manifest, path) {
  const entry = manifest.files.find((item) => item.path === path);
  if (!entry)
    throw new Error(
      `frozen source manifest omits native Reader input: ${path}`,
    );
  return entry;
}

function replaceOnce(source, before, after, label) {
  if (source.split(before).length !== 2)
    throw new Error(`source harness anchor is not unique: ${label}`);
  return source.replace(before, after);
}

export function transformHarness(
  stock,
  {
    sourceRoot,
    manifestPath,
    manifestSha256,
    manifest,
    overlayPath,
    overlaySourcePath,
    overlaySha256,
    seederPath,
    seederSourcePath,
    seederSha256,
  },
) {
  const runtimeSourceBinding = `owned_overlay=${JSON.stringify(overlayPath)}
owned_overlay_sha256=${JSON.stringify(overlaySha256)}
owned_seeder=${JSON.stringify(seederPath)}
owned_seeder_sha256=${JSON.stringify(seederSha256)}
node --input-type=module - "$owned_overlay" "$owned_overlay_sha256" "$owned_seeder" "$owned_seeder_sha256" <<'NATIVE_READER_OWNED_INPUTS'
import { createHash } from 'node:crypto';
import { lstat, readFile, realpath } from 'node:fs/promises';
const [overlayPath, overlaySha256, seederPath, seederSha256] = process.argv.slice(2);
for (const [path, expectedSha256, label] of [[overlayPath, overlaySha256, 'overlay'], [seederPath, seederSha256, 'seeder']]) {
  const info = await lstat(path);
  if (!info.isFile() || info.isSymbolicLink() || (process.getuid?.() !== undefined && info.uid !== process.getuid()) || await realpath(path) !== path) throw new Error(\`native Reader owned \${label} is unsafe\`);
  if (createHash('sha256').update(await readFile(path)).digest('hex') !== expectedSha256) throw new Error(\`native Reader owned \${label} changed after review\`);
}
NATIVE_READER_OWNED_INPUTS
repo_root=${JSON.stringify(sourceRoot)}
repo_root="$(cd -P "$repo_root" && pwd -P)"
frozen_source_manifest=${JSON.stringify(manifestPath)}
frozen_source_manifest_sha256=${JSON.stringify(manifestSha256)}
frozen_source_digest=${JSON.stringify(manifest.digest)}
frozen_source_file_count=${manifest.fileCount}
node --input-type=module - "$repo_root" "$frozen_source_manifest" "$frozen_source_manifest_sha256" "$frozen_source_digest" "$frozen_source_file_count" <<'NATIVE_READER_THIRTEEN_SOURCE_BINDING'
import { createHash } from 'node:crypto';
import { readFile, realpath } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
const [rootArg, manifestArg, expectedManifestSha, expectedDigest, expectedCountText] = process.argv.slice(2);
const root = await realpath(rootArg);
const manifestPath = await realpath(manifestArg);
if (root !== ${JSON.stringify(sourceRoot)} || manifestPath !== ${JSON.stringify(manifestPath)}) throw new Error('native Reader source or manifest canonical path changed');
const bytes = await readFile(manifestPath);
if (createHash('sha256').update(bytes).digest('hex') !== expectedManifestSha) throw new Error('native Reader frozen manifest changed');
const manifest = JSON.parse(bytes);
const expectedCount = Number(expectedCountText);
if (Object.keys(manifest).sort().join(',') !== 'algorithm,digest,fileCount,files' || manifest.algorithm !== 'sha256-path-and-content-manifest' || manifest.digest !== expectedDigest || manifest.fileCount !== expectedCount || manifest.files?.length !== expectedCount) throw new Error('native Reader frozen manifest contract changed');
process.chdir(root);
const { sourceSnapshot } = await import(pathToFileURL(root + '/scripts/run-mail-safety.mjs'));
const live = sourceSnapshot();
if (live?.algorithm !== manifest.algorithm || live?.digest !== expectedDigest || live?.fileCount !== expectedCount) throw new Error('native Reader live source differs from the frozen source');
NATIVE_READER_THIRTEEN_SOURCE_BINDING
for native_reader_arg in "$@"; do
  if [[ "$native_reader_arg" == '--attachment-interruption' ]]; then
    printf '%s cannot be combined with the thirteen-fixture Reader overlay. Run attachment interruption separately.\\n' '--attachment-interruption' >&2
    exit 64
  fi
done`;
  let harness = replaceOnce(
    stock,
    'repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"',
    runtimeSourceBinding,
    "source root and runtime binding",
  );
  harness = replaceOnce(
    harness,
    'fixture_pid=""\nproxy_pid=""\nattachment_interruption_pid=""',
    'fixture_pid=""\nproxy_pid=""\nattachment_interruption_pid=""\nnative_reader_thirteen_overlay_pid=""',
    "overlay process variable",
  );
  harness = replaceOnce(
    harness,
    '  stop_owned_child "$attachment_interruption_pid"\n  attachment_interruption_pid=""',
    '  stop_owned_child "$attachment_interruption_pid"\n  attachment_interruption_pid=""\n  stop_owned_child "$native_reader_thirteen_overlay_pid"\n  native_reader_thirteen_overlay_pid=""',
    "overlay cleanup",
  );
  harness = replaceOnce(
    harness,
    `  printf '%s\\n' 'dakia-native-mail-safety-fixture-v1' > "$profile_root/.dakia-native-mail-safety-profile"
)

openssl req`,
    `  printf '%s\\n' 'dakia-native-mail-safety-fixture-v1' > "$profile_root/.dakia-native-mail-safety-profile"
)
owned_overlay_run="$run_root/native-reader-thirteen-overlay.mjs"
owned_seeder_run="$run_root/seed-native-reader-thirteen-fixtures.mjs"
owned_inputs_receipt="$run_root/native-reader-thirteen-owned-inputs.json"
node --input-type=module - "$run_root" "$owned_overlay" "$owned_overlay_sha256" "$owned_overlay_run" "$owned_seeder" "$owned_seeder_sha256" "$owned_seeder_run" "$owned_inputs_receipt" <<'NATIVE_READER_COPY_OWNED_INPUTS'
import { createHash } from 'node:crypto';
import { constants } from 'node:fs';
import { chmod, copyFile, lstat, readFile, realpath, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
const [root, overlaySource, overlaySha256, overlayCopy, seederSource, seederSha256, seederCopy, receipt] = process.argv.slice(2);
if (await realpath(root) !== root || overlayCopy !== join(root, 'native-reader-thirteen-overlay.mjs') || seederCopy !== join(root, 'seed-native-reader-thirteen-fixtures.mjs') || receipt !== join(root, 'native-reader-thirteen-owned-inputs.json')) throw new Error('native Reader owned copy paths changed');
for (const [source, expectedSha256, destination, label] of [[overlaySource, overlaySha256, overlayCopy, 'overlay'], [seederSource, seederSha256, seederCopy, 'seeder']]) {
  const sourceInfo = await lstat(source);
  if (!sourceInfo.isFile() || sourceInfo.isSymbolicLink() || (process.getuid?.() !== undefined && sourceInfo.uid !== process.getuid()) || await realpath(source) !== source) throw new Error(\`native Reader owned \${label} source is unsafe\`);
  const bytes = await readFile(source);
  if (createHash('sha256').update(bytes).digest('hex') !== expectedSha256) throw new Error(\`native Reader owned \${label} changed before its run copy\`);
  await copyFile(source, destination, constants.COPYFILE_EXCL);
  await chmod(destination, 0o600);
  if (createHash('sha256').update(await readFile(destination)).digest('hex') !== expectedSha256) throw new Error(\`native Reader owned \${label} run copy changed\`);
}
await writeFile(receipt, JSON.stringify({ schemaVersion: 1, overlay: { path: overlayCopy, sourcePath: ${JSON.stringify(overlaySourcePath)}, sha256: overlaySha256 }, seeder: { path: seederCopy, sourcePath: ${JSON.stringify(seederSourcePath)}, sha256: seederSha256 } }, null, 2) + '\\n', { flag: 'wx', mode: 0o600 });
NATIVE_READER_COPY_OWNED_INPUTS

openssl req`,
    "owned runtime input copies",
  );
  harness = replaceOnce(
    harness,
    "PYTHON\n\nattachment_interruption_binding=''",
    `PYTHON

node "$owned_seeder_run" "$repo_root" "$smtp_port" "$certificate_root/ca.crt"
python3 - "$imap_port" "$certificate_root/ca.crt" <<'NATIVE_READER_THIRTEEN_MAILBOX'
import imaplib, re, ssl, sys
context = ssl.create_default_context(cafile=sys.argv[2])
with imaplib.IMAP4_SSL('127.0.0.1', int(sys.argv[1]), ssl_context=context, timeout=10) as client:
    client.login('fictional-reader@localhost', 'synthetic-only-password')
    status, count = client.select('INBOX', readonly=True)
    if status != 'OK' or count != [b'16']:
        raise RuntimeError('Native Reader fixture does not have exactly sixteen server-side messages')
    status, data = client.uid('fetch', '1:*', '(UID FLAGS)')
    observed = {int(match.group(1)) for row in data if isinstance(row, bytes) for match in re.finditer(rb'\\bUID (\\d+)\\b', row)}
    if status != 'OK' or observed != set(range(1, 17)):
        raise RuntimeError('Native Reader fixture UIDs are not the deterministic range 1 through 16')
    if any(re.search(rb'\\\\Seen(?:[ )]|$)', row, re.IGNORECASE) for row in data if isinstance(row, bytes)):
        raise RuntimeError('Native Reader fixture setup unexpectedly marked mail as read')
print('Confirmed thirteen registered MIME fixtures as unread UIDs 4 through 16.')
NATIVE_READER_THIRTEEN_MAILBOX

attachment_interruption_binding=''`,
    "thirteen fixture seeding and read-neutral validation",
  );
  harness = replaceOnce(
    harness,
    `if [[ "$prepare_only" == "true" && "$attachment_interruption" != true ]]; then
  printf 'Fictional provider fixture accepted normal and malformed MIME messages.\\n'
  exit 0
fi

backend_imap_port="$imap_port"`,
    `backend_imap_port="$imap_port"`,
    "prepare-only must pass both proxy readiness checks",
  );
  const outerProxy = `fi
node "$repo_root/scripts/native-fictional-imap-proxy.mjs" \\
  --root "$run_root" --backend-port "$backend_imap_port" >"$run_root/imap-proxy.log" 2>&1 &`;
  const layeredProxy = `fi
real_backend_imap_port="$backend_imap_port"
node "$owned_overlay_run" \\
  --root "$run_root" --source-root "$repo_root" --backend-port "$real_backend_imap_port" \\
  --certificate "$certificate_root/tls.crt" --key "$certificate_root/tls.key" \\
  --ca "$certificate_root/ca.crt" >"$run_root/native-reader-thirteen-overlay.log" 2>&1 &
native_reader_thirteen_overlay_pid=$!
for _ in $(seq 1 30); do
  if [[ -f "$run_root/native-reader-thirteen-overlay.json" ]]; then break; fi
  if ! kill -0 "$native_reader_thirteen_overlay_pid" >/dev/null 2>&1; then break; fi
  sleep 1
done
[[ -f "$run_root/native-reader-thirteen-overlay.json" ]] || {
  printf 'Native Reader thirteen-fixture inner TLS overlay did not become ready.\\n' >&2
  exit 1
}
backend_imap_port="$(node --input-type=module - "$run_root/native-reader-thirteen-overlay.json" "$real_backend_imap_port" "$native_reader_thirteen_overlay_pid" <<'NATIVE_READER_THIRTEEN_OVERLAY_METADATA'
import { lstat, readFile, realpath } from 'node:fs/promises';
const [path, expectedBackendPort, expectedPid] = process.argv.slice(2);
const info = await lstat(path);
if (!info.isFile() || info.isSymbolicLink() || (info.mode & 0o777) !== 0o600 || (process.getuid?.() !== undefined && info.uid !== process.getuid())) throw new Error('native Reader overlay metadata is unsafe');
if (await realpath(path) !== path) throw new Error('native Reader overlay metadata path is not canonical');
const report = JSON.parse(await readFile(path, 'utf8'));
if (report.schemaVersion !== 1 || report.mode !== 'native-reader-thirteen-inner-tls' || report.backendPort !== Number(expectedBackendPort) || report.pid !== Number(expectedPid)) throw new Error('native Reader overlay identity changed');
if (!Number.isInteger(report.port) || report.port < 1 || report.port > 65535 || !Array.isArray(report.fixtures) || report.fixtures.length !== 13) throw new Error('native Reader overlay contract is invalid');
const expectedUids = Array.from({ length: 13 }, (_, index) => index + 4);
if (JSON.stringify(report.fixtures.map(({ uid }) => uid)) !== JSON.stringify(expectedUids)) throw new Error('native Reader overlay UID range changed');
process.stdout.write(String(report.port));
NATIVE_READER_THIRTEEN_OVERLAY_METADATA
)"
node "$repo_root/scripts/native-fictional-imap-proxy.mjs" \\
  --root "$run_root" --backend-port "$backend_imap_port" >"$run_root/imap-proxy.log" 2>&1 &`;
  harness = replaceOnce(
    harness,
    outerProxy,
    layeredProxy,
    "stock controlled outer proxy",
  );
  harness = replaceOnce(
    harness,
    "printf 'Fictional provider fixture and attachment interruption proxies are ready.\\n'",
    "printf 'Fictional provider fixture and native Reader thirteen-fixture proxies are ready.\\n'",
    "prepare-only success text",
  );
  return harness;
}

export function requireExternalOutputPath(sourceRoot, output) {
  const pathFromSource = relative(sourceRoot, output);
  if (
    pathFromSource === "" ||
    (!pathFromSource.startsWith(`..${sep}`) && pathFromSource !== "..")
  )
    throw new Error(
      "generated harness must be outside the source checkout and supplied as an explicit runtime path",
    );
}

export async function resolveExternalOutputPath(sourceRoot, output) {
  const parent = await realpath(dirname(output));
  const canonicalOutput = join(parent, basename(output));
  requireExternalOutputPath(sourceRoot, canonicalOutput);
  return canonicalOutput;
}

async function validateFrozenSource(
  sourceRoot,
  manifestPath,
  expectedManifestSha256,
) {
  const manifestBytes = await readFile(manifestPath);
  if (sha256(manifestBytes) !== expectedManifestSha256)
    throw new Error("frozen source manifest digest changed");
  const manifest = JSON.parse(manifestBytes);
  if (
    Object.keys(manifest).sort().join(",") !==
      "algorithm,digest,fileCount,files" ||
    manifest.algorithm !== "sha256-path-and-content-manifest" ||
    !Array.isArray(manifest.files) ||
    manifest.fileCount !== manifest.files.length ||
    !/^[a-f0-9]{64}$/.test(manifest.digest)
  )
    throw new Error("frozen source manifest contract is invalid");
  const paths = new Set();
  const recomputed = createHash("sha256");
  for (const item of manifest.files) {
    if (
      !item ||
      item.state !== "present" ||
      typeof item.path !== "string" ||
      item.path.startsWith("/") ||
      item.path.split("/").includes("..") ||
      !/^[a-f0-9]{64}$/.test(item.sha256) ||
      !Number.isInteger(item.bytes) ||
      item.bytes < 0 ||
      paths.has(item.path)
    )
      throw new Error("frozen source manifest entry is invalid");
    paths.add(item.path);
    const path = join(sourceRoot, item.path);
    const info = await lstat(path);
    if (
      !info.isFile() ||
      info.isSymbolicLink() ||
      info.size !== item.bytes ||
      sha256(await readFile(path)) !== item.sha256
    )
      throw new Error(`source differs from frozen manifest: ${item.path}`);
    recomputed
      .update(item.path)
      .update("\0")
      .update(Buffer.from(item.sha256, "hex"))
      .update("\0");
  }
  if (JSON.stringify([...paths]) !== JSON.stringify([...paths].sort()))
    throw new Error("frozen source manifest paths are not sorted");
  for (const requiredPath of [
    "scripts/native-provider-reader-acceptance.sh",
    "scripts/run-mail-safety.mjs",
    ...runtimeHelperPaths,
    ...FIXTURES.map(([, , path]) => path),
  ]) {
    if (!paths.has(requiredPath))
      throw new Error(
        `frozen source manifest omits native Reader input: ${requiredPath}`,
      );
  }
  if (recomputed.digest("hex") !== manifest.digest)
    throw new Error(
      "frozen source manifest digest does not match its path-and-content entries",
    );
  const snapshotCode =
    'import { pathToFileURL } from "node:url"; process.chdir(process.argv[1]); const { sourceSnapshot } = await import(pathToFileURL(process.argv[1] + "/scripts/run-mail-safety.mjs")); process.stdout.write(JSON.stringify(sourceSnapshot()));';
  const live = JSON.parse(
    execFileSync(
      process.execPath,
      ["--input-type=module", "-e", snapshotCode, sourceRoot],
      { encoding: "utf8", maxBuffer: 32 * 1024 * 1024 },
    ),
  );
  if (
    live?.algorithm !== manifest.algorithm ||
    live?.digest !== manifest.digest ||
    live?.fileCount !== manifest.fileCount
  )
    throw new Error("live source snapshot differs from frozen source manifest");
  return manifest;
}

async function main() {
  const [sourceRootArg, manifestArg, expectedManifestSha256, outputArg] =
    process.argv.slice(2);
  if (
    !sourceRootArg ||
    !manifestArg ||
    !/^[a-f0-9]{64}$/.test(expectedManifestSha256 ?? "") ||
    !outputArg
  )
    throw new Error(
      "usage: prepare-native-reader-thirteen.mjs SOURCE_ROOT FROZEN_MANIFEST MANIFEST_SHA256 OUTPUT",
    );
  const sourceRoot = await realpath(sourceRootArg);
  const manifestPath = await realpath(manifestArg);
  const output = await resolveExternalOutputPath(
    sourceRoot,
    resolve(outputArg),
  );
  const manifest = await validateFrozenSource(
    sourceRoot,
    manifestPath,
    expectedManifestSha256,
  );
  const sourceScripts = await realpath(join(sourceRoot, "scripts"));
  if (ownedRoot !== sourceScripts)
    throw new Error(
      "native Reader preparation must execute from the frozen source checkout",
    );
  const stockPath = join(
    sourceRoot,
    "scripts/native-provider-reader-acceptance.sh",
  );
  const stock = await readFile(stockPath, "utf8");
  const stockEntry = requireManifestEntry(
    manifest,
    "scripts/native-provider-reader-acceptance.sh",
  );
  if (sha256(Buffer.from(stock)) !== stockEntry.sha256)
    throw new Error(
      "stock native harness changed after frozen-manifest validation",
    );
  const overlaySourcePath = "scripts/native-reader-thirteen-overlay.mjs";
  const seederSourcePath = "scripts/seed-native-reader-thirteen-fixtures.mjs";
  const overlayPath = await realpath(join(sourceRoot, overlaySourcePath));
  const seederPath = await realpath(join(sourceRoot, seederSourcePath));
  const overlaySha256 = sha256(await readFile(overlayPath));
  const seederSha256 = sha256(await readFile(seederPath));
  for (const [path, actual] of [
    [overlaySourcePath, overlaySha256],
    [seederSourcePath, seederSha256],
    [
      "scripts/prepare-native-reader-thirteen.mjs",
      sha256(await readFile(new URL(import.meta.url))),
    ],
  ]) {
    if (requireManifestEntry(manifest, path).sha256 !== actual)
      throw new Error(
        `native Reader helper differs from frozen source: ${path}`,
      );
  }
  const generated = transformHarness(stock, {
    sourceRoot,
    manifestPath,
    manifestSha256: expectedManifestSha256,
    manifest,
    overlayPath,
    overlaySourcePath,
    overlaySha256,
    seederPath,
    seederSourcePath,
    seederSha256,
  });
  await writeFile(output, generated, { flag: "wx", mode: 0o700 });
  await chmod(output, 0o700);
  const receipt = {
    schemaVersion: 1,
    contract: "dakia-native-reader-thirteen-preparation-v1",
    sourceRoot,
    sourceManifest: manifestPath,
    sourceManifestSha256: expectedManifestSha256,
    sourceAlgorithm: manifest.algorithm,
    sourceDigest: manifest.digest,
    sourceFileCount: manifest.fileCount,
    originalHarness: stockPath,
    originalHarnessSha256: sha256(Buffer.from(stock)),
    generatedHarness: output,
    generatedHarnessSha256: sha256(Buffer.from(generated)),
    overlayTopology:
      "stock-controlled-opaque-outer-to-thirteen-fixture-inner-tls-to-greenmail",
    fixtureCount: 13,
    fixtureUids: [4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
    fixtures: FIXTURES.map(([uid, id, relativePath, digest]) => ({
      uid,
      id,
      relativePath,
      sha256: digest,
    })),
    overlayPath,
    overlaySha256,
    seederPath,
    seederSha256,
  };
  if (
    sha256(await readFile(overlayPath)) !== overlaySha256 ||
    sha256(await readFile(seederPath)) !== seederSha256
  )
    throw new Error("native Reader runtime input changed during generation");
  const receiptPath = `${output}.receipt.json`;
  await writeFile(receiptPath, `${JSON.stringify(receipt, null, 2)}\n`, {
    flag: "wx",
    mode: 0o600,
  });
  console.log(JSON.stringify({ harness: output, receipt: receiptPath }));
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  main().catch((error) => {
    console.error(error.stack || error);
    process.exit(1);
  });
}
