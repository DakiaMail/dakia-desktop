#!/bin/bash
set -euo pipefail
umask 077

helper=/private/tmp/dakia-native-attachment-fault-helper.sh
[[ -x "$helper" ]] || { echo 'helper is not executable' >&2; exit 1; }
test_log=$(mktemp "${TMPDIR:-/tmp}/dakia-attachment-helper-test-log.XXXXXX")

make_plan() {
  cat > "$1/native-acceptance-plan.json" <<'JSON'
{
  "schemaVersion": 3,
  "messageSubject": "Remote content safety check",
  "attachmentChecks": {
    "unsafeFilename": "quarterly-report.pdf.command",
    "safeFilename": "meeting-notes.txt",
    "required": "Keep the unsafe warning visible before and after individual save and Save all; verify exact bytes, mode 0600, no execute bits, quarantine, and path containment."
  }
}
JSON
  chmod 600 "$1/native-acceptance-plan.json"
}

make_root() {
  local target
  target=$(mktemp -d "${TMPDIR:-/tmp}/dakia-native-provider-reader.XXXXXX")
  target=$(cd -P "$target" && pwd)
  mkdir -m 700 "$target/Downloads"
  printf '%s\n' 'dakia-native-mail-safety-fixture-v1' > "$target/.dakia-native-mail-safety-root"
  chmod 600 "$target/.dakia-native-mail-safety-root"
  make_plan "$target"
  printf '%s' "$target"
}

expect_reject() {
  local label=$1
  shift
  if "$@" >"$test_log.$label.stdout" 2>"$test_log.$label.stderr"; then
    echo "expected rejection succeeded: $label" >&2
    exit 1
  fi
}

write_unsafe_result() {
  local root=$1 xattr_value=${2:-'0081;123;Dakia;'}
  node - "$root/Downloads/quarterly-report.pdf (1).command" <<'NODE'
const fs = require('fs');
fs.writeFileSync(process.argv[2], Buffer.from('66696374696f6e616c206174746163686d656e7420627974657300ff', 'hex'), {mode: 0o600, flag: 'wx'});
NODE
  if [[ "$xattr_value" != 'NONE' ]]; then
    /usr/bin/xattr -w com.apple.quarantine "$xattr_value" "$root/Downloads/quarterly-report.pdf (1).command"
  fi
}

prepare_leaf_case() {
  local root
  root=$(make_root)
  "$helper" leaf-prepare --root "$root" >/dev/null
  printf '%s' "$root"
}

write_report() {
  local root=$1 filename=$2 operation=$3
  node - "$root/native-attachment-fault-evidence/$filename" "$operation" "$root" <<'NODE'
const fs = require('fs');
const [path, operation, root] = process.argv.slice(2);
fs.writeFileSync(path, `${JSON.stringify({schemaVersion: 1, operation, root, recordedAt: new Date().toISOString()}, null, 2)}\n`, {mode: 0o600, flag: 'wx'});
NODE
}

prepare_synthetic_restore_chain() {
  local root=$1 inode_override=${2:-}
  local evidence="$root/native-attachment-fault-evidence"
  printf '%s\n' 'synthetic detached image for read-only helper validation' > "$root/attachment-enospc.dmg"
  chmod 600 "$root/attachment-enospc.dmg"
  local downloads_device downloads_inode downloads_mode image_device image_inode
  downloads_device=$(stat -f '%d' "$root/Downloads")
  downloads_inode=$(stat -f '%i' "$root/Downloads")
  downloads_mode=$(stat -f '%Lp' "$root/Downloads")
  image_device=$(stat -f '%d' "$root/attachment-enospc.dmg")
  image_inode=$(stat -f '%i' "$root/attachment-enospc.dmg")
  [[ -z "$inode_override" ]] || downloads_inode=$inode_override
  node - "$evidence/enospc-state.json" "$root" "$downloads_device" "$downloads_inode" "$downloads_mode" "$image_device" "$image_inode" <<'NODE'
const fs = require('fs');
const [path, root, originalDownloadsDevice, originalDownloadsInode, originalDownloadsMode, imageFileDevice, imageInode] = process.argv.slice(2);
fs.writeFileSync(path, `${JSON.stringify({
  schemaVersion: 1,
  root,
  image: `${root}/attachment-enospc.dmg`,
  mountPoint: `${root}/Downloads`,
  savedDownloads: `${root}/Downloads.saved-before-enospc`,
  device: '/dev/dakia-synthetic-never-mounted',
  initialImageSha256: 'synthetic',
  imageBytes: 58,
  imageFileDevice,
  imageInode,
  originalDownloadsDevice,
  originalDownloadsInode,
  originalDownloadsMode,
  createdAt: new Date().toISOString()
}, null, 2)}\n`, {mode: 0o600, flag: 'wx'});
NODE
  write_report "$root" enospc-prepare.json enospc-prepare
  write_report "$root" enospc-fill.json enospc-fill
  write_report "$root" enospc-check.json enospc-check
  write_report "$root" enospc-restore.json enospc-restore
  node - "$root/Downloads/meeting-notes.txt" <<'NODE'
const fs = require('fs');
fs.writeFileSync(process.argv[2], Buffer.from('66696374696f6e616c2073616665206174746163686d656e742062797465730a', 'hex'), {mode: 0o600, flag: 'wx'});
NODE
  /usr/bin/xattr -w com.apple.quarantine '0081;456;Dakia;' "$root/Downloads/meeting-notes.txt"
}

