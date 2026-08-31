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
  assert.match(
    workflow,
    /APPLE_SIGNING_IDENTITY: \$\{\{ matrix\.platform == 'macos' && '-' \|\| '' \}\}/,
  );
  assert.doesNotMatch(workflow, /actions\/upload-artifact|retention-days:/);
  assert.match(workflow, /hdiutil attach/);
  assert.match(workflow, /NSIS installer failed/);
  assert.match(workflow, /Installed Dakia CLI sidecar is missing/);
  assert.match(workflow, /uninstall\.exe/);
  assert.match(workflow, /group: hosted-build-measurement/);
  assert.match(workflow, /bash \.\/scripts\/prepare-desktop-assets\.sh/);
  assert.match(workflow, /-- --locked/);
  assert.match(workflow, /RUSTC_WRAPPER=sccache/);
  assert.match(workflow, /SCCACHE_GHA_ENABLED=on/);
  assert.match(workflow, /SCCACHE_GHA_RW_MODE=READ_WRITE/);
  assert.match(
    workflow,
    /name: Report Rust compiler cache statistics\n\s+if: always\(\)\n\s+run: sccache --show-stats/,
  );
  assert.match(workflow, /sccache-v0\.17\.0-aarch64-apple-darwin\.tar\.gz/);
  assert.match(workflow, /sccache-v0\.17\.0-x86_64-apple-darwin\.tar\.gz/);
  assert.match(workflow, /sccache-v0\.17\.0-x86_64-pc-windows-msvc\.tar\.gz/);
  assert.match(workflow, /sccache_sha256/);
  assert.doesNotMatch(workflow, /cargo install sccache/);
  assert.match(
    workflow,
    /SCCACHE_GHA_VERSION=dakia-\$\{\{ matrix\.target \}\}-rust-1\.89\.0-v1/,
  );
  assert.match(
    workflow,
    /restore-keys: \|\n\s+npm-\$\{\{ runner\.os \}\}-\$\{\{ runner\.arch \}\}-node-20\.19\.0-/,
  );
  for (const action of workflow.matchAll(/uses:\s*[^@\s]+@([^\s#]+)/g)) {
    assert.match(action[1], /^[a-f0-9]{40}$/, `unpinned action: ${action[0]}`);
  }
  assert.doesNotMatch(
    workflow,
    /gh release|gh api|TAURI_SIGNING_PRIVATE_KEY|notary/i,
  );
});

test("Windows CLI sidecars are built for the MSVC target", () => {
  assert.match(cliBundler, /windows \| mingw\* \| msys\*\)/);
  assert.match(cliBundler, /target="\$\{arch\}-pc-windows-msvc"/);
  assert.match(cliBundler, /executable=dakia\.exe/);
  assert.match(cliBundler, /destination="\$destination\.exe"/);
  assert.match(
    cliBundler,
    /\$repo_root\/target\/\$target\/\$cargo_profile\/\$executable/,
  );
  assert.match(cliBundler, /cargo build --locked/);
});
