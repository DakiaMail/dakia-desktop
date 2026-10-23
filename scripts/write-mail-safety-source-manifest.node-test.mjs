import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmodSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { execFileSync, spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";

import { sourceSnapshot } from "./run-mail-safety.mjs";
import { writeMailSafetySourceManifest } from "./write-mail-safety-source-manifest.mjs";

const script = new URL(
  "./write-mail-safety-source-manifest.mjs",
  import.meta.url,
).pathname;

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function fixture() {
  const base = mkdtempSync(join(tmpdir(), "dakia-source-manifest-"));
  const root = join(base, "checkout");
  const evidence = join(base, "evidence");
  mkdirSync(join(root, "scripts"), { recursive: true });
  mkdirSync(join(root, "apps/desktop/src"), { recursive: true });
  mkdirSync(evidence);
  execFileSync("git", ["init", "--quiet"], { cwd: root });
  writeFileSync(
    join(root, "scripts/tracked.mjs"),
    "export const tracked = true;\n",
  );
  writeFileSync(
    join(root, "scripts/untracked.mjs"),
    "export const untracked = true;\n",
  );
  writeFileSync(join(root, "apps/desktop/src/view.ts"), "export {};\n");
  writeFileSync(join(root, "README.md"), "outside governed paths\n");
  execFileSync(
    "git",
    ["add", "scripts/tracked.mjs", "apps/desktop/src/view.ts"],
    { cwd: root },
  );
  return { base, root, evidence };
}

function run(root, ...args) {
  return spawnSync(process.execPath, [script, ...args], {
    cwd: root,
    encoding: "utf8",
  });
}

test("writes the exact governed source manifest, includes untracked inputs, and emits an exclusive Gmail summary", () => {
  const paths = fixture();
  try {
    const manifestPath = join(paths.evidence, "source-manifest.json");
    const summaryPath = join(paths.evidence, "summary.json");
    const result = run(
      paths.root,
      "--output",
      manifestPath,
      "--summary-output",
      summaryPath,
    );
    assert.equal(result.status, 0, result.stderr);
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    assert.deepEqual(Object.keys(manifest), [
      "algorithm",
      "digest",
      "fileCount",
      "files",
    ]);
    assert.ok(
      manifest.files.some((row) => row.path === "scripts/untracked.mjs"),
    );
    assert.ok(!manifest.files.some((row) => row.path === "README.md"));
    assert.deepEqual(
      manifest.files.map((row) => Object.keys(row)),
      manifest.files.map(() => ["path", "bytes", "sha256", "state"]),
    );
    assert.ok(manifest.files.every((row) => row.state === "present"));
    const prior = process.cwd();
    try {
      process.chdir(paths.root);
      const snapshot = sourceSnapshot();
      assert.equal(manifest.digest, snapshot.digest);
      assert.equal(manifest.fileCount, snapshot.fileCount);
    } finally {
      process.chdir(prior);
    }
    const summary = JSON.parse(readFileSync(summaryPath, "utf8"));
    assert.deepEqual(Object.keys(summary), [
      "algorithm",
      "digest",
      "fileCount",
    ]);
    assert.equal(summary.algorithm, manifest.algorithm);
    assert.equal(summary.digest, manifest.digest);
    assert.equal(summary.fileCount, manifest.fileCount);
    assert.equal(lstatSync(manifestPath).mode & 0o777, 0o600);
    assert.equal(lstatSync(summaryPath).mode & 0o777, 0o600);
  } finally {
    rmSync(paths.base, { recursive: true, force: true });
  }
});

test("rejects output inside the checkout, existing files, and output symlinks", () => {
  const paths = fixture();
  try {
    const inside = run(
      paths.root,
      "--output",
      join(paths.root, "scripts/manifest.json"),
    );
    assert.notEqual(inside.status, 0);
    assert.match(inside.stderr, /outside the source checkout/);
    const existing = join(paths.evidence, "existing.json");
    writeFileSync(existing, "existing");
    const existingResult = run(paths.root, "--output", existing);
    assert.notEqual(existingResult.status, 0);
    assert.match(existingResult.stderr, /already exists/);
    const target = join(paths.evidence, "target.json");
    writeFileSync(target, "target");
    const link = join(paths.evidence, "link.json");
    symlinkSync(target, link);
    const symlinkResult = run(paths.root, "--output", link);
    assert.notEqual(symlinkResult.status, 0);
    assert.match(symlinkResult.stderr, /already exists/);
  } finally {
    rmSync(paths.base, { recursive: true, force: true });
  }
});

test("fails closed for deleted and symlinked governed source inputs", () => {
  const paths = fixture();
  try {
    unlinkSync(join(paths.root, "scripts/tracked.mjs"));
    const missing = run(
      paths.root,
      "--output",
      join(paths.evidence, "missing.json"),
    );
    assert.notEqual(missing.status, 0);
    assert.match(missing.stderr, /input is missing/);
    writeFileSync(join(paths.root, "scripts/tracked.mjs"), "tracked again\n");
    unlinkSync(join(paths.root, "scripts/tracked.mjs"));
    symlinkSync("untracked.mjs", join(paths.root, "scripts/tracked.mjs"));
    const symlink = run(
      paths.root,
      "--output",
      join(paths.evidence, "symlink.json"),
    );
    assert.notEqual(symlink.status, 0);
    assert.match(symlink.stderr, /input is a symlink/);
  } finally {
    rmSync(paths.base, { recursive: true, force: true });
  }
});

test("does not write a manifest when governed source changes during generation", () => {
  const paths = fixture();
  try {
    const output = join(paths.evidence, "drift.json");
    const prior = process.cwd();
    try {
      process.chdir(paths.root);
      let calls = 0;
      assert.throws(
        () =>
          writeMailSafetySourceManifest({
            output,
            snapshot: () => {
              calls += 1;
              if (calls === 2) {
                writeFileSync(
                  join(paths.root, "scripts/untracked.mjs"),
                  "changed during generation\n",
                );
              }
              return sourceSnapshot();
            },
          }),
        /Governed source changed before the manifest could be written/,
      );
    } finally {
      process.chdir(prior);
    }
    assert.throws(() => lstatSync(output), /ENOENT/);
  } finally {
    rmSync(paths.base, { recursive: true, force: true });
  }
});
