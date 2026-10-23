#!/bin/bash
set -euo pipefail

umask 077

readonly ROOT_MARKER_NAME='.dakia-native-mail-safety-root'
readonly ROOT_MARKER_VALUE='dakia-native-mail-safety-fixture-v1'
readonly PLAN_NAME='native-acceptance-plan.json'
readonly EVIDENCE_DIR_NAME='native-attachment-fault-evidence'
readonly EVIDENCE_MARKER_NAME='.dakia-native-attachment-fault-evidence'
readonly EVIDENCE_MARKER_VALUE='dakia-native-attachment-fault-evidence-v1'
readonly UNSAFE_NAME='quarterly-report.pdf.command'
readonly UNSAFE_SIZE='28'
readonly UNSAFE_SHA='1679a56fbd964b0463ec644f56fa9a3189d1402343de932a66b210586d00c02f'
readonly SAFE_NAME='meeting-notes.txt'
readonly SAFE_SIZE='32'
readonly SAFE_SHA='666dec5256feac267cd6c46b97107397c0a87fcde80a932321819f5a1cf2955e'
readonly PLAN_REQUIRED_TEXT='Keep the unsafe warning visible before and after individual save and Save all; verify exact bytes, mode 0600, no execute bits, quarantine, and path containment.'
readonly CANARY_NAME='attachment-leaf-symlink-canary.txt'
readonly CANARY_SIZE='35'
readonly CANARY_SHA='af0fb29d777c7bfea075bfff6615c174e9c4236bd770ccc05d1f7943fdfe6faf'
readonly SAVED_DOWNLOADS_NAME='Downloads.saved-before-enospc'
readonly IMAGE_NAME='attachment-enospc.dmg'
readonly STATE_NAME='enospc-state.json'
readonly MAX_IMAGE_BYTES='100663296'
readonly MAX_FS_KIB='98304'

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat >&2 <<'USAGE'
Usage:
  dakia-native-attachment-fault-helper.sh leaf-prepare --root ABSOLUTE_ROOT
  dakia-native-attachment-fault-helper.sh leaf-check --root ABSOLUTE_ROOT
  dakia-native-attachment-fault-helper.sh enospc-prepare --root ABSOLUTE_ROOT --ui-idle
  dakia-native-attachment-fault-helper.sh enospc-fill --root ABSOLUTE_ROOT
  dakia-native-attachment-fault-helper.sh enospc-fill-resume --root ABSOLUTE_ROOT
  dakia-native-attachment-fault-helper.sh enospc-check --root ABSOLUTE_ROOT
  dakia-native-attachment-fault-helper.sh enospc-restore --root ABSOLUTE_ROOT --ui-idle
  dakia-native-attachment-fault-helper.sh enospc-retry-check --root ABSOLUTE_ROOT

The helper is restricted to the owned fictional native mail-safety harness root.
It never launches Dakia and never overrides HOME.
USAGE
  exit 2
}

[[ $# -ge 3 ]] || usage
operation=$1
shift
root=''
ui_idle=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      [[ $# -ge 2 ]] || usage
      root=$2
      shift 2
      ;;
    --ui-idle)
      ui_idle=1
      shift
      ;;
    *) usage ;;
  esac
done

case "$operation" in
  leaf-prepare|leaf-check|enospc-fill|enospc-fill-resume|enospc-check|enospc-retry-check)
    [[ $ui_idle -eq 0 ]] || die '--ui-idle is accepted only for enospc-prepare and enospc-restore'
    ;;
  enospc-prepare|enospc-restore)
    [[ $ui_idle -eq 1 ]] || die "$operation requires --ui-idle after the parent has confirmed no save is active"
    ;;
  *) usage ;;
esac