valid_root=$(prepare_leaf_case)
write_unsafe_result "$valid_root"
"$helper" leaf-check --root "$valid_root" >/dev/null

for corruption in canary-bytes link-target result-bytes result-mode xattr-missing xattr-malformed xattr-agent; do
  root=$(prepare_leaf_case)
  write_unsafe_result "$root"
  case "$corruption" in
    canary-bytes) printf 'changed\n' > "$root/attachment-leaf-symlink-canary.txt" ;;
    link-target) unlink "$root/Downloads/quarterly-report.pdf.command"; ln -s "$root/native-acceptance-plan.json" "$root/Downloads/quarterly-report.pdf.command" ;;
    result-bytes) printf 'wrong' > "$root/Downloads/quarterly-report.pdf (1).command" ;;
    result-mode) chmod 644 "$root/Downloads/quarterly-report.pdf (1).command" ;;
    xattr-missing) /usr/bin/xattr -d com.apple.quarantine "$root/Downloads/quarterly-report.pdf (1).command" ;;
    xattr-malformed) /usr/bin/xattr -w com.apple.quarantine 'bogus;Dakia;bogus' "$root/Downloads/quarterly-report.pdf (1).command" ;;
    xattr-agent) /usr/bin/xattr -w com.apple.quarantine '0081;123;Other;' "$root/Downloads/quarterly-report.pdf (1).command" ;;
  esac
  expect_reject "leaf-$corruption" "$helper" leaf-check --root "$root"
done

unmarked_root=$(make_root)
unlink "$unmarked_root/.dakia-native-mail-safety-root"
expect_reject unmarked "$helper" leaf-prepare --root "$unmarked_root"

wrong_plan_root=$(make_root)
printf '%s\n' '{"schemaVersion":3,"messageSubject":"Remote content safety check","attachmentChecks":{"unsafeFilename":"wrong","safeFilename":"meeting-notes.txt","required":"wrong"}}' > "$wrong_plan_root/native-acceptance-plan.json"
expect_reject wrong-plan "$helper" leaf-prepare --root "$wrong_plan_root"

extra_plan_root=$(make_root)
node - "$extra_plan_root/native-acceptance-plan.json" <<'NODE'
const fs = require('fs'); const path = process.argv[2]; const plan = JSON.parse(fs.readFileSync(path)); plan.attachmentChecks.extra = true; fs.writeFileSync(path, JSON.stringify(plan));
NODE
expect_reject extra-attachment-contract "$helper" leaf-prepare --root "$extra_plan_root"

marker_symlink_root=$(make_root)
mv "$marker_symlink_root/.dakia-native-mail-safety-root" "$marker_symlink_root/marker-real"
ln -s "$marker_symlink_root/marker-real" "$marker_symlink_root/.dakia-native-mail-safety-root"
expect_reject marker-symlink "$helper" leaf-prepare --root "$marker_symlink_root"

plan_symlink_root=$(make_root)
mv "$plan_symlink_root/native-acceptance-plan.json" "$plan_symlink_root/plan-real.json"
ln -s "$plan_symlink_root/plan-real.json" "$plan_symlink_root/native-acceptance-plan.json"
expect_reject plan-symlink "$helper" leaf-prepare --root "$plan_symlink_root"

