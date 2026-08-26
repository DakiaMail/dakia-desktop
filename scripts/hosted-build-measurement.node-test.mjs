import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const workflow = readFileSync(
  join(root, ".github", "workflows", "hosted-build-measurement.yml"),
  "utf8",
);
const cliBundler = readFileSync(join(root, "scripts", "bundle-cli.sh"), "utf8");

test("hosted build measurement is dispatch-only and explicitly opt-in", () => {
  assert.match(workflow, /^on:\n  workflow_dispatch:/m);
  assert.match(workflow, /run_hosted_measurement:/);
  assert.match(workflow, /test "\$ENABLED" = true/);
  assert.doesNotMatch(workflow, /^  push:|^  schedule:/m);
});

test("hosted build measurement covers both Macs and Windows without release writes", () => {
  for (const value of ["macos-15", "macos-15-intel", "windows-2025"]) {
    assert.match(workflow, new RegExp(`runner: ${value}`));
  }
  assert.match(workflow, /aarch64-apple-darwin/);
  assert.match(workflow, /x86_64-apple-darwin/);
  assert.match(workflow, /x86_64-pc-windows-msvc/);
  assert.match(workflow, /createUpdaterArtifacts":false/);
  assert.match(workflow, /beforeBuildCommand":""/);
  assert.match(workflow, /APPLE_SIGNING_IDENTITY: \$\{\{ matrix\.platform == 'macos' && '-' \|\| '' \}\}/);
  assert.match(workflow, /retention-days: 1/);
  assert.match(workflow, /hdiutil attach/);
  assert.match(workflow, /Verify Windows installer exists/);
  assert.match(workflow, /group: hosted-build-measurement/);
  assert.doesNotMatch(workflow, /gh release|gh api|TAURI_SIGNING_PRIVATE_KEY|notary/i);
});

test("Windows CLI sidecars keep Tauri's target-qualified executable name", () => {
  assert.match(cliBundler, /windows \| mingw\* \| msys\*\)/);
  assert.match(cliBundler, /target="\$\{arch\}-pc-windows-msvc"/);
  assert.match(cliBundler, /executable=dakia\.exe/);
  assert.match(cliBundler, /destination="\$destination\.exe"/);
  assert.match(cliBundler, /\$repo_root\/target\/\$target\/\$cargo_profile\/\$executable/);
  assert.match(cliBundler, /cargo build --locked/);
});
