import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const privateHelper = '/private/tmp/dakia-native-attachment-observation-fix-20261021/native-attachment-fault-acceptance.sh';
const brokenHelper = '/Users/arsalan/code/codex-worktrees/36f9/dakia-desktop/scripts/native-attachment-fault-acceptance.sh';

function extractCapture(source) {
  const start = source.indexOf('capture_hdiutil_info() {\n');
  assert.notEqual(start, -1, 'capture_hdiutil_info function is missing');
  const end = source.indexOf('\n}\n\nverify_owned_mount()', start);
  assert.notEqual(end, -1, 'capture_hdiutil_info terminator is missing');
  return source.slice(start, end + 3);
}

function exercise(helper) {
  const source = fs.readFileSync(helper, 'utf8');
  const capture = extractCapture(source).replace(
    '/usr/bin/hdiutil info -plist',
    '/bin/cat "$fixture_plist"',
  );
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'dakia-hdiutil-observation-'));
  const evidence = path.join(root, 'evidence');
  fs.mkdirSync(evidence, { mode: 0o700 });
  const fixture = path.join(root, 'fixture.plist');
  fs.writeFileSync(fixture, '<?xml version="1.0"?><plist><dict/></plist>\n', { mode: 0o600 });
  const harness = path.join(root, 'exercise.sh');
  fs.writeFileSync(harness, `#!/bin/bash\nset -euo pipefail\numask 077\nevidence=$1\nfixture_plist=$2\ndie() { printf 'ERROR: %s\\n' "$*" >&2; exit 1; }\n${capture}\nfirst=$(capture_hdiutil_info repeated-observation)\nsecond=$(capture_hdiutil_info repeated-observation)\n[[ "$first" != "$second" ]]\n[[ -f "$first" && ! -L "$first" && -f "$second" && ! -L "$second" ]]\n[[ $(stat -f '%Lp' "$first") == 600 && $(stat -f '%Lp' "$second") == 600 ]]\ncmp -s "$first" "$fixture_plist"\ncmp -s "$second" "$fixture_plist"\nprintf '%s\\n%s\\n' "$first" "$second"\n`, { mode: 0o700 });
  return spawnSync('/bin/bash', [harness, evidence, fixture], { encoding: 'utf8', timeout: 10_000 });
}

const fixedSource = fs.readFileSync(privateHelper, 'utf8');
assert.match(fixedSource, /mktemp "\$evidence\/\.hdiutil-info-\$label\.XXXXXXXX"/);
assert.match(fixedSource, /hdiutil-info-\$label-\$token\.plist/);
assert.doesNotMatch(fixedSource, /local plist="\$evidence\/hdiutil-info-\$1\.plist"/);

const fixed = exercise(privateHelper);
assert.equal(fixed.status, 0, `fixed helper must preserve two same-label observations\n${fixed.stderr}`);
const fixedPaths = fixed.stdout.trim().split('\n');
assert.equal(fixedPaths.length, 2);
assert.notEqual(fixedPaths[0], fixedPaths[1]);

const sensitivity = exercise(brokenHelper);
assert.notEqual(sensitivity.status, 0, 'broken fixed-name helper unexpectedly passed collision regression');
assert.match(sensitivity.stderr, /refusing to overwrite hdiutil evidence/);

console.log('PASS: unique observation paths survive two same-label captures; the checked-in fixed-name implementation fails the same regression.');