downloads_symlink_root=$(make_root)
rmdir "$downloads_symlink_root/Downloads"
mkdir -m 700 "$downloads_symlink_root/outside-downloads"
ln -s "$downloads_symlink_root/outside-downloads" "$downloads_symlink_root/Downloads"
expect_reject downloads-symlink "$helper" leaf-prepare --root "$downloads_symlink_root"

evidence_symlink_root=$(make_root)
mkdir -m 700 "$evidence_symlink_root/evidence-real"
ln -s "$evidence_symlink_root/evidence-real" "$evidence_symlink_root/native-attachment-fault-evidence"
expect_reject evidence-symlink "$helper" leaf-prepare --root "$evidence_symlink_root"

noncanonical_root=$(make_root)
noncanonical_link=$(mktemp -d "${TMPDIR:-/tmp}/dakia-native-provider-reader.XXXXXX")
rmdir "$noncanonical_link"
ln -s "$noncanonical_root" "$noncanonical_link"
expect_reject noncanonical-root "$helper" leaf-prepare --root "$noncanonical_link"

expect_reject missing-ui-idle "$helper" enospc-prepare --root "$valid_root"
expect_reject missing-resume-state "$helper" enospc-fill-resume --root "$valid_root"

missing_chain_root=$(prepare_leaf_case)
write_unsafe_result "$missing_chain_root"
expect_reject missing-restore-chain "$helper" enospc-retry-check --root "$missing_chain_root"

bad_state_root=$(prepare_leaf_case)
prepare_synthetic_restore_chain "$bad_state_root" 999999999
expect_reject changed-original-inode "$helper" enospc-retry-check --root "$bad_state_root"

retry_root=$(prepare_leaf_case)
prepare_synthetic_restore_chain "$retry_root"
"$helper" enospc-retry-check --root "$retry_root" >/dev/null

tail_unit_root=$(make_root)
tail_unit_script="$tail_unit_root/tail-unit.py"
sed -n "/<<'PY'$/,/^PY$/p" "$helper" | sed '1d;$d' > "$tail_unit_script"
chmod 600 "$tail_unit_script"
tail_unit_filler="$tail_unit_root/Downloads/.dakia-enospc-fill"
printf 'seed' > "$tail_unit_filler"
chmod 600 "$tail_unit_filler"
tail_dir_dev=$(stat -f '%d' "$tail_unit_root/Downloads")
tail_dir_inode=$(stat -f '%i' "$tail_unit_root/Downloads")
tail_file_dev=$(stat -f '%d' "$tail_unit_filler")
tail_file_inode=$(stat -f '%i' "$tail_unit_filler")
tail_uid=$(id -u)
tail_size_before=$(stat -f '%z' "$tail_unit_filler")
expect_reject tail-wrong-inode /usr/bin/python3 "$tail_unit_script" "$tail_unit_root/Downloads" "$tail_dir_dev" "$tail_dir_inode" "$tail_file_dev" 999999999 "$tail_uid"
[[ $(stat -f '%z' "$tail_unit_filler") == "$tail_size_before" ]] || { echo 'wrong-inode tail unit mutated filler' >&2; exit 1; }
expect_reject tail-two-mib-cap /usr/bin/python3 "$tail_unit_script" "$tail_unit_root/Downloads" "$tail_dir_dev" "$tail_dir_inode" "$tail_file_dev" "$tail_file_inode" "$tail_uid"
tail_size_after=$(stat -f '%z' "$tail_unit_filler")
[[ $((tail_size_after - tail_size_before)) -eq 2097152 ]] || { echo 'tail unit did not stop exactly at its 2 MiB cap' >&2; exit 1; }
/usr/bin/grep -q 'tail fill reached the 2 MiB bound without ENOSPC' "$test_log.tail-two-mib-cap.stderr" || { echo 'tail unit did not report its bounded refusal' >&2; exit 1; }

