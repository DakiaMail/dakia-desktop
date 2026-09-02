import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const draftScript = join(root, "scripts", "prepare-github-release-draft.sh");
const publishScript = join(root, "scripts", "publish-github-release.sh");
const packageJson = JSON.parse(
  readFileSync(join(root, "package.json"), "utf8"),
);

function script(path) {
  return readFileSync(path, "utf8");
}

test("GitHub release-stage scripts are syntactically valid Bash", () => {
  for (const path of [draftScript, publishScript]) {
    const result = spawnSync("bash", ["-n", path], { encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
  }
});

test("draft staging is pinned to clean exact main provenance and an SSH-signed remote tag", () => {
  const source = script(draftScript);
  assert.match(source, /release_repo="DakiaMail\/dakia-desktop"/);
  assert.match(source, /status --porcelain=v1 --untracked-files=all/);
  assert.match(source, /branch --show-current\)" == "main"/);
  assert.match(source, /dakia_require_live_main_provenance "\$root_dir"/);
  assert.match(source, /local-release-env\.sh/);
  assert.match(source, /gpg\.format/);
  assert.match(source, /gpg\.ssh\.allowedSignersFile/);
  assert.match(source, /SHA256:kN9R3QFJZbrE5i2HjEpp\+ns5ZNxBTuFySvFx8Ldf\/gE/);
  assert.match(source, /BEGIN SSH SIGNATURE/);
  assert.match(source, /verify-tag "\$tag"/);
  assert.match(
    source,
    /ls-remote --tags origin "refs\/tags\/\$tag" "refs\/tags\/\$tag\^\{\}"/,
  );
  assert.match(source, /Remote release tag object/);
  assert.match(
    source,
    /Remote release tag \$tag does not target the exact origin\/main commit/,
  );
});

test("draft staging accepts no unverified retry and validates exact local and draft bytes", () => {
  const source = script(draftScript);
  assert.match(source, /shasum -a 256 -c "\$checksums"/);
  assert.match(source, /source-commit\.txt/);
  assert.match(source, /must cover exactly the GitHub distributable artifacts/);
  assert.match(source, /--verify-tag/);
  assert.match(source, /--draft/);
  assert.match(source, /--target "\$\(git -C "\$root_dir" rev-parse HEAD\)"/);
  assert.match(source, /assets do not exactly match the expected allowlist/);
  assert.match(
    source,
    /GitHub Release body does not exactly match release-notes\.md/,
  );
  assert.match(source, /gh release download "\$tag" --repo "\$release_repo"/);
  assert.match(source, /GitHub Release asset bytes do not match local/);
  assert.match(source, /Verified exact existing GitHub Release draft/);
  assert.match(source, /Verified exact existing public GitHub Release/);
  assert.match(source, /require_exact_public_r2_resume/);
  assert.match(
    source,
    /public latest\.json is not the exact R2 resume candidate/,
  );
  assert.match(
    source,
    /Public R2 resume .* differs from the local release artifact/,
  );
  const finalProvenanceGate = source.lastIndexOf(
    'dakia_require_release_mutation_provenance "$root_dir"',
  );
  const create = source.indexOf('gh release create "$tag"');
  assert.ok(finalProvenanceGate > 0);
  assert.ok(finalProvenanceGate < create);
  assert.doesNotMatch(source, /--clobber/);
  assert.doesNotMatch(source, /gh release delete/);
});

test("GitHub release stages mirror validated optional Linux and Windows updater assets", () => {
  for (const path of [draftScript, publishScript]) {
    const source = script(path);
    assert.match(
      source,
      /add_optional_platform_assets "linux" "\$asset_dir\/linux" "Dakia_\$\{version\}_amd64\.AppImage"/,
    );
    assert.match(
      source,
      /add_optional_platform_assets "windows" "\$asset_dir\/windows" "Dakia_\$\{version\}_x64-setup\.exe"/,
    );
    assert.match(
      source,
      /SHA256SUMS\.txt must contain exactly the installer and signature checksums/,
    );
    assert.match(
      source,
      /github_asset_names\+=\("\$updater_name" "\$signature_name"\)/,
    );
    assert.match(source, /github_asset_local_path\(\)/);
    assert.match(source, /Dakia_\$\{version\}_amd64\.AppImage\.sig/);
    assert.match(source, /Dakia_\$\{version\}_x64-setup\.exe\.sig/);
  }
  assert.match(
    draftScript ? script(draftScript) : "",
    /require_exact_optional_public_r2_resume/,
  );
  assert.match(
    publishScript ? script(publishScript) : "",
    /require_public_optional_r2_candidate/,
  );
});

test("publication requires the exact public R2 candidate before making GitHub public", () => {
  const source = script(publishScript);
  const draftVerification = source.lastIndexOf("if verify_release true; then");
  const r2Gate = source.indexOf(
    "require_public_r2_candidate",
    draftVerification,
  );
  const publish = source.indexOf(
    'gh release edit "$tag" --repo "$release_repo" --draft=false --latest',
  );
  const mutationProvenance = source.lastIndexOf(
    'dakia_require_release_mutation_provenance "$root_dir"',
    publish,
  );
  assert.ok(draftVerification >= 0);
  assert.ok(r2Gate >= 0);
  assert.ok(publish > r2Gate);
  assert.ok(mutationProvenance > r2Gate);
  assert.ok(mutationProvenance < publish);
  assert.match(source, /macos\/latest\/latest\.json\?release-gate=\$tag/);
  assert.match(source, /\.version == \$version/);
  assert.match(source, /\.notes == \$notes/);
  assert.match(source, /\.platforms\["darwin-aarch64"\]\.url == \$url/);
  assert.match(
    source,
    /\.platforms\["darwin-aarch64"\]\.signature == \$signature/,
  );
  assert.match(
    source,
    /Public updater manifest is not the exact signed candidate/,
  );
  assert.match(source, /readFileSync\(process\.argv\[1\], "utf8"\)\.trim\(\)/);
  assert.doesNotMatch(source, /expected_signature="\$\(base64/);
});

test("publication independently downloads and byte-compares every public GitHub asset", () => {
  const source = script(publishScript);
  assert.match(
    source,
    /https:\/\/github\.com\/\$release_repo\/releases\/download\/\$tag\/\$artifact/,
  );
  assert.match(source, /Public GitHub asset bytes do not match local/);
  assert.match(
    source,
    /Public GitHub checksum file does not validate downloaded assets/,
  );
  assert.match(source, /verify_release false/);
  assert.match(source, /return 2/);
  assert.match(
    source,
    /if \[\[ "\$expected_draft" == "true" \]\]; then[\s\S]*?"\$actual_draft" == "true"[\s\S]*?else[\s\S]*?"\$actual_draft" == "false"/,
  );
  assert.doesNotMatch(source, /--clobber/);
  assert.doesNotMatch(source, /gh release delete/);
});

test("package scripts expose the GitHub release stages and focused tests", () => {
  assert.equal(
    packageJson.scripts["release:github:draft"],
    "./scripts/prepare-github-release-draft.sh",
  );
  assert.equal(
    packageJson.scripts["release:github:publish"],
    "./scripts/publish-github-release.sh",
  );
  assert.equal(
    packageJson.scripts["test:github-release-stage"],
    "node --test scripts/github-release-stage.node-test.mjs",
  );
});
