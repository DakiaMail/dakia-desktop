#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'Usage: %s --outer-root PATH\n' "$0" >&2
  exit 64
}

outer_root=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --outer-root)
      [[ $# -ge 2 ]] || usage
      outer_root="$2"
      shift 2
      ;;
    *) usage ;;
  esac
done
[[ -n "$outer_root" ]] || usage

repo_root="/Users/arsalan/code/codex-worktrees/36f9/dakia-desktop"
build_target="/private/tmp/dakia-native-mail-safety-final-20261017/target"
target_triple="aarch64-apple-darwin"
fixture_marker="dakia-native-mail-safety-fixture-v1"
packaged_pid=""
packaged_root=""
evidence_root=""

copy_evidence() {
  [[ -n "$evidence_root" && -d "$evidence_root" ]] || return 0
  [[ -n "$packaged_root" && -d "$packaged_root" ]] || return 0
  local failed=0
  for name in \
    app.log acl-report.json app-inventory.json artifact-provenance.json \
    build-commands.json build-source-before.json build-source-after.json \
    outer-source-binding.json run-source-after.json counter-before.json \
    counter-after-report.json counter-after-exit.json \
    provider-ownership.json provider-after-exit.json signature.txt \
    packaged-exit-status.txt cua-handoff-timeout.json \
    tauri-packaged-acl.json vite-build.log cli-build.log tauri-build.log; do
    if [[ "$name" == "app.log" && -n "$packaged_pid" ]] && jobs -pr | rg -qx "$packaged_pid"; then
      continue
    fi
    [[ -f "$packaged_root/$name" ]] || continue
    local temporary="$evidence_root/.${name}.tmp.$$"
    if cp "$packaged_root/$name" "$temporary" && \
      [[ "$(shasum -a 256 "$packaged_root/$name" | awk '{print $1}')" == "$(shasum -a 256 "$temporary" | awk '{print $1}')" ]] && \
      mv -f "$temporary" "$evidence_root/$name"; then
      :
    else
      rm -f "$temporary"
      printf 'Could not atomically preserve evidence file: %s\n' "$name" >&2
      failed=1
    fi
  done
  return "$failed"
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$packaged_pid" ]] && jobs -pr | rg -qx "$packaged_pid"; then
    kill -TERM "$packaged_pid" >/dev/null 2>&1 || true
    wait "$packaged_pid" >/dev/null 2>&1 || true
  fi
  if ! copy_evidence; then status=1; fi
  [[ -z "$packaged_root" ]] || printf 'Packaged ACL working evidence: %s\n' "$packaged_root"
  if [[ -n "$evidence_root" ]]; then
    if [[ "$status" -eq 0 ]]; then
      printf 'Preserved packaged ACL evidence: %s\n' "$evidence_root"
    else
      printf 'Packaged ACL evidence retained for failed or incomplete run: %s\n' "$evidence_root" >&2
    fi
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in cargo codesign ditto docker file git install_name_tool lipo lsof node npx otool ps rg shasum; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'Required command is unavailable: %s\n' "$command" >&2
    exit 69
  }
done
[[ "$(uname -m)" == "arm64" ]] || {
  printf 'Packaged ACL acceptance requires the Apple Silicon test host.\n' >&2
  exit 69
}

outer_root="$(cd -P "$outer_root" 2>/dev/null && pwd -P)" || {
  printf 'Outer fictional fixture root does not exist.\n' >&2
  exit 66
}
[[ "$outer_root" == /private/var/* || "$outer_root" == /private/tmp/* ]] || {
  printf 'Outer fictional fixture root must be in the private temporary tree.\n' >&2
  exit 66
}
[[ "$(cat "$outer_root/.dakia-native-mail-safety-root" 2>/dev/null)" == "$fixture_marker" ]] || {
  printf 'Outer fictional fixture ownership marker is missing.\n' >&2
  exit 66
}
metadata="$outer_root/sleep-wake-fixture.json"
proxy_metadata="$outer_root/imap-proxy.json"
[[ -f "$metadata" && -f "$proxy_metadata" ]] || {
  printf 'Outer fictional fixture metadata is incomplete.\n' >&2
  exit 66
}

IFS=$'\t' read -r outer_app_pid outer_app_executable outer_app_sha256 proxy_pid proxy_port backend_port proxy_nonce < <(
node --input-type=module - "$outer_root" "$metadata" "$proxy_metadata" <<'NODE'
import { readFileSync, realpathSync } from 'node:fs';
const [root, metadataPath, proxyPath] = process.argv.slice(2);
const metadata = JSON.parse(readFileSync(metadataPath, 'utf8'));
const proxy = JSON.parse(readFileSync(proxyPath, 'utf8'));
const integer = (value, name) => {
  if (!Number.isInteger(value) || value < 1) throw new Error(`${name} is invalid`);
  return value;
};
const appExecutable = realpathSync(metadata.appExecutable);
if (!appExecutable.startsWith(`${root}/`)) throw new Error('outer app is outside root');
if (metadata.imapPort !== proxy.port || metadata.imapProxyPid !== proxy.pid)
  throw new Error('outer IMAP metadata disagrees');
const values = [
  integer(metadata.pid, 'app PID'),
  appExecutable,
  metadata.appSha256,
  integer(proxy.pid, 'proxy PID'),
  integer(proxy.port, 'proxy port'),
  integer(proxy.backendPort, 'backend port'),
  proxy.controlNonce,
];
if (values.some((value) => /[\t\r\n]/.test(String(value))))
  throw new Error('outer metadata contains control characters');
console.log(values.join('\t'));
NODE
)

[[ "$outer_app_sha256" =~ ^[a-f0-9]{64}$ && "$proxy_nonce" =~ ^[a-f0-9]{48}$ ]] || {
  printf 'Outer fixture hashes or proxy nonce are invalid.\n' >&2
  exit 66
}
[[ "$(shasum -a 256 "$outer_app_executable" | awk '{print $1}')" == "$outer_app_sha256" ]] || {
  printf 'Outer fixture application hash changed.\n' >&2
  exit 66
}
kill -0 "$outer_app_pid" >/dev/null 2>&1 || {
  printf 'Outer fixture application is no longer alive.\n' >&2
  exit 69
}
[[ "$(ps -p "$outer_app_pid" -o command= | sed 's/^ *//')" == "$outer_app_executable" ]] || {
  printf 'Outer fixture PID no longer owns its exact executable.\n' >&2
  exit 69
}
outer_harness_pid="$(ps -p "$outer_app_pid" -o ppid= | tr -d ' ')"
[[ "$outer_harness_pid" =~ ^[0-9]+$ ]] || exit 69
outer_harness_command="$(ps -p "$outer_harness_pid" -o command= | sed 's/^ *//')"
[[ "$outer_harness_command" == *"native-provider-reader-acceptance.sh"* ]] || {
  printf 'Could not bind the outer app to its owned harness process.\n' >&2
  exit 69
}
kill -0 "$proxy_pid" >/dev/null 2>&1 || {
  printf 'Owned fictional IMAP proxy is no longer alive.\n' >&2
  exit 69
}
proxy_command="$(ps -p "$proxy_pid" -o command= | sed 's/^ *//')"
expected_proxy_fragment="$repo_root/scripts/native-fictional-imap-proxy.mjs --root $outer_root --backend-port $backend_port"
[[ "$proxy_command" == *"$expected_proxy_fragment"* ]] || {
  printf 'IMAP proxy PID does not match the owned fixture command.\n' >&2
  exit 69
}
lsof -nP -a -p "$proxy_pid" -iTCP:"$proxy_port" -sTCP:LISTEN | rg -q "127\\.0\\.0\\.1:$proxy_port .*LISTEN" || {
  printf 'Owned fictional IMAP proxy is not listening on its recorded loopback port.\n' >&2
  exit 69
}

fixture_pid="$(lsof -nP -t -iTCP:48731 -sTCP:LISTEN | sort -u)"
[[ "$fixture_pid" =~ ^[0-9]+$ ]] || {
  printf 'Owned remote-content fixture listener is ambiguous or missing.\n' >&2
  exit 69
}
fixture_command="$(ps -p "$fixture_pid" -o command= | sed 's/^ *//')"
[[ "$fixture_command" == *"$repo_root/scripts/native-mail-safety-fixture-server.mjs --port 48731 --tls-probe-port 48732 --evidence $outer_root/network-requests.json"* ]] || {
  printf 'Remote-content listener does not belong to the outer fixture.\n' >&2
  exit 69
}
for fixture_port in 48731 48732; do
  lsof -nP -a -p "$fixture_pid" -iTCP:"$fixture_port" -sTCP:LISTEN | rg -q "127\\.0\\.0\\.1:$fixture_port .*LISTEN" || {
    printf 'Owned remote-content fixture is not listening on port %s.\n' "$fixture_port" >&2
    exit 69
  }
done

packaged_root="$(mktemp -d "$outer_root/packaged-acl.XXXXXX")"
packaged_root="$(cd -P "$packaged_root" && pwd -P)"
chmod 700 "$packaged_root"
evidence_root="$(mktemp -d /private/tmp/dakia-packaged-acl-evidence.XXXXXX)"
chmod 700 "$evidence_root"
profile_root="$packaged_root/profile"
downloads_root="$packaged_root/Downloads"
web_dist="$packaged_root/web-dist"
private_binaries="$packaged_root/binaries"
mkdir -p "$profile_root" "$downloads_root" "$private_binaries" "$build_target"
printf '%s\n' "$fixture_marker" > "$packaged_root/.dakia-native-mail-safety-root"
printf '%s\n' "$fixture_marker" > "$profile_root/.dakia-native-mail-safety-profile"
cp "$outer_root/tls/tls.der" "$packaged_root/tls.der"
outer_ca_sha="$(shasum -a 256 "$outer_root/tls/tls.der" | awk '{print $1}')"
copied_ca_sha="$(shasum -a 256 "$packaged_root/tls.der" | awk '{print $1}')"
[[ "$outer_ca_sha" == "$copied_ca_sha" ]] || exit 1

suffix="$(basename "$packaged_root" | tr -cd 'A-Za-z0-9' | tr '[:upper:]' '[:lower:]')"
product_name="DakiaFictionalPackagedAcl${suffix}"
bundle_identifier="dev.dakia.fictional-packaged-acl.${suffix}"
window_title="Dakia Fictional Packaged ACL ${suffix}"
tauri_overlay="$packaged_root/tauri-packaged-acl.json"
cargo_runner="$packaged_root/cargo-locked"
cat > "$cargo_runner" <<'RUNNER'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "build" ]]; then
  shift
  exec cargo build --locked "$@"