node - "$helper" <<'NODE'
const fs = require('fs');
const source = fs.readFileSync(process.argv[2], 'utf8');
const required = [
  "readonly MAX_IMAGE_BYTES='100663296'",
  "readonly MAX_FS_KIB='98304'",
  "-size 64m -fs 'Journaled HFS+'",
  'hdiutil imageinfo -plist',
  "info.Format !== 'UDRW'",
  'bs=1048576 count=128 conv=fsync',
  'os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW',
  "os.stat('.dakia-enospc-fill', dir_fd=directory_fd, follow_symlinks=False)",
  'os.stat(downloads, follow_symlinks=False)',
  'os.fstatvfs(directory_fd)',
  'chunk_bytes = 4096',
  'maximum_append_bytes = 2 * 1024 * 1024',
  "error.errno != errno.ENOSPC",
  "(( available_kib <= 4 )) || die 'bounded tail fill observed ENOSPC but more than 4 KiB remains available'",
  'validate_existing_fill_artifacts',
  "(( available_kib <= 4 ))",
  'verify_original_downloads_identity "$saved_downloads"',
  'verify_original_downloads_identity "$downloads"',
  'verify_image_detached',
];
for (const text of required) if (!source.includes(text)) throw new Error(`missing static guard: ${text}`);
const createLine = source.split('\n').find(line => line.includes('/usr/bin/hdiutil create')) || '';
if (createLine.includes('-type ') || createLine.includes('-format ')) throw new Error('blank hdiutil create must use its documented default without source-only selectors');
const prepare = source.slice(source.indexOf('  enospc-prepare)'), source.indexOf('  enospc-fill)'));
const createIndex = prepare.indexOf('/usr/bin/hdiutil create');
const chmodIndex = prepare.indexOf('/bin/chmod 600 "$image"');
const inspectIndex = prepare.indexOf('/usr/bin/hdiutil imageinfo -plist');
const moveIndex = prepare.indexOf('/bin/mv "$downloads" "$saved_downloads"');
if (!(createIndex >= 0 && chmodIndex > createIndex && inspectIndex > chmodIndex && moveIndex > inspectIndex)) throw new Error('blank image must be privatized and verified before Downloads moves');
const resume = source.slice(source.indexOf('  enospc-fill-resume)'), source.indexOf('  enospc-check)'));
const resumeReportIndex = resume.indexOf('require_report enospc-prepare.json enospc-prepare');
const resumeMountIndex = resume.indexOf('verify_owned_mount');
const resumeNoFinalIndex = resume.indexOf('final ENOSPC fill evidence already exists');
const resumeTailIndex = resume.indexOf('tail_fill_to_enospc');
const resumePublishIndex = resume.indexOf('write_json_exclusive "$evidence/enospc-fill.json"');
if (!(resumeReportIndex >= 0 && resumeMountIndex > resumeReportIndex && resumeNoFinalIndex > resumeMountIndex && resumeTailIndex > resumeNoFinalIndex && resumePublishIndex > resumeTailIndex)) throw new Error('resume must verify its exact chain and mount before append and final evidence');
if (source.includes('$tail_report.tmp')) throw new Error('tail fill must not leave a fixed retry-blocking temp file');
const check = source.slice(source.indexOf('  enospc-check)'), source.indexOf('  enospc-restore)'));
const checkValidateIndex = check.indexOf('validate_existing_fill_artifacts');
const checkFreeIndex = check.indexOf('available_kib <= 4');
const checkPublishIndex = check.indexOf('write_json_exclusive "$evidence/enospc-check.json"');
if (!(checkValidateIndex >= 0 && checkFreeIndex > checkValidateIndex && checkPublishIndex > checkFreeIndex)) throw new Error('ENOSPC check must revalidate filler identity and fullness before evidence');
const restore = source.slice(source.indexOf('  enospc-restore)'));
const verifyIndex = restore.indexOf('verify_owned_mount');
const detachIndex = restore.indexOf('/usr/bin/hdiutil detach');
const restoreIndex = restore.indexOf('/bin/mv "$saved_downloads" "$downloads"');
if (!(verifyIndex >= 0 && detachIndex > verifyIndex && restoreIndex > detachIndex)) throw new Error('restore order guard is not preserved');
if (/(^|[^A-Za-z_])(HOME|home|CODEX_HOME)=/m.test(source)) throw new Error('helper assigns a forbidden home variable');
NODE

printf 'PASS: leaf lifecycle, corruption sensitivity, namespace and symlink guards, restore-chain identity, quarantine format, and static ENOSPC bounds/order.\n'
printf 'Synthetic logs retained at %s\n' "$test_log"
printf 'Synthetic roots are retained under the private temp directory with the dakia-native-provider-reader.* namespace.\n'
