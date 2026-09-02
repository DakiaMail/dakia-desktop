import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const publisher = join(root, "scripts", "publish-release-to-r2.sh");

function source() {
  return readFileSync(publisher, "utf8");
}

function position(script, fragment) {
  const index = script.indexOf(fragment);
  assert.notEqual(index, -1, `missing ${fragment}`);
  return index;
}

test("R2 publisher remains valid Bash", () => {
  const result = spawnSync("bash", ["-n", publisher], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
});

test("non-macOS publication requires an exact macOS builder verification record", () => {
  const script = source();
  assert.match(script, /DAKIA_MACOS_CI_VERIFICATION/);
  assert.match(
    script,
    /record\.verification !== "scripts\/build-local-macos-release\.sh completed"/,
  );
  assert.match(script, /record\.tag !== tag/);
  assert.match(script, /record\.source_commit !== sourceCommit/);
  assert.match(script, /record\.checksums_sha256 !== checksumsSha256/);
});

test("R2 publisher fails closed on tracked notes and exact checksums", () => {
  const script = source();

  assert.match(
    script,
    /git -C "\$root_dir" ls-files --error-unmatch -- "docs\/releases\/\$tag\.md"/,
  );
  assert.match(script, /cmp -s "\$release_notes_source" "\$notes_file"/);
  assert.match(
    script,
    /Release notes asset does not exactly match tracked notes/,
  );
  assert.match(
    script,
    /shasum -a 256 "Dakia_\$\{version\}_aarch64\.dmg" "Dakia-aarch64\.app\.tar\.gz" "Dakia-aarch64\.app\.tar\.gz\.sig"/,
  );
  assert.match(script, /cmp -s "\$expected_checksums" "\$checksums_file"/);
  assert.match(script, /SHA256SUMS\.txt must exactly verify/);
  assert.match(script, /source-commit\.txt does not bind these artifacts/);
  assert.doesNotMatch(script, /printf 'Dakia %s\\n'/);
});

test("R2 publisher proves the signed tag is the exact remote main source", () => {
  const script = source();

  assert.match(script, /git -C "\$root_dir" status --porcelain/);
  assert.match(script, /branch --show-current/);
  assert.match(script, /dakia_require_live_main_provenance "\$root_dir"/);
  assert.match(
    script,
    /dakia_require_release_mutation_provenance "\$root_dir"/,
  );
  assert.match(script, /dakia_require_expected_release_origin "\$root_dir"/);
  assert.match(script, /local-release-env\.sh/);
  assert.match(script, /refs\/tags\/\$tag\^\{commit\}/);
  assert.match(script, /git -C "\$root_dir" verify-tag "\$tag"/);
  assert.match(script, /git -C "\$root_dir" ls-remote --tags origin/);
  assert.match(
    script,
    /Remote source tag \$tag must point exactly at origin\/main/,
  );
});

test("R2 publisher rejects a rollback and permits only an exact manifest resume", () => {
  const script = source();

  assert.match(
    script,
    /node "\$root_dir\/scripts\/updater-manifest\.mjs" validate --manifest "\$public_manifest"/,
  );
  assert.match(
    script,
    /relation="\$\(version_relation "\$version" "\$existing_version"\)"/,
  );
  assert.match(
    script,
    /0\) verify_current_manifest_is_candidate "\$public_manifest"; resuming=true/,
  );
  assert.match(
    script,
    /-1\) echo "Refusing to publish \$tag: public updater version \$existing_version is newer\./,
  );
  assert.match(script, /entry\?\.url !== url/);
  assert.match(script, /entry\?\.signature\?\.trim\(\) !== readFileSync/);
  assert.match(script, /manifest\.notes !== readFileSync/);
  assert.match(script, /if \[\[ "\$resuming" == true \]\]; then[\s\S]*?exit 0/);
  assert.match(
    script,
    /Verified exact R2 resume for \$tag; latest\.json and stable DMG are current/,
  );
});

