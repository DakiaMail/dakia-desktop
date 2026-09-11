import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const workflow = readFileSync(
  resolve(root, ".github/workflows/production-release.yml"),
  "utf8",
);

test("release builds run after the optional preparation job is skipped", () => {
  const buildJob = workflow.match(/\n  build:\n([\s\S]*?)\n  publish:\n/)?.[1];
  assert.ok(buildJob, "production release build job is missing");
  assert.match(buildJob, /if:\s*>-\s*\n\s+always\(\)/);
  assert.match(buildJob, /needs\.decide\.result == 'success'/);
  assert.match(buildJob, /needs\.validate\.result == 'success'/);
  assert.match(buildJob, /needs\.decide\.outputs\.should_release == 'true'/);
});