fi
exec cargo "$@"
RUNNER
chmod 700 "$cargo_runner"

node --input-type=module - "$tauri_overlay" "$web_dist" "$private_binaries/dakia" "$product_name" "$bundle_identifier" "$window_title" <<'NODE'
import { writeFileSync } from 'node:fs';
const [output, frontendDist, externalBin, productName, identifier, title] = process.argv.slice(2);
writeFileSync(output, `${JSON.stringify({
  productName,
  identifier,
  build: { beforeBuildCommand: '', beforeDevCommand: '', devUrl: null, frontendDist },
  app: { windows: [{ label: 'main', title, width: 1440, height: 900 }] },
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

snapshot_source() {
  local output="$1"
  node --input-type=module - "$repo_root" "$output" <<'NODE'
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const [root, output] = process.argv.slice(2);
process.chdir(root);
const { sourceSnapshot } = await import(pathToFileURL(`${root}/scripts/run-mail-safety.mjs`));
const source = sourceSnapshot();
if (!source) throw new Error('source snapshot unavailable');
const revision = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
writeFileSync(output, `${JSON.stringify({ revision, source }, null, 2)}\n`, { mode: 0o600 });
NODE
}

verify_source_snapshot() {
  local baseline="$1"
  local output="$2"
  snapshot_source "$output"
  node --input-type=module - "$baseline" "$output" <<'NODE'
import { readFileSync } from 'node:fs';
const before = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const after = JSON.parse(readFileSync(process.argv[3], 'utf8'));
if (before.revision !== after.revision || before.source.digest !== after.source.digest || before.source.fileCount !== after.source.fileCount)
  throw new Error('source changed during packaged ACL acceptance');
NODE
}

snapshot_source "$packaged_root/build-source-before.json"
node --input-type=module - "$outer_root/source-before-build.json" "$packaged_root/build-source-before.json" "$packaged_root/outer-source-binding.json" <<'NODE'
import { readFileSync, writeFileSync } from 'node:fs';
const [outerPath, currentPath, output] = process.argv.slice(2);
const outer = JSON.parse(readFileSync(outerPath, 'utf8'));
const current = JSON.parse(readFileSync(currentPath, 'utf8'));
if (outer.algorithm !== current.source.algorithm || outer.digest !== current.source.digest || outer.fileCount !== current.source.fileCount)
  throw new Error('packaged source does not match the live outer fixture source baseline');
writeFileSync(output, `${JSON.stringify({ schemaVersion: 1, status: 'passed', outerBaseline: outer, packagedBaseline: current, note: 'The outer harness records an exact source-content snapshot without a separate revision field. Exact digest and file-count equality binds both artifacts to the packaged baseline revision recorded here.' }, null, 2)}\n`, { mode: 0o600 });
NODE
node --input-type=module - "$packaged_root/build-commands.json" "$web_dist" "$build_target" "$target_triple" "$tauri_overlay" "$cargo_runner" <<'NODE'
import { writeFileSync } from 'node:fs';
const [output, webDist, targetDir, target, overlay, runner] = process.argv.slice(2);
writeFileSync(output, `${JSON.stringify({
  frontend: { command: 'node', args: ['node_modules/vite/bin/vite.js', 'build', '--config', 'apps/desktop/vite.config.ts', '--outDir', webDist, '--emptyOutDir'], environment: { VITE_NATIVE_MAIL_SAFETY_FIXTURE: '1', TAURI_DEBUG: '1' } },
  cli: { command: 'cargo', args: ['build', '--locked', '-p', 'dakia-cli', '--bin', 'dakia', '--target', target], environment: { CARGO_TARGET_DIR: targetDir } },
  desktop: { command: 'npx', args: ['tauri', 'build', '--debug', '--target', target, '--bundles', 'app', '--no-sign', '--ci', '--features', 'native-mail-safety-fixture', '--runner', runner, '--config', 'apps/desktop/src-tauri/tauri.conf.json', '--config', overlay], environment: { CARGO_TARGET_DIR: targetDir, ORT_LIB_LOCATION: 'apps/desktop/src-tauri/frameworks', ORT_PREFER_DYNAMIC_LINK: '1' } },
  signing: ['codesign nested ONNX dylib', 'codesign bundled CLI', 'codesign desktop executable', 'codesign app bundle', 'codesign --verify --deep --strict'],
}, null, 2)}\n`, { mode: 0o600 });
NODE

(cd "$repo_root" && \
  VITE_NATIVE_MAIL_SAFETY_FIXTURE=1 TAURI_DEBUG=1 \
  node node_modules/vite/bin/vite.js build --config apps/desktop/vite.config.ts \
    --outDir "$web_dist" --emptyOutDir) >"$packaged_root/vite-build.log" 2>&1

(cd "$repo_root" && CARGO_TARGET_DIR="$build_target" \
  cargo build --locked -p dakia-cli --bin dakia --target "$target_triple") >"$packaged_root/cli-build.log" 2>&1
cli_source="$build_target/$target_triple/debug/dakia"
[[ -x "$cli_source" ]] || exit 1
file "$cli_source" | rg -q 'Mach-O 64-bit executable arm64' || exit 1
expected_version="$(node -p "require('$repo_root/apps/desktop/src-tauri/tauri.conf.json').version")"
[[ "$("$cli_source" --version)" == "dakia $expected_version" ]] || {
  printf 'Fresh CLI version does not match the desktop package.\n' >&2
  exit 1
}
cp "$cli_source" "$private_binaries/dakia-$target_triple"
chmod 755 "$private_binaries/dakia-$target_triple"
staged_cli_sha="$(shasum -a 256 "$private_binaries/dakia-$target_triple" | awk '{print $1}')"

generated_app="$build_target/$target_triple/debug/bundle/macos/$product_name.app"
[[ ! -e "$generated_app" ]] || {
  printf 'Unique generated bundle path already exists in retained target.\n' >&2
  exit 73
}
(cd "$repo_root" && \
  ORT_LIB_LOCATION="$repo_root/apps/desktop/src-tauri/frameworks" \
  ORT_PREFER_DYNAMIC_LINK=1 \
  CARGO_TARGET_DIR="$build_target" \
  npx tauri build --debug --target "$target_triple" --bundles app --no-sign --ci \
    --features native-mail-safety-fixture --runner "$cargo_runner" \
    --config apps/desktop/src-tauri/tauri.conf.json --config "$tauri_overlay") >"$packaged_root/tauri-build.log" 2>&1
verify_source_snapshot "$packaged_root/build-source-before.json" "$packaged_root/build-source-after.json"
[[ -d "$generated_app" ]] || {
  printf 'Tauri did not create the exact unique packaged app path.\n' >&2
  exit 1
}

app_root="$packaged_root/$product_name.app"
[[ ! -e "$app_root" ]] || exit 73
ditto "$generated_app" "$app_root"
app_executable="$app_root/Contents/MacOS/dakia-desktop"
bundled_cli="$app_root/Contents/MacOS/dakia"
bundled_onnx="$app_root/Contents/Frameworks/libonnxruntime.1.23.2.dylib"
[[ -x "$app_executable" && -x "$bundled_cli" && -f "$bundled_onnx" ]] || {
  printf 'Packaged app is missing its desktop, fresh CLI, or ONNX runtime.\n' >&2
  exit 1
}
[[ "$(shasum -a 256 "$bundled_cli" | awk '{print $1}')" == "$staged_cli_sha" ]] || {
  printf 'Bundled CLI does not match the freshly built sidecar.\n' >&2
  exit 1
}
onnx_source="$repo_root/apps/desktop/src-tauri/frameworks/libonnxruntime.1.23.2.dylib"
[[ "$(shasum -a 256 "$bundled_onnx" | awk '{print $1}')" == "$(shasum -a 256 "$onnx_source" | awk '{print $1}')" ]] || {
  printf 'Bundled ONNX runtime does not match the prepared source asset.\n' >&2
  exit 1
}
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_root/Contents/Info.plist")" == "$bundle_identifier" ]] || exit 1
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app_root/Contents/Info.plist")" == "dakia-desktop" ]] || exit 1

for packaged_executable in "$app_executable" "$bundled_cli"; do
  if otool -L "$packaged_executable" | rg -q 'libonnxruntime' && \
    ! otool -l "$packaged_executable" | rg -F -A2 '@executable_path/../Frameworks' >/dev/null; then
      install_name_tool -add_rpath '@executable_path/../Frameworks' "$packaged_executable"
  fi
done

codesign --force --sign - "$bundled_onnx" >/dev/null
codesign --force --sign - "$bundled_cli" >/dev/null
codesign --force --sign - "$app_executable" >/dev/null
codesign --force --sign - "$app_root" >/dev/null
codesign --verify --deep --strict --verbose=2 "$app_root" >"$packaged_root/signature.txt" 2>&1
codesign -dv --verbose=4 "$app_root" >>"$packaged_root/signature.txt" 2>&1
[[ "$("$bundled_cli" --version)" == "dakia $expected_version" ]] || {
  printf 'Signed bundled CLI version does not match the desktop package.\n' >&2
  exit 1
}

file "$app_executable" | rg -q 'Mach-O 64-bit executable arm64' || exit 1
[[ "$(lipo -archs "$app_executable")" == "arm64" ]] || exit 1
file "$bundled_cli" | rg -q 'Mach-O 64-bit executable arm64' || exit 1
[[ "$(lipo -archs "$bundled_cli")" == "arm64" ]] || exit 1
file "$bundled_onnx" | rg -q 'Mach-O' || exit 1
lipo -archs "$bundled_onnx" | tr ' ' '\n' | rg -qx 'arm64' || exit 1
app_sha256="$(shasum -a 256 "$app_executable" | awk '{print $1}')"
cli_sha256="$(shasum -a 256 "$bundled_cli" | awk '{print $1}')"
onnx_sha256="$(shasum -a 256 "$bundled_onnx" | awk '{print $1}')"

node --input-type=module - "$app_root" "$packaged_root/app-inventory.json" <<'NODE'
import { createHash } from 'node:crypto';
import { lstatSync, readdirSync, readFileSync, readlinkSync, writeFileSync } from 'node:fs';
import { join, relative } from 'node:path';
const [root, output] = process.argv.slice(2);
const files = [];
function walk(path) {
  const info = lstatSync(path);
  if (info.isDirectory()) for (const name of readdirSync(path).sort()) walk(join(path, name));
  else if (info.isSymbolicLink()) files.push({ path: relative(root, path), symlink: readlinkSync(path) });
  else if (info.isFile()) files.push({ path: relative(root, path), bytes: info.size, mode: info.mode & 0o777, sha256: createHash('sha256').update(readFileSync(path)).digest('hex') });
}
walk(root);
writeFileSync(output, `${JSON.stringify({ schemaVersion: 1, root, files }, null, 2)}\n`, { mode: 0o600 });
NODE

node --input-type=module - "$packaged_root/artifact-provenance.json" "$packaged_root/build-source-before.json" "$tauri_overlay" "$web_dist" "$app_root" "$app_executable" "$app_sha256" "$bundled_cli" "$cli_sha256" "$staged_cli_sha" "$bundled_onnx" "$onnx_sha256" "$bundle_identifier" "$outer_ca_sha" <<'NODE'
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join, relative } from 'node:path';
const [output, sourcePath, overlay, webRoot, appRoot, executable, appSha256, cli, cliSha256, stagedCliSha256, onnx, onnxSha256, bundleIdentifier, caSha256] = process.argv.slice(2);
const web = [];
function walk(path) {
  for (const name of readdirSync(path).sort()) {
    const child = join(path, name);
    if (statSync(child).isDirectory()) walk(child);
    else web.push({ path: relative(webRoot, child), sha256: createHash('sha256').update(readFileSync(child)).digest('hex') });
  }
}
walk(webRoot);
writeFileSync(output, `${JSON.stringify({ schemaVersion: 1, source: JSON.parse(readFileSync(sourcePath, 'utf8')), overlay: JSON.parse(readFileSync(overlay, 'utf8')), web, appRoot, executable: { path: executable, sha256: appSha256 }, cli: { path: cli, sha256: cliSha256, matchedFreshBuildBeforeRpathAndSigning: stagedCliSha256 }, onnx: { path: onnx, sha256: onnxSha256 }, bundleIdentifier, caSha256, releaseBoundary: 'ad-hoc fixture-enabled debug app; not Developer ID signed, notarized, stapled, updater-signed, installed, or published' }, null, 2)}\n`, { mode: 0o600 });
NODE

node --input-type=module - "http://127.0.0.1:48731/requests" "$packaged_root/counter-before.json" <<'NODE'
import { writeFileSync } from 'node:fs';
const response = await fetch(process.argv[2]);
if (!response.ok) throw new Error(`counter HTTP ${response.status}`);
const value = await response.json();
if (value.totalContacts !== 0) throw new Error('remote-content fixture is not at the required zero-contact boundary');
writeFileSync(process.argv[3], `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
NODE

node --input-type=module - "$backend_port" "$proxy_port" "$outer_root/tls/ca.crt" "$packaged_root/provider-ownership.json" <<'NODE'
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import tls from 'node:tls';
const [backendPortRaw, proxyPortRaw, caPath, output] = process.argv.slice(2);
const backendPort = Number(backendPortRaw);
const proxyPort = Number(proxyPortRaw);
const lines = execFileSync('docker', ['ps', '--filter', 'ancestor=greenmail/standalone:2.1.14', '--format', '{{json .}}'], { encoding: 'utf8' }).trim().split('\n').filter(Boolean).map(JSON.parse);
const token = `127.0.0.1:${backendPort}->3993/tcp`;
const matches = lines.filter((entry) => String(entry.Ports).split(', ').includes(token));
if (matches.length !== 1) throw new Error('exact owned GreenMail backend container is not running');
await new Promise((resolve, reject) => {
  const socket = tls.connect({ host: '127.0.0.1', port: proxyPort, servername: 'localhost', ca: readFileSync(caPath), rejectUnauthorized: true });
  let text = '';
  const timer = setTimeout(() => socket.destroy(new Error('owned IMAP greeting timeout')), 5000);
  socket.on('data', (chunk) => {
    text += chunk;
    if (text.includes('\n')) {
      clearTimeout(timer);
      socket.end();
      if (/^\* OK/i.test(text)) resolve();
      else reject(new Error('owned IMAP greeting is invalid'));
    }
  });
  socket.on('error', (error) => { clearTimeout(timer); reject(error); });
});
writeFileSync(output, `${JSON.stringify({ schemaVersion: 1, container: matches[0], backendPort, proxyPort, tlsAuthenticated: true }, null, 2)}\n`, { mode: 0o600 });
NODE

[[ "$(shasum -a 256 "$packaged_root/tls.der" | awk '{print $1}')" == "$outer_ca_sha" ]] || exit 1
kill -0 "$outer_app_pid" >/dev/null 2>&1 || exit 1
kill -0 "$outer_harness_pid" >/dev/null 2>&1 || exit 1
kill -0 "$proxy_pid" >/dev/null 2>&1 || exit 1
[[ "$(ps -p "$outer_app_pid" -o command= | sed 's/^ *//')" == "$outer_app_executable" ]] || exit 1
[[ "$(ps -p "$outer_harness_pid" -o command= | sed 's/^ *//')" == "$outer_harness_command" ]] || exit 1
[[ "$(ps -p "$proxy_pid" -o command= | sed 's/^ *//')" == "$proxy_command" ]] || exit 1
lsof -nP -a -p "$proxy_pid" -iTCP:"$proxy_port" -sTCP:LISTEN | rg -q "127\\.0\\.0\\.1:$proxy_port .*LISTEN" || exit 1
[[ "$(ps -p "$fixture_pid" -o command= | sed 's/^ *//')" == "$fixture_command" ]] || exit 1
for fixture_port in 48731 48732; do
  lsof -nP -a -p "$fixture_pid" -iTCP:"$fixture_port" -sTCP:LISTEN | rg -q "127\\.0\\.0\\.1:$fixture_port .*LISTEN" || exit 1
done

env -i \
  HOME="$HOME" USER="${USER:-arsalan}" LOGNAME="${LOGNAME:-arsalan}" \
  PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin" \
  TMPDIR="${TMPDIR:-/tmp}" LANG="${LANG:-en_US.UTF-8}" \
  DAKIA_NATIVE_MAIL_SAFETY_FIXTURE=1 \
  DAKIA_NATIVE_MAIL_SAFETY_ROOT="$packaged_root" \
  DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR="$profile_root" \
  DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR="$downloads_root" \
  DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST=127.0.0.1 \
  DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT="$proxy_port" \
  DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME=localhost \
  DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH="$packaged_root/tls.der" \
  DAKIA_NATIVE_MAIL_SAFETY_PASSWORD=synthetic-only-password \
  DAKIA_NATIVE_MAIL_SAFETY_ACL_PROBE=1 \
  "$app_executable" >"$packaged_root/app.log" 2>&1 &
packaged_pid=$!

report="$packaged_root/native-acl-acceptance.json"
for _ in $(seq 1 240); do
  [[ -f "$report" ]] && break
  kill -0 "$packaged_pid" >/dev/null 2>&1 || break
  sleep 1
done
[[ -f "$report" ]] || {
  printf 'Packaged ACL app exited or timed out before writing evidence.\n' >&2
  exit 1
}
cp "$report" "$packaged_root/acl-report.json"

node --input-type=module - "$report" "$app_executable" "$app_sha256" "$packaged_pid" "$proxy_port" "$profile_root" <<'NODE'
import { readFileSync, realpathSync } from 'node:fs';
const [reportPath, executable, sha256, pidRaw, forbiddenPortRaw, profile] = process.argv.slice(2);
const report = JSON.parse(readFileSync(reportPath, 'utf8'));
const pid = Number(pidRaw);
const forbiddenPort = Number(forbiddenPortRaw);
if (report.schemaVersion !== 1 || report.status !== 'passed') throw new Error('packaged ACL report did not pass');
if (report.feature !== 'native-mail-safety-fixture' || report.probeFlag !== 'DAKIA_NATIVE_MAIL_SAFETY_ACL_PROBE') throw new Error('packaged ACL fixture identity is wrong');
if (realpathSync(report.profile) !== realpathSync(profile)) throw new Error('packaged ACL profile identity is wrong');
if (realpathSync(report.executable.path) !== realpathSync(executable) || report.executable.sha256 !== sha256 || report.executable.pid !== pid) throw new Error('packaged executable identity mismatch');
if (report.main?.url !== 'tauri://localhost/') throw new Error(`unexpected packaged main URL: ${report.main?.url}`);
if (report.main?.label !== 'main') throw new Error('packaged main window label is wrong');
if (report.localProbe?.url !== 'tauri://localhost/__dakia_native_acl_probe__') throw new Error(`unexpected packaged local probe URL: ${report.localProbe?.url}`);
if (report.localProbe?.label !== 'native-acl-local-probe' || report.remoteProbe?.label !== 'native-acl-remote-probe') throw new Error('packaged ACL probe labels are wrong');
const remote = new URL(report.remoteProbe?.url);
if (remote.protocol !== 'http:' || remote.hostname !== '127.0.0.1' || remote.pathname !== '/probe' || !remote.searchParams.get('nonce') || Number(remote.port) === forbiddenPort || [48731, 48732].includes(Number(remote.port))) throw new Error('remote probe did not use its distinct owned loopback HTTP origin');
const expectedChecks = ['localBridgeReachedAcl', 'localRegisteredCommandsDenied', 'mainReadOnlyBridge', 'noSideEffects', 'remoteOriginIsolated', 'staticProbeWindowScopesDenied'];
if (Object.keys(report.checks ?? {}).sort().join() !== expectedChecks.sort().join() || expectedChecks.some((key) => report.checks[key] !== true)) throw new Error('packaged ACL checks are incomplete');
const sideEffects = report.sideEffects ?? {};
if (sideEffects.downloadsUnchanged !== true || sideEffects.forgedWindowAbsent !== true || sideEffects.messageUnchanged !== true || sideEffects.openerCanaryHits !== 0 || sideEffects.settingsWindowAbsent !== true) throw new Error('packaged ACL side-effect oracle failed');
const registered = report.registeredCommandInventory ?? [];
const runtime = report.runtimeCommandInventory ?? [];
const staticOnly = report.staticOnlyCommands ?? [];
const unique = (values) => new Set(values).size === values.length;
if (![registered, runtime, staticOnly].every((values) => Array.isArray(values) && unique(values))) throw new Error('packaged ACL inventories are malformed');
if (new Set([...runtime, ...staticOnly]).size !== registered.length || registered.some((value) => !runtime.includes(value) && !staticOnly.includes(value))) throw new Error('packaged ACL command partition is incomplete');
if (report.localProbe?.result?.ipcAvailable !== true) throw new Error('local Wry IPC bridge was not reached');
NODE

node --input-type=module - "http://127.0.0.1:48731/requests" "$packaged_root/counter-after-report.json" <<'NODE'
import { writeFileSync } from 'node:fs';
const response = await fetch(process.argv[2]);
const value = await response.json();
if (!response.ok || value.totalContacts !== 0) throw new Error('packaged app contaminated the zero-contact privacy boundary');
writeFileSync(process.argv[3], `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
NODE

kill -0 "$packaged_pid" >/dev/null 2>&1 || exit 1
[[ "$(ps -p "$packaged_pid" -o command= | sed 's/^ *//')" == "$app_executable" ]] || exit 1
[[ "$(shasum -a 256 "$app_executable" | awk '{print $1}')" == "$app_sha256" ]] || exit 1
copy_evidence
for required_evidence in \
  acl-report.json app-inventory.json artifact-provenance.json \
  build-commands.json build-source-before.json build-source-after.json \
  outer-source-binding.json counter-before.json counter-after-report.json \
  provider-ownership.json signature.txt tauri-packaged-acl.json; do
  [[ -s "$evidence_root/$required_evidence" ]] || {
    printf 'Required preserved evidence is missing: %s\n' "$required_evidence" >&2
    exit 1
  }
  [[ "$(shasum -a 256 "$packaged_root/$required_evidence" | awk '{print $1}')" == "$(shasum -a 256 "$evidence_root/$required_evidence" | awk '{print $1}')" ]] || exit 1
done
printf '\nPackaged ACL acceptance report passed. CUA verification is still required.\n'
printf '  packaged PID: %s\n' "$packaged_pid"
printf '  exact executable: %s\n' "$app_executable"
printf '  executable SHA-256: %s\n' "$app_sha256"
printf '  bundle identifier: %s\n' "$bundle_identifier"
printf '  required live WebKit URL: tauri://localhost/\n'
printf '  ACL report: %s\n' "$report"
printf '  preserved evidence: %s\n' "$evidence_root"
printf 'After CUA proves the exact PID, executable, and URL, terminate only packaged PID %s.\n' "$packaged_pid"

cua_deadline=$((SECONDS + 600))
while jobs -pr | rg -qx "$packaged_pid"; do
  if (( SECONDS >= cua_deadline )); then
    node --input-type=module - "$packaged_root/cua-handoff-timeout.json" "$packaged_pid" "$app_executable" "$app_sha256" <<'NODE'
import { writeFileSync } from 'node:fs';
const [output, pid, executable, sha256] = process.argv.slice(2);
writeFileSync(output, `${JSON.stringify({ schemaVersion: 1, status: 'failed', reason: 'CUA handoff exceeded 600 seconds', pid: Number(pid), executable, sha256 }, null, 2)}\n`, { mode: 0o600 });
NODE
    printf 'CUA handoff exceeded its 600-second deadline.\n' >&2
    exit 1
  fi
  sleep 1
done
set +e
stopped_packaged_pid="$packaged_pid"
wait "$packaged_pid"
packaged_exit=$?
set -e
if kill -0 "$stopped_packaged_pid" >/dev/null 2>&1; then
  printf 'Packaged PID remained alive after the CUA handoff ended.\n' >&2
  exit 1
fi
packaged_pid=""
printf '%s\n' "$packaged_exit" > "$packaged_root/packaged-exit-status.txt"
[[ "$(shasum -a 256 "$app_executable" | awk '{print $1}')" == "$app_sha256" ]] || exit 1
codesign --verify --deep --strict --verbose=2 "$app_root" >/dev/null 2>&1 || exit 1

kill -0 "$outer_app_pid" >/dev/null 2>&1 || {
  printf 'Outer fixture app did not survive the packaged acceptance.\n' >&2
  exit 1
}
kill -0 "$outer_harness_pid" >/dev/null 2>&1 || {
  printf 'Outer fixture harness did not survive the packaged acceptance.\n' >&2
  exit 1
}
kill -0 "$proxy_pid" >/dev/null 2>&1 || {
  printf 'Outer fixture proxy did not survive the packaged acceptance.\n' >&2
  exit 1
}
[[ "$(ps -p "$outer_app_pid" -o command= | sed 's/^ *//')" == "$outer_app_executable" ]] || exit 1
[[ "$(shasum -a 256 "$outer_app_executable" | awk '{print $1}')" == "$outer_app_sha256" ]] || exit 1
[[ "$(ps -p "$outer_harness_pid" -o command= | sed 's/^ *//')" == "$outer_harness_command" ]] || exit 1
[[ "$(ps -p "$proxy_pid" -o command= | sed 's/^ *//')" == "$proxy_command" ]] || exit 1
lsof -nP -a -p "$proxy_pid" -iTCP:"$proxy_port" -sTCP:LISTEN | rg -q "127\\.0\\.0\\.1:$proxy_port .*LISTEN" || exit 1
[[ "$(ps -p "$fixture_pid" -o command= | sed 's/^ *//')" == "$fixture_command" ]] || exit 1
for fixture_port in 48731 48732; do
  lsof -nP -a -p "$fixture_pid" -iTCP:"$fixture_port" -sTCP:LISTEN | rg -q "127\\.0\\.0\\.1:$fixture_port .*LISTEN" || exit 1
done
node --input-type=module - "$backend_port" "$proxy_port" "$outer_root/tls/ca.crt" "$packaged_root/provider-ownership.json" "$packaged_root/provider-after-exit.json" <<'NODE'
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import tls from 'node:tls';
const [backendPortRaw, proxyPortRaw, caPath, beforePath, output] = process.argv.slice(2);
const backendPort = Number(backendPortRaw);
const proxyPort = Number(proxyPortRaw);
const lines = execFileSync('docker', ['ps', '--filter', 'ancestor=greenmail/standalone:2.1.14', '--format', '{{json .}}'], { encoding: 'utf8' }).trim().split('\n').filter(Boolean).map(JSON.parse);
const token = `127.0.0.1:${backendPort}->3993/tcp`;
const matches = lines.filter((entry) => String(entry.Ports).split(', ').includes(token));
if (matches.length !== 1) throw new Error('exact owned GreenMail backend did not survive');
const before = JSON.parse(readFileSync(beforePath, 'utf8'));
if (matches[0].ID !== before.container?.ID || matches[0].Names !== before.container?.Names) throw new Error('GreenMail container identity changed');
await new Promise((resolve, reject) => {
  const socket = tls.connect({ host: '127.0.0.1', port: proxyPort, servername: 'localhost', ca: readFileSync(caPath), rejectUnauthorized: true });
  let text = '';
  const timer = setTimeout(() => socket.destroy(new Error('owned IMAP greeting timeout after packaged exit')), 5000);
  socket.on('data', (chunk) => {
    text += chunk;
    if (text.includes('\n')) {
      clearTimeout(timer);
      socket.end();
      if (/^\* OK/i.test(text)) resolve(); else reject(new Error('owned IMAP greeting is invalid after packaged exit'));
    }
  });
  socket.on('error', (error) => { clearTimeout(timer); reject(error); });
});
writeFileSync(output, `${JSON.stringify({ schemaVersion: 1, status: 'passed', container: matches[0], backendPort, proxyPort, tlsAuthenticated: true }, null, 2)}\n`, { mode: 0o600 });
NODE
node --input-type=module - "http://127.0.0.1:48731/requests" "$packaged_root/counter-after-exit.json" <<'NODE'
import { writeFileSync } from 'node:fs';
const response = await fetch(process.argv[2]);
const value = await response.json();
if (!response.ok || value.totalContacts !== 0) throw new Error('privacy counter changed before packaged shutdown completed');
writeFileSync(process.argv[3], `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
NODE
verify_source_snapshot "$packaged_root/build-source-before.json" "$packaged_root/run-source-after.json"
copy_evidence
for required_evidence in \
  packaged-exit-status.txt provider-after-exit.json \
  counter-after-exit.json run-source-after.json; do
  [[ -s "$evidence_root/$required_evidence" ]] || {
    printf 'Required final evidence is missing: %s\n' "$required_evidence" >&2
    exit 1
  }
  [[ "$(shasum -a 256 "$packaged_root/$required_evidence" | awk '{print $1}')" == "$(shasum -a 256 "$evidence_root/$required_evidence" | awk '{print $1}')" ]] || exit 1
done
printf 'Packaged fixture stopped; outer provider fixture remains alive.\n'
