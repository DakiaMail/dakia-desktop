import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const helperPath = path.join(
  scriptDirectory,
  "native-attachment-fault-acceptance.sh",
);
const shellTestPath = path.join(
  scriptDirectory,
  "test-native-attachment-fault-acceptance.sh",
);
const helperSource = fs.readFileSync(helperPath, "utf8");
const shellTestSource = fs.readFileSync(shellTestPath, "utf8");

function run(command, args, options = {}) {
  return spawnSync(command, args, {
    encoding: "utf8",
    timeout: 30_000,
    ...options,
  });
}

function extractHeredoc(source, opener) {
  const openerIndex = source.indexOf(opener);
  assert.notEqual(openerIndex, -1, `missing heredoc opener: ${opener}`);
  const bodyStart = openerIndex + opener.length;
  const terminator = opener.endsWith("'PY'\n") ? "\nPY\n" : "\nNODE\n";
  const bodyEnd = source.indexOf(terminator, bodyStart);
  assert.notEqual(bodyEnd, -1, `missing heredoc terminator after: ${opener}`);
  return source.slice(bodyStart, bodyEnd);
}

function writeJson(file, value) {
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`, {
    mode: 0o600,
  });
}

function capturedImageInfo() {
  return {
    "Class Name": "CRawDiskImage",
    Format: "UDRW",
    "Format Description": "raw read/write",
    "Size Information": { "Total Bytes": 67_108_864 },
    partitions: {
      partitions: [
        {
          "partition-name": "Protective Master Boot Record",
          "partition-hint": "MBR",
        },
        {
          "partition-name": "disk image",
          "partition-hint": "Apple_HFS",
          "partition-filesystems": { "HFS+": "" },
        },
      ],
    },
  };
}

function extractCaptureFunction(source) {
  const start = source.indexOf("capture_hdiutil_info() {\n");
  assert.notEqual(start, -1, "capture_hdiutil_info function is missing");
  const end = source.indexOf("\n}\n\nverify_owned_mount()", start);
  assert.notEqual(
    end,
    -1,
    "capture_hdiutil_info function terminator is missing",
  );
  return source.slice(start, end + 3);
}

function exerciseCaptureFunction(
  source,
  replacement = '/bin/cat "$fixture_plist"',
) {
  const capture = extractCaptureFunction(source).replace(
    "/usr/bin/hdiutil info -plist",
    replacement,
  );
  const root = fs.mkdtempSync(
    path.join(os.tmpdir(), "dakia-hdiutil-observation-"),
  );
  const evidence = path.join(root, "evidence");
  const fixture = path.join(root, "fixture.plist");
  const harness = path.join(root, "exercise.sh");
  fs.mkdirSync(evidence, { mode: 0o700 });
  fs.writeFileSync(fixture, '<?xml version="1.0"?><plist><dict/></plist>\n', {
    mode: 0o600,
  });
  fs.writeFileSync(
    harness,
    `#!/bin/bash
set -euo pipefail
umask 077
evidence=$1
fixture_plist=$2
die() { printf 'ERROR: %s\\n' "$*" >&2; exit 1; }
${capture}
first=$(capture_hdiutil_info repeated-observation)
second=$(capture_hdiutil_info repeated-observation)
[[ "$first" != "$second" ]]
[[ -f "$first" && ! -L "$first" && -f "$second" && ! -L "$second" ]]
/usr/bin/cmp -s "$first" "$fixture_plist"
/usr/bin/cmp -s "$second" "$fixture_plist"
printf '%s\\n%s\\n' "$first" "$second"
`,
    { mode: 0o700 },
  );
  return { result: run("/bin/bash", [harness, evidence, fixture]), evidence };
}

test("both shell scripts pass bash syntax validation and use a relative helper path", () => {
  for (const file of [helperPath, shellTestPath]) {
    const result = run("/bin/bash", ["-n", file]);
    assert.equal(result.status, 0, `${file}\n${result.stderr}`);
  }
  assert.match(
    shellTestSource,
    /helper="\$script_dir\/native-attachment-fault-acceptance\.sh"/,
  );
  assert.doesNotMatch(
    shellTestSource,
    /\/private\/tmp\/dakia-native-attachment-fault-helper/,
  );
});

test("blank image creation uses the validated default and inspects before moving Downloads", () => {
  const createLine = helperSource
    .split("\n")
    .find((line) => line.includes("/usr/bin/hdiutil create"));
  assert.ok(createLine);
  assert.match(
    createLine,
    /-size 64m -fs 'Journaled HFS\+' -volname DakiaAttachmentENOSPC -nospotlight/,
  );
  assert.doesNotMatch(createLine, /-type\s/);
  assert.doesNotMatch(createLine, /-format\s/);
  assert.ok(
    helperSource.indexOf("/usr/bin/hdiutil imageinfo -plist") >
      helperSource.indexOf("/usr/bin/hdiutil create"),
  );
  assert.ok(
    helperSource.indexOf('/bin/mv "$downloads" "$saved_downloads"') >
      helperSource.indexOf("/usr/bin/hdiutil imageinfo -plist"),
  );

  const rejectedSelectors = [
    `${createLine} -type UDIF`,
    `${createLine} -format UDRW`,
  ];
  for (const mutated of rejectedSelectors) {
    assert.match(
      mutated,
      /-(?:type|format)\s/,
      "the sensitivity control must contain a rejected selector",
    );
  }
});

test("the extracted image-info oracle accepts only the captured 64 MiB UDRW HFS+ shape", () => {
  const root = fs.mkdtempSync(
    path.join(os.tmpdir(), "dakia-imageinfo-oracle-"),
  );
  const validatorPath = path.join(root, "validator.cjs");
  const opener =
    "node - \"$imageinfo_json\" <<'NODE' || die 'created disk image is not an exact 64 MiB UDRW image with one HFS+ partition'\n";
  fs.writeFileSync(validatorPath, extractHeredoc(helperSource, opener), {
    mode: 0o600,
  });

  const cases = [
    ["valid", capturedImageInfo(), 0],
    ["wrong-format", { ...capturedImageInfo(), Format: "UDZO" }, 1],
    [
      "wrong-size",
      {
        ...capturedImageInfo(),
        "Size Information": { "Total Bytes": 67_108_863 },
      },
      1,
    ],
    [
      "missing-hfs",
      { ...capturedImageInfo(), partitions: { partitions: [] } },
      1,
    ],
    [
      "wrong-partition-hint",
      {
        ...capturedImageInfo(),
        partitions: {
          partitions: [
            {
              "partition-hint": "Apple_APFS",
              "partition-filesystems": { "HFS+": "" },
            },
          ],
        },
      },
      1,
    ],
  ];

  for (const [name, value, expectedStatus] of cases) {
    const infoPath = path.join(root, `${name}.json`);
    writeJson(infoPath, value);
    const result = run(process.execPath, [validatorPath, infoPath]);
    assert.equal(result.status, expectedStatus, `${name}\n${result.stderr}`);
  }
});

test("hdiutil observations use unique exclusive paths even within one process and second", () => {
  const fixed = exerciseCaptureFunction(helperSource);
  assert.equal(fixed.result.status, 0, fixed.result.stderr);
  const paths = fixed.result.stdout.trim().split("\n");
  assert.equal(paths.length, 2);
  assert.notEqual(paths[0], paths[1]);
  for (const capturedPath of paths) {
    assert.equal(fs.statSync(capturedPath).mode & 0o777, 0o600);
    assert.match(
      path.basename(capturedPath),
      /^hdiutil-info-repeated-observation-[A-Za-z0-9]+\.plist$/,
    );
  }

  const brokenCapture = `capture_hdiutil_info() {
  local plist="$evidence/hdiutil-info-$1.plist"
  [[ ! -e "$plist" ]] || die "refusing to overwrite hdiutil evidence: $plist"
  /usr/bin/hdiutil info -plist > "$plist.tmp"
  /bin/chmod 600 "$plist.tmp"
  /bin/mv "$plist.tmp" "$plist"
  /usr/bin/printf '%s' "$plist"
}
`;
  const brokenSource = helperSource.replace(
    extractCaptureFunction(helperSource),
    brokenCapture,
  );
  const sensitivity = exerciseCaptureFunction(brokenSource);
  assert.notEqual(
    sensitivity.result.status,
    0,
    "fixed-name implementation unexpectedly survived the collision",
  );
  assert.match(
    sensitivity.result.stderr,
    /refusing to overwrite hdiutil evidence/,
  );

  const failed = exerciseCaptureFunction(
    helperSource,
    "/bin/sh -c 'printf partial-hdiutil-output; exit 17'",
  );
  assert.notEqual(
    failed.result.status,
    0,
    "failed hdiutil observation unexpectedly succeeded",
  );
  assert.match(
    failed.result.stderr,
    /hdiutil info failed; private failure evidence retained/,
  );
  const failureArtifacts = fs
    .readdirSync(failed.evidence)
    .filter((name) => name.endsWith(".plist.failure"));
  assert.equal(failureArtifacts.length, 1);
  const failurePath = path.join(failed.evidence, failureArtifacts[0]);
  assert.equal(fs.readFileSync(failurePath, "utf8"), "partial-hdiutil-output");
  assert.equal(fs.statSync(failurePath).mode & 0o777, 0o600);
});

test(
  "the extracted tail core rejects an inode mismatch and stops at the two MiB no-ENOSPC bound",
  { skip: process.platform === "win32" },
  () => {
    const python = process.env.PYTHON ?? "python3";
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "dakia-tail-core-"));
    const downloads = path.join(root, "Downloads");
    const filler = path.join(downloads, ".dakia-enospc-fill");
    const corePath = path.join(root, "tail-core.py");
    fs.mkdirSync(downloads, { mode: 0o700 });
    fs.writeFileSync(filler, "seed", { mode: 0o600 });
    fs.writeFileSync(corePath, extractHeredoc(helperSource, "<<'PY'\n"), {
      mode: 0o600,
    });

    const directory = fs.statSync(downloads, { bigint: true });
    const file = fs.statSync(filler, { bigint: true });
    const args = [
      corePath,
      downloads,
      directory.dev.toString(),
      directory.ino.toString(),
      file.dev.toString(),
      file.ino.toString(),
      String(process.getuid()),
    ];
    const initialSize = fs.statSync(filler).size;

    const wrongInode = [...args];
    wrongInode[5] = (file.ino + 1n).toString();
    const mismatch = run(python, wrongInode);
    assert.notEqual(mismatch.status, 0);
    assert.match(mismatch.stderr, /identity changed before open/);
    assert.equal(fs.statSync(filler).size, initialSize);

    const bounded = run(python, args);
    assert.notEqual(bounded.status, 0);
    assert.match(
      bounded.stderr,
      /tail fill reached the 2 MiB bound without ENOSPC/,
    );
    assert.equal(fs.statSync(filler).size - initialSize, 2 * 1024 * 1024);
  },
);

test(
  "the extracted tail core refuses a symlink leaf without changing its target",
  { skip: process.platform === "win32" },
  () => {
    const python = process.env.PYTHON ?? "python3";
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "dakia-tail-symlink-"));
    const downloads = path.join(root, "Downloads");
    const target = path.join(root, "target");
    const filler = path.join(downloads, ".dakia-enospc-fill");
    const corePath = path.join(root, "tail-core.py");
    fs.mkdirSync(downloads, { mode: 0o700 });
    fs.writeFileSync(target, "canary", { mode: 0o600 });
    fs.symlinkSync(target, filler);
    fs.writeFileSync(corePath, extractHeredoc(helperSource, "<<'PY'\n"), {
      mode: 0o600,
    });
    const directory = fs.statSync(downloads, { bigint: true });
    const targetStat = fs.statSync(target, { bigint: true });
    const result = run(python, [
      corePath,
      downloads,
      directory.dev.toString(),
      directory.ino.toString(),
      targetStat.dev.toString(),
      targetStat.ino.toString(),
      String(process.getuid()),
    ]);
    assert.notEqual(result.status, 0);
    assert.equal(fs.readFileSync(target, "utf8"), "canary");
  },
);

test(
  "the complete fictional macOS synthetic suite passes without creating or mounting an image",
  { skip: process.platform !== "darwin" },
  () => {
    const result = run("/bin/bash", [shellTestPath], { timeout: 60_000 });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.match(result.stdout, /PASS: leaf lifecycle/);
  },
);