test("R2 publisher keeps latest last and repairs a CAS loser's stable alias to its winner", () => {
  const script = source();

  assert.match(
    script,
    /--proto '=https' --connect-timeout 15 --max-time 90 --retry 3 --retry-delay 1 --retry-max-time 180/,
  );
  assert.match(
    script,
    /\[\[ "\$status" == "200" \]\] && cmp -s "\$source" "\$downloaded"/,
  );
  assert.match(
    script,
    /verify_public_copy "\$apple_update_key" "\$apple_update"/,
  );
  assert.match(
    script,
    /verify_public_copy "\$apple_signature_key" "\$apple_signature"/,
  );
  assert.match(script, /verify_public_copy "\$apple_dmg_key" "\$apple_dmg"/);
  assert.match(
    script,
    /upload_immutable "\$apple_checksums_key" "\$checksums_file"/,
  );
  assert.match(
    script,
    /verify_public_copy "\$apple_checksums_key" "\$checksums_file"/,
  );
  assert.match(script, /verify_public_copy "\$stable_dmg_key" "\$apple_dmg"/);
  assert.match(script, /for attempt in 1 2 3 4 5/);
  assert.match(script, /cmp -s "\$authoritative" "\$public_copy"/);

  const immutableDmg = position(script, 'upload_immutable "$apple_dmg_key"');
  const immutableUpdate = position(
    script,
    'upload_immutable "$apple_update_key"',
  );
  const immutableSignature = position(
    script,
    'upload_immutable "$apple_signature_key"',
  );
  const stableGate = position(
    script,
    "# Every artifact gate, including public stable-DMG byte verification",
  );
  const stable = script.indexOf(
    'if ! public_copy_matches "$stable_dmg_key"',
    stableGate,
  );
  const stableVerify = script.indexOf(
    'verify_public_copy "$stable_dmg_key" "$apple_dmg"',
    stableGate,
  );
  assert.notEqual(stable, -1);
  assert.notEqual(stableVerify, -1);
  const latest = position(
    script,
    'aws s3api put-object --bucket "$bucket" --key "$manifest_key" --body "$manifest"',
  );
  const githubDraft = position(
    script,
    '"$root_dir/scripts/prepare-github-release-draft.sh" "$tag" "$asset_dir"',
  );
  const eligibility = position(script, 'case "$relation" in');
  assert.ok(eligibility < githubDraft);
  assert.ok(githubDraft < immutableDmg);
  assert.match(
    script,
    /upload\(\)[\s\S]*?dakia_require_release_mutation_provenance "\$root_dir"/,
  );
  assert.match(
    script,
    /upload_immutable\(\)[\s\S]*?dakia_require_release_mutation_provenance "\$root_dir"/,
  );
  assert.match(
    script,
    /claim_publication_state\(\)[\s\S]*?dakia_require_release_mutation_provenance "\$root_dir"/,
  );
  assert.ok(
    script.lastIndexOf(
      'dakia_require_release_mutation_provenance "$root_dir"',
      latest,
    ) < latest,
    "latest.json CAS must have its own final live-main gate",
  );
  const publicationClaim = script.lastIndexOf("\nclaim_publication_state\n");
  assert.notEqual(publicationClaim, -1);
  assert.ok(immutableSignature < publicationClaim);
  assert.ok(publicationClaim < stable);
  assert.ok(immutableDmg < stable);
  assert.ok(immutableUpdate < stable);
  assert.ok(immutableSignature < stable);
  assert.ok(stable < latest);
  assert.ok(stable < stableVerify);
  assert.equal(
    script.lastIndexOf(
      'aws s3api put-object --bucket "$bucket" --key "$manifest_key" --body "$manifest"',
    ),
    latest,
  );
  assert.match(script, /--if-match "\$manifest_etag"/);
  assert.match(
    script,
    /publication_state_key="macos\/latest\/publication\.json"/,
  );
  assert.match(
    script,
    /Another incomplete release owns the mutable publication state/,
  );
  assert.match(script, /winner_dmg_key\(\)/);
  assert.match(script, /repair_stable_alias_to_manifest\(\)/);
  assert.match(script, /get_authenticated_manifest\(\)/);
  assert.match(
    script,
    /aws s3api get-object --bucket "\$bucket" --key "\$manifest_key"/,
  );
  assert.match(script, /public_manifest_converges_to\(\)/);
  assert.match(script, /verify_public_copy "\$winner_key" "\$winner_dmg"/);
  assert.match(script, /verify_public_copy "\$stable_dmg_key" "\$winner_dmg"/);
  assert.match(
    script,
    /get_authenticated_manifest "\$authoritative_winner_manifest"/,
  );
  assert.match(script, /validate --manifest "\$authoritative_winner_manifest"/);
  assert.match(
    script,
    /public_manifest_converges_to "\$authoritative_winner_manifest" "\$published_manifest"/,
  );
  assert.match(
    script,
    /verify_current_manifest_is_candidate "\$authoritative_winner_manifest"/,
  );
  assert.match(
    script,
    /repair_stable_alias_to_manifest "\$authoritative_winner_manifest"/,
  );
  assert.match(
    script,
    /get_authenticated_manifest "\$after_repair_authenticated"/,
  );
  assert.match(script, /validate --manifest "\$after_repair_authenticated"/);
  assert.match(
    script,
    /public_manifest_converges_to "\$after_repair_authenticated" "\$after_repair_public"/,
  );
  assert.match(
    script,
    /cmp -s "\$winner_manifest" "\$after_repair_authenticated"/,
  );
  assert.match(
    script,
    /Updater manifest changed while repairing the stable DMG alias/,
  );
  assert.match(
    script,
    /Another release won latest\.json; repaired its stable DMG alias and stopped/,
  );
  assert.match(script, /This is the final normal-path R2 mutation/);
  assert.doesNotMatch(script.slice(latest), /upload "\$stable_dmg_key"/);
});

