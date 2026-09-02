import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  nextPatch,
  parseCliArgs,
  releaseNotes,
  userFacingSubject,
} from "./prepare-nightly-release.mjs";

test("nightly releases use the next patch version", () => {
  assert.equal(nextPatch("0.4.0"), "0.4.1");
  assert.throws(() => nextPatch("0.4"), /stable semantic version/);
});

test("generated notes retain reader-facing changes and hide release mechanics", () => {
  assert.equal(
    userFacingSubject("Add feedback button to the mailbox sidebar (#75)"),
    "Send feedback directly from the mailbox sidebar.",
  );
  assert.equal(
    userFacingSubject("Interactive email address header actions (#74)"),
    "Work with email addresses directly from message headers.",
  );
  assert.equal(userFacingSubject("Merge branch 'main'"), null);
  assert.equal(userFacingSubject("chore: Bump clap from 4.6.2 to 4.6.6"), null);
  const notes = releaseNotes({
    version: "0.4.3",
    subjects: [
      "Add privacy-preserving usage analytics (#83)",
      "Fix people categorization across desktop, CLI, and core (#78)",
      "Optimize CI caching and harden release validation boundaries (#82)",
      "Add Send again action for sent messages (#81)",
      "Make archive and read mutations optimistic (#80)",
      "Merge branch 'main'",
    ],
  });
  assert.match(notes, /Added privacy-preserving usage analytics\./);
  assert.match(notes, /Improve people categorization/);
  assert.doesNotMatch(notes, /Optimize CI caching|strengthen release validation/);
  assert.match(notes, /new Send again action\./);
  assert.match(notes, /faster optimistic updates\./);
  assert.doesNotMatch(notes, /Merge branch/);
  assert.match(notes, /available for macOS, Linux, and Windows/);
});

test("dry-run preserves the positional CLI and accepts either flag order", () => {
  assert.deepEqual(parseCliArgs(["abc123"]), {
    base: "abc123",
    dryRun: false,
  });
  assert.deepEqual(parseCliArgs(["abc123", "--dry-run"]), {
    base: "abc123",
    dryRun: true,
  });
  assert.deepEqual(parseCliArgs(["--dry-run", "abc123"]), {
    base: "abc123",
    dryRun: true,
  });
  assert.throws(() => parseCliArgs([]), /Usage/);
  assert.throws(() => parseCliArgs(["abc", "extra"]), /Usage/);
});

test("dry-run prints preview JSON without changing its checkout", (context) => {
  const fixture = mkdtempSync(join(tmpdir(), "dakia-nightly-preview-"));
  context.after(() => rmSync(fixture, { recursive: true, force: true }));
  mkdirSync(join(fixture, "scripts"));
  mkdirSync(join(fixture, "docs", "releases"), { recursive: true });
  copyFileSync(
    new URL("./prepare-nightly-release.mjs", import.meta.url),
    join(fixture, "scripts", "prepare-nightly-release.mjs"),
  );
  writeFileSync(join(fixture, "package.json"), '{"version":"1.2.3"}\n');
  const git = (...args) =>
    spawnSync("git", args, { cwd: fixture, encoding: "utf8" });
  assert.equal(git("init", "-q").status, 0);
  assert.equal(git("config", "user.name", "Dakia Test").status, 0);
  assert.equal(git("config", "user.email", "test@example.invalid").status, 0);
  assert.equal(git("add", ".").status, 0);
  assert.equal(git("commit", "-qm", "baseline").status, 0);
  const base = git("rev-parse", "HEAD").stdout.trim();
  writeFileSync(join(fixture, "change.txt"), "reader-facing\n");
  assert.equal(git("add", ".").status, 0);
  assert.equal(
    git("commit", "-qm", "Add privacy-preserving usage analytics").status,
    0,
  );

  const beforePackage = readFileSync(join(fixture, "package.json"), "utf8");
  const result = spawnSync(
    process.execPath,
    [
      join(fixture, "scripts", "prepare-nightly-release.mjs"),
      base,
      "--dry-run",
    ],
    { cwd: fixture, encoding: "utf8" },
  );
  assert.equal(result.status, 0, result.stderr);
  const preview = JSON.parse(result.stdout);
  assert.equal(preview.tag, "v1.2.4");
  assert.match(preview.notes, /privacy-preserving usage analytics/);
  assert.equal(
    readFileSync(join(fixture, "package.json"), "utf8"),
    beforePackage,
  );
  assert.equal(
    existsSync(join(fixture, "docs", "releases", "v1.2.4.md")),
    false,
  );
  assert.equal(git("status", "--porcelain").stdout, "");
});
