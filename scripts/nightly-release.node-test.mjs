import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";

import {
  nextPatch,
  releaseNotes,
  userFacingSubject,
} from "./prepare-nightly-release.mjs";

const root = resolve(new URL("..", import.meta.url).pathname);

test("nightly releases use the next patch version", () => {
  assert.equal(nextPatch("0.4.0"), "0.4.1");
  assert.throws(() => nextPatch("0.4"), /stable semantic version/);
});

test("generated notes retain reader-facing changes and hide release mechanics", () => {
  assert.equal(userFacingSubject("Add feedback button to the mailbox sidebar (#75)"), "Send feedback directly from the mailbox sidebar.");
  assert.equal(userFacingSubject("Interactive email address header actions (#74)"), "Work with email addresses directly from message headers.");
  assert.equal(userFacingSubject("Merge branch 'main'"), null);
  assert.equal(userFacingSubject("chore: Bump clap from 4.6.2 to 4.6.6"), null);
  const notes = releaseNotes({
    version: "0.4.1",
    subjects: [
      "Add feedback button to the mailbox sidebar (#75)",
      "Merge branch 'main'",
      "chore: Bump clap from 4.6.2 to 4.6.6",
    ],
  });
  assert.match(notes, /## What changed/);
  assert.match(notes, /Send feedback directly from the mailbox sidebar\./);
  assert.doesNotMatch(notes, /Merge branch|Bump clap/);
  assert.match(notes, /Apple Silicon Macs/);
});

test("nightly workflow is main-only, serialized, and uses the trusted release runner", () => {
  const workflow = readFileSync(resolve(root, ".github/workflows/nightly-release.yml"), "utf8");
  assert.match(workflow, /cron: '17 02 \* \* \*'/);
  assert.match(workflow, /github\.ref == 'refs\/heads\/main'/);
  assert.match(workflow, /cancel-in-progress: false/);
  assert.match(workflow, /\[self-hosted, macos, arm64, dakia-release\]/);
  assert.match(workflow, /environment: production-release/);
});

test("release runner checks the published updater baseline before preparing and publishing", () => {
  const runner = readFileSync(resolve(root, "scripts/run-nightly-release.sh"), "utf8");
  assert.match(runner, /macos\/latest\/latest\.json/);
  assert.match(runner, /No source changes since/);
  assert.match(runner, /git merge-base --is-ancestor/);
  assert.match(runner, /npm run verify:local/);
  assert.match(runner, /npm run release:build/);
  assert.match(runner, /npm run release:publish/);
  assert.match(runner, /gh release create.*--draft/);
  assert.match(runner, /gh release edit.*--draft=false/);
});