test("R2 publisher gates optional Linux and Windows feeds behind immutable artifacts", () => {
  const script = source();

  assert.match(script, /\[\[ -d "\$asset_dir\/linux" \]\]/);
  assert.match(script, /\[\[ -d "\$asset_dir\/windows" \]\]/);
  assert.match(script, /Dakia_\$\{version\}_amd64\.AppImage/);
  assert.match(script, /Dakia_\$\{version\}_x64-setup\.exe/);
  assert.match(script, /--platform "\$platform"/);
  assert.match(script, /--if-none-match "\*"/);
  assert.match(script, /--if-match "\$current_etag"/);

  const optionalFunction = position(script, "publish_optional_platform() {");
  const immutableInstaller = script.indexOf(
    'upload_immutable "$installer_key"',
    optionalFunction,
  );
  const verifySignature = script.indexOf(
    'verify_public_copy "$signature_key"',
    optionalFunction,
  );
  const uploadChecksums = script.indexOf(
    'upload_immutable "$checksums_key" "$platform_dir/SHA256SUMS.txt"',
    optionalFunction,
  );
  const verifyChecksums = script.indexOf(
    'verify_public_copy "$checksums_key" "$platform_dir/SHA256SUMS.txt"',
    optionalFunction,
  );
  const createManifest = script.indexOf(
    'updater-manifest.mjs" create --platform "$platform"',
    optionalFunction,
  );
  const writeManifest = script.indexOf(
    'aws s3api put-object --bucket "$bucket" --key "$platform_manifest_key" --body "$candidate_manifest"',
    optionalFunction,
  );
  assert.ok(immutableInstaller < uploadChecksums);
  assert.ok(uploadChecksums < verifySignature);
  assert.ok(verifySignature < verifyChecksums);
  assert.ok(verifyChecksums < createManifest);
  assert.ok(createManifest < writeManifest);
});