[[ -n "$root" && "$root" == /* ]] || die '--root must be an absolute path'
[[ -d "$root" && ! -L "$root" ]] || die 'root must be an existing non-symlink directory'
root_canonical=$(cd -P "$root" && pwd)
[[ "$root" == "$root_canonical" ]] || die 'root must already be canonical'
temp_root=$(cd -P "${TMPDIR:-/tmp}" && pwd)
[[ $(/usr/bin/dirname "$root") == "$temp_root" ]] || die 'root must be directly inside the canonical private temporary directory'
[[ $(/usr/bin/basename "$root") =~ ^dakia-native-provider-reader\.[A-Za-z0-9]{6}$ ]] || die 'root is outside the exact fictional native harness namespace'
[[ $(/usr/bin/stat -f '%u' "$root") == "$(id -u)" ]] || die 'root is not owned by the current user'
root_mode=$(/usr/bin/stat -f '%Lp' "$root")
(( (8#$root_mode & 0022) == 0 )) || die 'root must not be writable by group or other users'

root_marker="$root/$ROOT_MARKER_NAME"
[[ -f "$root_marker" && ! -L "$root_marker" ]] || die 'owned fixture root marker is missing or unsafe'
[[ $(/usr/bin/stat -f '%u' "$root_marker") == "$(id -u)" ]] || die 'root marker is not owned by the current user'
/usr/bin/cmp -s "$root_marker" <(/usr/bin/printf '%s\n' "$ROOT_MARKER_VALUE") || die 'owned fixture root marker content is invalid'

plan="$root/$PLAN_NAME"
[[ -f "$plan" && ! -L "$plan" ]] || die 'native acceptance plan is missing or unsafe'
[[ $(/usr/bin/stat -f '%u' "$plan") == "$(id -u)" ]] || die 'native acceptance plan is not owned by the current user'
node - "$plan" "$UNSAFE_NAME" "$SAFE_NAME" "$PLAN_REQUIRED_TEXT" <<'NODE' || die 'native acceptance plan does not authorize the exact attachment fixtures'
const fs = require('fs');
const [planPath, unsafeName, safeName, required] = process.argv.slice(2);
const plan = JSON.parse(fs.readFileSync(planPath, 'utf8'));
const attachmentChecks = plan?.attachmentChecks;
if (plan?.schemaVersion !== 3 ||
    plan?.messageSubject !== 'Remote content safety check' ||
    !attachmentChecks ||
    JSON.stringify(Object.keys(attachmentChecks).sort()) !== JSON.stringify(['required', 'safeFilename', 'unsafeFilename']) ||
    attachmentChecks.unsafeFilename !== unsafeName ||
    attachmentChecks.safeFilename !== safeName ||
    attachmentChecks.required !== required) process.exit(1);
NODE

downloads="$root/Downloads"
[[ -d "$downloads" && ! -L "$downloads" ]] || die 'Downloads must be an existing non-symlink directory'
downloads_canonical=$(cd -P "$downloads" && pwd)
[[ "$downloads_canonical" == "$downloads" ]] || die 'Downloads must resolve to the exact owned path'
[[ $(/usr/bin/stat -f '%u' "$downloads") == "$(id -u)" ]] || die 'Downloads is not owned by the current user'
downloads_mode=$(/usr/bin/stat -f '%Lp' "$downloads")
(( (8#$downloads_mode & 0022) == 0 )) || die 'Downloads must not be writable by group or other users'

evidence="$root/$EVIDENCE_DIR_NAME"
if [[ -e "$evidence" ]]; then
  [[ -d "$evidence" && ! -L "$evidence" ]] || die 'evidence path exists but is not a safe directory'
  [[ $(/usr/bin/stat -f '%u' "$evidence") == "$(id -u)" ]] || die 'evidence directory is not owned by the current user'
  evidence_mode=$(/usr/bin/stat -f '%Lp' "$evidence")
  (( (8#$evidence_mode & 0022) == 0 )) || die 'evidence directory must not be writable by group or other users'
  [[ -f "$evidence/$EVIDENCE_MARKER_NAME" && ! -L "$evidence/$EVIDENCE_MARKER_NAME" ]] || die 'evidence marker is missing or unsafe'
  /usr/bin/cmp -s "$evidence/$EVIDENCE_MARKER_NAME" <(/usr/bin/printf '%s\n' "$EVIDENCE_MARKER_VALUE") || die 'evidence marker content is invalid'
else
  /bin/mkdir -m 700 "$evidence"
  /usr/bin/printf '%s\n' "$EVIDENCE_MARKER_VALUE" > "$evidence/$EVIDENCE_MARKER_NAME"
  /bin/chmod 600 "$evidence/$EVIDENCE_MARKER_NAME"
fi

sha256() {
  /usr/bin/shasum -a 256 "$1" | /usr/bin/awk '{print $1}'
}

file_mode() {
  /usr/bin/stat -f '%Lp' "$1"
}

write_json_exclusive() {
  local output=$1
  local payload=$2
  [[ ! -e "$output" && ! -e "$output.tmp" ]] || die "refusing to overwrite evidence: $output"
  PAYLOAD="$payload" node - "$output" <<'NODE'
const fs = require('fs');
const path = process.argv[2];
const payload = JSON.parse(process.env.PAYLOAD);
const data = `${JSON.stringify(payload, null, 2)}\n`;
const fd = fs.openSync(`${path}.tmp`, 'wx', 0o600);
try {
  fs.writeFileSync(fd, data, 'utf8');
  fs.fsyncSync(fd);
} finally {
  fs.closeSync(fd);
}
fs.renameSync(`${path}.tmp`, path);
NODE
}

json_payload() {
  node - "$@" <<'NODE'
const [operation, root, ...pairs] = process.argv.slice(2);
const out = { schemaVersion: 1, operation, root, recordedAt: new Date().toISOString() };
for (let i = 0; i < pairs.length; i += 2) out[pairs[i]] = pairs[i + 1];
process.stdout.write(JSON.stringify(out));
NODE
}

require_exact_file() {
  local path=$1 expected_size=$2 expected_sha=$3
  [[ -f "$path" && ! -L "$path" ]] || die "expected regular non-symlink file is missing: $path"
  [[ $(/usr/bin/stat -f '%z' "$path") == "$expected_size" ]] || die "unexpected file size: $path"
  [[ $(sha256 "$path") == "$expected_sha" ]] || die "unexpected file digest: $path"
}

require_safe_saved_file() {
  local path=$1 expected_size=$2 expected_sha=$3
  require_exact_file "$path" "$expected_size" "$expected_sha"
  [[ $(file_mode "$path") == '600' ]] || die "saved file mode is not 0600: $path"
  [[ ! -x "$path" ]] || die "saved file is executable: $path"
  local quarantine
  quarantine=$(/usr/bin/xattr -p com.apple.quarantine "$path" 2>/dev/null) || die "saved file lacks quarantine metadata: $path"
  [[ "$quarantine" =~ ^0081\;[0-9a-f]+\;Dakia\;$ ]] || die "saved file quarantine metadata does not match Dakia's exact production format: $path"
  /usr/bin/printf '%s' "$quarantine"
}

state="$evidence/$STATE_NAME"
image="$root/$IMAGE_NAME"
saved_downloads="$root/$SAVED_DOWNLOADS_NAME"

read_state_field() {
  node - "$state" "$1" <<'NODE'
const fs = require('fs');
const [path, key] = process.argv.slice(2);
const value = JSON.parse(fs.readFileSync(path, 'utf8'))[key];
if (typeof value !== 'string' && typeof value !== 'number') process.exit(1);
process.stdout.write(String(value));
NODE
}

require_state() {
  [[ -f "$state" && ! -L "$state" ]] || die 'ENOSPC state is missing or unsafe'
  [[ $(read_state_field root) == "$root" ]] || die 'ENOSPC state root mismatch'
  [[ $(read_state_field image) == "$image" ]] || die 'ENOSPC state image mismatch'
  [[ $(read_state_field mountPoint) == "$downloads" ]] || die 'ENOSPC state mount mismatch'
  [[ $(read_state_field savedDownloads) == "$saved_downloads" ]] || die 'ENOSPC state saved Downloads mismatch'
}

require_report() {
  local name=$1 expected_operation=$2 report="$evidence/$1"
  [[ -f "$report" && ! -L "$report" ]] || die "required operation evidence is missing or unsafe: $name"
  node - "$report" "$expected_operation" "$root" <<'NODE' || die "operation evidence is invalid: $name"
const fs = require('fs');
const [path, operation, root] = process.argv.slice(2);
const report = JSON.parse(fs.readFileSync(path, 'utf8'));
if (report?.schemaVersion !== 1 || report?.operation !== operation || report?.root !== root) process.exit(1);
NODE
}

verify_original_downloads_identity() {
  local path=$1
  [[ -d "$path" && ! -L "$path" ]] || die 'original Downloads directory is missing or unsafe'
  [[ $(/usr/bin/stat -f '%u' "$path") == "$(id -u)" ]] || die 'original Downloads owner changed'
  [[ $(/usr/bin/stat -f '%d' "$path") == "$(read_state_field originalDownloadsDevice)" ]] || die 'original Downloads device changed'
  [[ $(/usr/bin/stat -f '%i' "$path") == "$(read_state_field originalDownloadsInode)" ]] || die 'original Downloads inode changed'
  [[ $(/usr/bin/stat -f '%Lp' "$path") == "$(read_state_field originalDownloadsMode)" ]] || die 'original Downloads mode changed'
}

verify_image_detached() {
  local info_plist info_json expected_device
  expected_device=$(read_state_field device)
  info_plist=$(capture_hdiutil_info "detached-$(date +%s)-$$")
  info_json="$info_plist.json"
  /usr/bin/plutil -convert json -o "$info_json.tmp" "$info_plist"
  /bin/chmod 600 "$info_json.tmp"
  /bin/mv "$info_json.tmp" "$info_json"
  node - "$info_json" "$image" "$downloads" "$expected_device" <<'NODE' || die 'owned attachment image or device remains attached'
const fs = require('fs');
const [path, image, mountPoint, device] = process.argv.slice(2);
const images = JSON.parse(fs.readFileSync(path, 'utf8')).images || [];
if (images.some(item => item['image-path'] === image ||
  (item['system-entities'] || []).some(e => e['mount-point'] === mountPoint || e['dev-entry'] === device))) process.exit(1);
NODE
}

capture_hdiutil_info() {
  local plist="$evidence/hdiutil-info-$1.plist"
  [[ ! -e "$plist" ]] || die "refusing to overwrite hdiutil evidence: $plist"
  /usr/bin/hdiutil info -plist > "$plist.tmp"
  /bin/chmod 600 "$plist.tmp"
  /bin/mv "$plist.tmp" "$plist"
  /usr/bin/printf '%s' "$plist"
}

verify_owned_mount() {
  require_state
  verify_backing_image_identity
  local expected_device info_plist info_json fs_kib df_device
  expected_device=$(read_state_field device)
  info_plist=$(capture_hdiutil_info "$(date +%s)-$$")
  info_json="$info_plist.json"
  /usr/bin/plutil -convert json -o "$info_json.tmp" "$info_plist"
  /bin/chmod 600 "$info_json.tmp"
  /bin/mv "$info_json.tmp" "$info_json"
  node - "$info_json" "$image" "$downloads" "$expected_device" <<'NODE' || die 'hdiutil does not map the exact owned image, mount point, and device'
const fs = require('fs');
const [path, image, mountPoint, device] = process.argv.slice(2);
const info = JSON.parse(fs.readFileSync(path, 'utf8'));
const images = info.images || [];
const matches = images.filter(item => item['image-path'] === image &&
  (item['system-entities'] || []).some(e => e['mount-point'] === mountPoint && e['dev-entry'] === device));
if (matches.length !== 1) process.exit(1);
NODE
  df_device=$(/bin/df -kP "$downloads" | /usr/bin/awk 'NR==2 {print $1}')
  [[ "$df_device" == "$expected_device" ]] || die 'mounted filesystem device differs from recorded owned device'
  fs_kib=$(/bin/df -kP "$downloads" | /usr/bin/awk 'NR==2 {print $2}')
  [[ "$fs_kib" =~ ^[0-9]+$ ]] || die 'could not read mounted filesystem capacity'
  (( fs_kib > 0 && fs_kib <= MAX_FS_KIB )) || die 'mounted filesystem exceeds the 96 MiB safety bound'
}

verify_backing_image_identity() {
  [[ -f "$image" && ! -L "$image" ]] || die 'owned disk image is missing or unsafe'
  [[ $(/usr/bin/stat -f '%u' "$image") == "$(id -u)" ]] || die 'disk image is not owned by current user'
  local image_size expected_image_file_device expected_image_inode
  image_size=$(/usr/bin/stat -f '%z' "$image")
  (( image_size > 0 && image_size <= MAX_IMAGE_BYTES )) || die 'disk image size is outside the bounded limit'
  expected_image_file_device=$(read_state_field imageFileDevice)
  expected_image_inode=$(read_state_field imageInode)
  [[ $(/usr/bin/stat -f '%d' "$image") == "$expected_image_file_device" ]] || die 'disk image backing-file device changed since attach'
  [[ $(/usr/bin/stat -f '%i' "$image") == "$expected_image_inode" ]] || die 'disk image backing-file inode changed since attach'
}

validate_existing_fill_artifacts() {
  filler="$downloads/.dakia-enospc-fill"
  log="$evidence/enospc-dd.log"
  [[ -f "$filler" && ! -L "$filler" ]] || die 'bounded ENOSPC filler is missing or unsafe'
  [[ $(/usr/bin/stat -f '%u' "$filler") == "$(id -u)" ]] || die 'bounded ENOSPC filler owner changed'
  [[ $(/usr/bin/stat -f '%Lp' "$filler") == '600' ]] || die 'bounded ENOSPC filler mode changed from 0600'
  filler_size=$(/usr/bin/stat -f '%z' "$filler")
  filler_device=$(/usr/bin/stat -f '%d' "$filler")
  filler_inode=$(/usr/bin/stat -f '%i' "$filler")
  filler_owner=$(/usr/bin/stat -f '%u' "$filler")
  filler_mode=$(/usr/bin/stat -f '%Lp' "$filler")
  [[ "$filler_size" =~ ^[0-9]+$ ]] || die 'bounded ENOSPC filler size is invalid'
  (( filler_size > 0 && filler_size <= 67108864 )) || die 'bounded ENOSPC filler size is outside the owned 64 MiB volume'
  [[ "$filler_device" == "$(/usr/bin/stat -f '%d' "$downloads")" ]] || die 'bounded ENOSPC filler is on the wrong filesystem'
  [[ -f "$log" && ! -L "$log" ]] || die 'original ENOSPC log is missing or unsafe'
  [[ $(/usr/bin/stat -f '%u' "$log") == "$(id -u)" ]] || die 'original ENOSPC log owner changed'
  [[ $(/usr/bin/stat -f '%Lp' "$log") == '600' ]] || die 'original ENOSPC log mode changed from 0600'
  log_size=$(/usr/bin/stat -f '%z' "$log")
  (( log_size > 0 && log_size <= 65536 )) || die 'original ENOSPC log size is outside its 64 KiB bound'
  /usr/bin/grep -q 'No space left on device' "$log" || die 'original fill log does not prove ENOSPC'
}

tail_fill_to_enospc() {
  validate_existing_fill_artifacts
  local tail_report="$evidence/enospc-tail-append.json"
  [[ ! -e "$tail_report" && ! -L "$tail_report" ]] || die 'refusing to overwrite bounded tail-fill evidence'
  local downloads_device downloads_inode current_uid tail_work tail_error tail_exit failure_stamp
  downloads_device=$(/usr/bin/stat -f '%d' "$downloads")
  downloads_inode=$(/usr/bin/stat -f '%i' "$downloads")
  current_uid=$(id -u)
  tail_work="$evidence/.enospc-tail-append.work.$$"
  tail_error="$evidence/.enospc-tail-append.stderr.$$"
  [[ ! -e "$tail_work" && ! -e "$tail_error" ]] || die 'bounded tail-fill work path already exists'
  set +e
  /usr/bin/python3 - "$downloads" "$downloads_device" "$downloads_inode" "$filler_device" "$filler_inode" "$current_uid" > "$tail_work" 2> "$tail_error" <<'PY'
import errno
import json
import os
import stat
import sys

downloads, expected_dir_dev, expected_dir_ino, expected_file_dev, expected_file_ino, expected_uid = sys.argv[1:]
expected_dir_dev = int(expected_dir_dev)
expected_dir_ino = int(expected_dir_ino)
expected_file_dev = int(expected_file_dev)
expected_file_ino = int(expected_file_ino)
expected_uid = int(expected_uid)
chunk_bytes = 4096
maximum_append_bytes = 2 * 1024 * 1024
directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
file_flags = os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW
directory_fd = os.open(downloads, directory_flags)
file_fd = None
appended_bytes = 0
enospc_stage = None
try:
    directory_stat = os.fstat(directory_fd)
    if directory_stat.st_dev != expected_dir_dev or directory_stat.st_ino != expected_dir_ino:
        raise RuntimeError('Downloads directory identity changed before tail fill')
    file_fd = os.open('.dakia-enospc-fill', file_flags, dir_fd=directory_fd)
    file_stat = os.fstat(file_fd)
    if not stat.S_ISREG(file_stat.st_mode):
        raise RuntimeError('tail-fill target is not a regular file')
    if file_stat.st_uid != expected_uid or stat.S_IMODE(file_stat.st_mode) != 0o600:
        raise RuntimeError('tail-fill target owner or mode changed')
    if file_stat.st_dev != expected_file_dev or file_stat.st_ino != expected_file_ino:
        raise RuntimeError('tail-fill target identity changed before open')
    zeroes = bytes(chunk_bytes)
    while appended_bytes < maximum_append_bytes:
        try:
            written = os.write(file_fd, zeroes)
            if written <= 0:
                raise RuntimeError('tail-fill write made no progress')
            appended_bytes += written
            os.fsync(file_fd)
        except OSError as error:
            if error.errno != errno.ENOSPC:
                raise
            enospc_stage = 'write-or-fsync'
            break
    if enospc_stage is None:
        raise RuntimeError('tail fill reached the 2 MiB bound without ENOSPC')
    final_stat = os.fstat(file_fd)
    if final_stat.st_dev != expected_file_dev or final_stat.st_ino != expected_file_ino:
        raise RuntimeError('tail-fill target identity changed after ENOSPC')
    current_entry = os.stat('.dakia-enospc-fill', dir_fd=directory_fd, follow_symlinks=False)
    if not stat.S_ISREG(current_entry.st_mode) or current_entry.st_dev != final_stat.st_dev or current_entry.st_ino != final_stat.st_ino:
        raise RuntimeError('tail-fill directory entry no longer names the pinned filler')
    current_directory = os.stat(downloads, follow_symlinks=False)
    if not stat.S_ISDIR(current_directory.st_mode) or current_directory.st_dev != directory_stat.st_dev or current_directory.st_ino != directory_stat.st_ino:
        raise RuntimeError('Downloads path no longer names the pinned directory')
    filesystem = os.fstatvfs(directory_fd)
    available_bytes = filesystem.f_bavail * filesystem.f_frsize
    if available_bytes > 4096:
        raise RuntimeError(f'pinned filesystem retains {available_bytes} available bytes after ENOSPC')
    evidence = {
        'schemaVersion': 1,
        'operation': 'bounded-tail-fill',
        'chunkBytes': chunk_bytes,
        'maximumAppendBytes': maximum_append_bytes,
        'appendedBytes': appended_bytes,
        'enospcObserved': True,
        'enospcStage': enospc_stage,
        'directoryDevice': str(directory_stat.st_dev),
        'directoryInode': str(directory_stat.st_ino),
        'fillerDevice': str(final_stat.st_dev),
        'fillerInode': str(final_stat.st_ino),
        'fillerBytesAfter': final_stat.st_size,
        'fillerOwner': str(final_stat.st_uid),
        'fillerMode': format(stat.S_IMODE(final_stat.st_mode), 'o'),
        'availableBytes': available_bytes,
    }
    print(json.dumps(evidence, indent=2))
finally:
    if file_fd is not None:
        os.close(file_fd)
    os.close(directory_fd)
PY
  tail_exit=$?
  set -e
  /bin/chmod 600 "$tail_work" "$tail_error"
  if [[ $tail_exit -ne 0 ]]; then
    failure_stamp="$(date +%s)-$$"
    /bin/mv "$tail_work" "$evidence/enospc-tail-append-failure-$failure_stamp.stdout"
    /bin/mv "$tail_error" "$evidence/enospc-tail-append-failure-$failure_stamp.stderr"
    die "bounded tail fill failed; private failure evidence retained with stamp $failure_stamp"
  fi
  [[ ! -s "$tail_error" ]] || {
    failure_stamp="$(date +%s)-$$"
    /bin/mv "$tail_work" "$evidence/enospc-tail-append-failure-$failure_stamp.stdout"
    /bin/mv "$tail_error" "$evidence/enospc-tail-append-failure-$failure_stamp.stderr"
    die "bounded tail fill emitted unexpected stderr; evidence retained with stamp $failure_stamp"
  }
  /bin/unlink "$tail_error"
  node - "$tail_work" "$downloads_device" "$downloads_inode" "$filler_device" "$filler_inode" "$current_uid" <<'NODE' || die 'bounded tail-fill evidence is invalid'
const fs = require('fs');
const report = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const [directoryDevice, directoryInode, fillerDevice, fillerInode, fillerOwner] = process.argv.slice(3);
if (report?.schemaVersion !== 1 ||
    report?.operation !== 'bounded-tail-fill' ||
    report?.chunkBytes !== 4096 ||
    report?.maximumAppendBytes !== 2097152 ||
    report?.enospcObserved !== true ||
    !Number.isInteger(report?.appendedBytes) ||
    report.appendedBytes < 0 ||
    report.appendedBytes > 2097152 ||
    report?.availableBytes > 4096 ||
    report?.directoryDevice !== directoryDevice ||
    report?.directoryInode !== directoryInode ||
    report?.fillerDevice !== fillerDevice ||
    report?.fillerInode !== fillerInode ||
    report?.fillerOwner !== fillerOwner ||
    report?.fillerMode !== '600') process.exit(1);
NODE
  verify_owned_mount
  validate_existing_fill_artifacts
  node - "$tail_work" "$downloads_device" "$downloads_inode" "$filler_device" "$filler_inode" "$filler_size" <<'NODE' || die 'current filler path does not match pinned tail-fill evidence'
const fs = require('fs');
const report = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const [directoryDevice, directoryInode, fillerDevice, fillerInode, fillerBytesAfter] = process.argv.slice(3);
if (report.directoryDevice !== directoryDevice ||
    report.directoryInode !== directoryInode ||
    report.fillerDevice !== fillerDevice ||
    report.fillerInode !== fillerInode ||
    String(report.fillerBytesAfter) !== fillerBytesAfter) process.exit(1);
NODE
  available_kib=$(/bin/df -kP "$downloads" | /usr/bin/awk 'NR==2 {print $4}')
  [[ "$available_kib" =~ ^[0-9]+$ ]] || die 'could not read free space after bounded tail fill'
  (( available_kib <= 4 )) || die 'bounded tail fill observed ENOSPC but more than 4 KiB remains available'
  /bin/chmod 600 "$tail_work"
  /bin/mv "$tail_work" "$tail_report"
}

case "$operation" in
  leaf-prepare)
    canary="$root/$CANARY_NAME"
    link="$downloads/$UNSAFE_NAME"
    result="$downloads/quarterly-report.pdf (1).command"
    [[ ! -e "$canary" && ! -L "$canary" ]] || die 'canary path already exists'
    [[ ! -e "$link" && ! -L "$link" ]] || die 'unsafe collision path already exists'
    [[ ! -e "$result" && ! -L "$result" ]] || die 'expected collision result path already exists'
    /usr/bin/printf '%s\n' 'dakia-owned-leaf-symlink-canary-v1' > "$canary"
    /bin/chmod 600 "$canary"
    require_exact_file "$canary" "$CANARY_SIZE" "$CANARY_SHA"
    /bin/ln -s "$canary" "$link"
    [[ -L "$link" && $(/usr/bin/readlink "$link") == "$canary" ]] || die 'failed to create the exact leaf symlink collision'
    payload=$(json_payload "$operation" "$root" canary "$canary" canarySha256 "$CANARY_SHA" canaryMode '600' link "$link" linkTarget "$canary" expectedResult "$result")
    write_json_exclusive "$evidence/leaf-prepare.json" "$payload"
    printf 'Leaf collision prepared. Use the visible UI to save %s once.\n' "$UNSAFE_NAME"
    ;;

  leaf-check)
    canary="$root/$CANARY_NAME"
    link="$downloads/$UNSAFE_NAME"
    result="$downloads/quarterly-report.pdf (1).command"
    require_exact_file "$canary" "$CANARY_SIZE" "$CANARY_SHA"
    [[ $(file_mode "$canary") == '600' ]] || die 'canary mode changed from 0600'
    [[ -L "$link" && $(/usr/bin/readlink "$link") == "$canary" ]] || die 'leaf symlink was replaced or retargeted'
    quarantine=$(require_safe_saved_file "$result" "$UNSAFE_SIZE" "$UNSAFE_SHA")
    payload=$(json_payload "$operation" "$root" canary "$canary" canarySha256 "$(sha256 "$canary")" canaryMode "$(file_mode "$canary")" link "$link" linkTarget "$(/usr/bin/readlink "$link")" savedFile "$result" savedSha256 "$(sha256 "$result")" savedSize "$(/usr/bin/stat -f '%z' "$result")" savedMode "$(file_mode "$result")" quarantine "$quarantine")
    write_json_exclusive "$evidence/leaf-check.json" "$payload"
    printf 'Leaf collision passed: canary and symlink unchanged; collision copy bytes, mode, and quarantine are exact.\n'
    ;;

  enospc-prepare)
    [[ $(uname -s) == 'Darwin' ]] || die 'ENOSPC image operations are macOS-only'
    [[ ! -e "$saved_downloads" && ! -L "$saved_downloads" ]] || die 'saved Downloads path already exists'
    [[ ! -e "$image" && ! -L "$image" ]] || die 'disk image path already exists'
    [[ ! -e "$state" && ! -L "$state" ]] || die 'ENOSPC state already exists'
    root_dev=$(/usr/bin/stat -f '%d' "$root")
    downloads_dev=$(/usr/bin/stat -f '%d' "$downloads")
    [[ "$root_dev" == "$downloads_dev" ]] || die 'Downloads is already a distinct mount; refusing to replace it'
    original_downloads_device=$downloads_dev
    original_downloads_inode=$(/usr/bin/stat -f '%i' "$downloads")
    original_downloads_mode=$(/usr/bin/stat -f '%Lp' "$downloads")
    node - "$downloads" <<'NODE' || die 'safe attachment filename or collision already exists before ENOSPC preparation'
const fs = require('fs');
const names = fs.readdirSync(process.argv[2]);
if (names.some(n => /^meeting-notes(?: \([0-9]+\))?\.txt$/.test(n))) process.exit(1);
NODE
    /usr/bin/hdiutil create -size 64m -fs 'Journaled HFS+' -volname DakiaAttachmentENOSPC -nospotlight "$image"
    [[ -f "$image" && ! -L "$image" ]] || die 'hdiutil did not create the exact disk image'
    /bin/chmod 600 "$image"
    image_size=$(/usr/bin/stat -f '%z' "$image")
    [[ "$image_size" == '67108864' ]] || die 'created disk image is not exactly 64 MiB'
    imageinfo_plist="$evidence/hdiutil-imageinfo.plist"
    imageinfo_json="$evidence/hdiutil-imageinfo.json"
    [[ ! -e "$imageinfo_plist" && ! -e "$imageinfo_json" ]] || die 'refusing to overwrite disk-image inspection evidence'
    /usr/bin/hdiutil imageinfo -plist "$image" > "$imageinfo_plist.tmp"
    /bin/chmod 600 "$imageinfo_plist.tmp"
    /bin/mv "$imageinfo_plist.tmp" "$imageinfo_plist"
    /usr/bin/plutil -convert json -o "$imageinfo_json.tmp" "$imageinfo_plist"
    /bin/chmod 600 "$imageinfo_json.tmp"
    /bin/mv "$imageinfo_json.tmp" "$imageinfo_json"
    node - "$imageinfo_json" <<'NODE' || die 'created disk image is not an exact 64 MiB UDRW image with one HFS+ partition'
const fs = require('fs');
const info = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const hfsPartitions = info?.partitions?.partitions?.filter(partition =>
  partition['partition-filesystems'] && Object.hasOwn(partition['partition-filesystems'], 'HFS+')) || [];
if (info.Format !== 'UDRW' ||
    info?.['Size Information']?.['Total Bytes'] !== 67108864 ||
    hfsPartitions.length !== 1 ||
    hfsPartitions[0]['partition-hint'] !== 'Apple_HFS') process.exit(1);
NODE
    /bin/mv "$downloads" "$saved_downloads"
    /bin/mkdir -m 700 "$downloads"
    attach_plist="$evidence/hdiutil-attach.plist"
    if ! /usr/bin/hdiutil attach -nobrowse -owners on -mountpoint "$downloads" -plist "$image" > "$attach_plist.tmp"; then
      /bin/rmdir "$downloads" || die 'attach failed and empty mountpoint could not be removed; manual inspection required'
      /bin/mv "$saved_downloads" "$downloads"
      die 'hdiutil attach failed; original Downloads was restored and image retained'
    fi
    /bin/chmod 600 "$attach_plist.tmp"
    /bin/mv "$attach_plist.tmp" "$attach_plist"
    attach_json="$evidence/hdiutil-attach.json"
    /usr/bin/plutil -convert json -o "$attach_json.tmp" "$attach_plist"
    /bin/chmod 600 "$attach_json.tmp"
    /bin/mv "$attach_json.tmp" "$attach_json"
    device=$(node - "$attach_json" "$downloads" <<'NODE'
const fs = require('fs');
const [path, mountPoint] = process.argv.slice(2);
const entities = JSON.parse(fs.readFileSync(path, 'utf8'))['system-entities'] || [];
const matches = entities.filter(e => e['mount-point'] === mountPoint && typeof e['dev-entry'] === 'string');
if (matches.length !== 1) process.exit(1);
process.stdout.write(matches[0]['dev-entry']);
NODE
    ) || die 'could not prove the attached device from hdiutil output; leave paths untouched for inspection'
    image_sha=$(sha256 "$image")
    image_file_device=$(/usr/bin/stat -f '%d' "$image")
    image_inode=$(/usr/bin/stat -f '%i' "$image")
    state_payload=$(node - "$root" "$image" "$downloads" "$saved_downloads" "$device" "$image_sha" "$image_size" "$image_file_device" "$image_inode" "$original_downloads_device" "$original_downloads_inode" "$original_downloads_mode" <<'NODE'
const [root, image, mountPoint, savedDownloads, device, initialImageSha256, imageBytes, imageFileDevice, imageInode, originalDownloadsDevice, originalDownloadsInode, originalDownloadsMode] = process.argv.slice(2);
process.stdout.write(JSON.stringify({schemaVersion: 1, root, image, mountPoint, savedDownloads, device, initialImageSha256, imageBytes: Number(imageBytes), imageFileDevice, imageInode, originalDownloadsDevice, originalDownloadsInode, originalDownloadsMode, createdAt: new Date().toISOString()}));
NODE
    )
    write_json_exclusive "$state" "$state_payload"
    verify_owned_mount
    fs_kib=$(/bin/df -kP "$downloads" | /usr/bin/awk 'NR==2 {print $2}')
    payload=$(json_payload "$operation" "$root" image "$image" initialImageSha256 "$image_sha" imageBytes "$image_size" imageInfoPlist "$imageinfo_plist" imageInfoPlistSha256 "$(sha256 "$imageinfo_plist")" imageFileDevice "$image_file_device" imageInode "$image_inode" mountPoint "$downloads" device "$device" filesystemKiB "$fs_kib" savedDownloads "$saved_downloads" originalDownloadsDevice "$original_downloads_device" originalDownloadsInode "$original_downloads_inode" originalDownloadsMode "$original_downloads_mode")
    write_json_exclusive "$evidence/enospc-prepare.json" "$payload"
    printf 'Owned 64 MiB attachment volume mounted. Run enospc-fill before the visible save attempt.\n'
    ;;

  enospc-fill)
    require_report enospc-prepare.json enospc-prepare
    verify_owned_mount
    filler="$downloads/.dakia-enospc-fill"
    log="$evidence/enospc-dd.log"
    [[ ! -e "$filler" && ! -L "$filler" && ! -e "$log" ]] || die 'ENOSPC filler or log already exists'
    set +e
    /bin/dd if=/dev/zero of="$filler" bs=1048576 count=128 conv=fsync 2> "$log.tmp"
    dd_status=$?
    set -e
    /bin/chmod 600 "$log.tmp"
    /bin/mv "$log.tmp" "$log"
    [[ $dd_status -ne 0 ]] || die 'bounded 128 MiB fill unexpectedly succeeded; filesystem was not proven full'
    /usr/bin/grep -q 'No space left on device' "$log" || die 'fill failed without the required ENOSPC diagnostic'
    [[ -f "$filler" && ! -L "$filler" ]] || die 'bounded filler is missing after ENOSPC'
    available_kib=$(/bin/df -kP "$downloads" | /usr/bin/awk 'NR==2 {print $4}')
    [[ "$available_kib" =~ ^[0-9]+$ ]] || die 'could not read post-fill free space'
    if (( available_kib > 4 )); then
      tail_fill_to_enospc
    fi
    available_kib=$(/bin/df -kP "$downloads" | /usr/bin/awk 'NR==2 {print $4}')
    (( available_kib <= 4 )) || die 'mounted filesystem still has more than 4 KiB available after bounded fill'
    validate_existing_fill_artifacts
    tail_report="$evidence/enospc-tail-append.json"
    tail_report_sha=''
    [[ ! -f "$tail_report" ]] || tail_report_sha=$(sha256 "$tail_report")
    payload=$(json_payload "$operation" "$root" filler "$filler" fillerBytes "$filler_size" fillerDevice "$filler_device" fillerInode "$filler_inode" fillerOwner "$filler_owner" fillerMode "$filler_mode" ddExit "$dd_status" ddLog "$log" tailReport "$tail_report" tailReportSha256 "$tail_report_sha" availableKiB "$available_kib")
    write_json_exclusive "$evidence/enospc-fill.json" "$payload"
    printf 'ENOSPC proven with the bounded primary and tail writes. Use the visible UI to save %s once.\n' "$SAFE_NAME"
    ;;

  enospc-fill-resume)
    require_report enospc-prepare.json enospc-prepare
    verify_owned_mount
    [[ ! -e "$evidence/enospc-fill.json" && ! -L "$evidence/enospc-fill.json" ]] || die 'final ENOSPC fill evidence already exists; resume is not allowed'
    tail_fill_to_enospc
    filler="$downloads/.dakia-enospc-fill"
    log="$evidence/enospc-dd.log"
    tail_report="$evidence/enospc-tail-append.json"
    validate_existing_fill_artifacts
    payload=$(json_payload enospc-fill "$root" resumedAfterPartialBlock 'true' filler "$filler" fillerBytes "$filler_size" fillerDevice "$filler_device" fillerInode "$filler_inode" fillerOwner "$filler_owner" fillerMode "$filler_mode" ddLog "$log" ddLogSha256 "$(sha256 "$log")" tailReport "$tail_report" tailReportSha256 "$(sha256 "$tail_report")" availableKiB "$available_kib")
    write_json_exclusive "$evidence/enospc-fill.json" "$payload"
    printf 'ENOSPC resume passed with at most 2 MiB of 4 KiB appends. Use the visible UI to save %s once.\n' "$SAFE_NAME"
    ;;

  enospc-check)
    require_report enospc-prepare.json enospc-prepare
    require_report enospc-fill.json enospc-fill
    verify_owned_mount
    validate_existing_fill_artifacts
    fill_report="$evidence/enospc-fill.json"
    node - "$fill_report" "$filler" "$filler_size" "$filler_device" "$filler_inode" "$filler_owner" "$filler_mode" <<'NODE' || die 'current ENOSPC filler does not match final fill evidence'
const fs = require('fs');
const report = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const [filler, fillerBytes, fillerDevice, fillerInode, fillerOwner, fillerMode] = process.argv.slice(3);
if (report?.filler !== filler ||
    report?.fillerBytes !== fillerBytes ||
    report?.fillerDevice !== fillerDevice ||
    report?.fillerInode !== fillerInode ||
    report?.fillerOwner !== fillerOwner ||
    report?.fillerMode !== fillerMode) process.exit(1);
NODE
    available_kib=$(/bin/df -kP "$downloads" | /usr/bin/awk 'NR==2 {print $4}')
    [[ "$available_kib" =~ ^[0-9]+$ ]] || die 'could not read free space before ENOSPC result check'
    (( available_kib <= 4 )) || die 'mounted filesystem is no longer full at ENOSPC result check'
    findings=$(node - "$downloads" <<'NODE'
const fs = require('fs');
const names = fs.readdirSync(process.argv[2]);
const unsafe = names.filter(n => /^meeting-notes(?: \([0-9]+\))?\.txt$/.test(n) || /^\.dakia-.*\.partial$/.test(n));
process.stdout.write(JSON.stringify(unsafe));
if (unsafe.length) process.exit(1);
NODE
    ) || die "save failure left a final or partial attachment file: $findings"
    payload=$(json_payload "$operation" "$root" filler "$filler" fillerBytes "$filler_size" fillerDevice "$filler_device" fillerInode "$filler_inode" prohibitedArtifacts "$findings" availableKiB "$available_kib" result 'no final or partial attachment file')
    write_json_exclusive "$evidence/enospc-check.json" "$payload"
    printf 'ENOSPC filesystem check passed: no final or partial attachment file exists. Confirm the visible failure and cleared spinner before restore.\n'
    ;;

  enospc-restore)
    require_report enospc-prepare.json enospc-prepare
    require_report enospc-fill.json enospc-fill
    require_report enospc-check.json enospc-check
    verify_owned_mount
    verify_original_downloads_identity "$saved_downloads"
    device=$(read_state_field device)
    /usr/bin/hdiutil detach "$device"
    root_dev=$(/usr/bin/stat -f '%d' "$root")
    downloads_dev=$(/usr/bin/stat -f '%d' "$downloads")
    [[ "$root_dev" == "$downloads_dev" ]] || die 'detach returned but Downloads remains a distinct filesystem'
    [[ -z "$(/bin/ls -A "$downloads")" ]] || die 'underlying mountpoint is not empty after detach; refusing restore'
    /bin/rmdir "$downloads"
    /bin/mv "$saved_downloads" "$downloads"
    [[ -d "$downloads" && ! -L "$downloads" ]] || die 'original Downloads was not restored safely'
    [[ $(cd -P "$downloads" && pwd) == "$downloads" ]] || die 'restored Downloads is not canonical'
    verify_original_downloads_identity "$downloads"
    verify_image_detached
    payload=$(json_payload "$operation" "$root" detachedDevice "$device" retainedImage "$image" retainedImageSha256 "$(sha256 "$image")" retainedImageBytes "$(/usr/bin/stat -f '%z' "$image")" restoredDownloads "$downloads" restoredDownloadsDevice "$(/usr/bin/stat -f '%d' "$downloads")" restoredDownloadsInode "$(/usr/bin/stat -f '%i' "$downloads")" restoredDownloadsMode "$(file_mode "$downloads")")
    write_json_exclusive "$evidence/enospc-restore.json" "$payload"
    printf 'Original Downloads restored. Use the visible UI to retry saving %s once.\n' "$SAFE_NAME"
    ;;

  enospc-retry-check)
    require_state
    require_report enospc-prepare.json enospc-prepare
    require_report enospc-fill.json enospc-fill
    require_report enospc-check.json enospc-check
    require_report enospc-restore.json enospc-restore
    verify_original_downloads_identity "$downloads"
    verify_backing_image_identity
    verify_image_detached
    result="$downloads/$SAFE_NAME"
    quarantine=$(require_safe_saved_file "$result" "$SAFE_SIZE" "$SAFE_SHA")
    partials=$(node - "$downloads" <<'NODE'
const fs = require('fs');
const names = fs.readdirSync(process.argv[2]).filter(n => /^\.dakia-.*\.partial$/.test(n));
process.stdout.write(JSON.stringify(names));
if (names.length) process.exit(1);
NODE
    ) || die "retry left partial attachment files: $partials"
    payload=$(json_payload "$operation" "$root" savedFile "$result" savedSha256 "$(sha256 "$result")" savedSize "$(/usr/bin/stat -f '%z' "$result")" savedMode "$(file_mode "$result")" quarantine "$quarantine" partialArtifacts "$partials")
    write_json_exclusive "$evidence/enospc-retry-check.json" "$payload"
    printf 'Retry passed: exact safe bytes, mode 0600, no execute bit, quarantine, and no partial files.\n'
    ;;
esac
