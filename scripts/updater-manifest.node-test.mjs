import assert from "node:assert/strict";
import test from "node:test";
import {
  buildManifest,
  corruptManifestSignatures,
  validateManifest,
} from "./updater-manifest.mjs";

const encodedSignature = Buffer.from(
  [
    "untrusted comment: signature from minisign secret key",
    "RWQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "trusted comment: timestamp:1753444800\tfile:Dakia.app.tar.gz",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==",
  ].join("\n"),
).toString("base64");

const input = {
  version: "0.2.7",
  pubDate: "2026-07-25T12:00:00Z",
  notes: "Safer updates",
  aarch64Url:
    "https://downloads.dakiamail.com/macos/v0.2.7/Dakia-aarch64.app.tar.gz",
  aarch64Signature: encodedSignature,
  x86_64Url:
    "https://downloads.dakiamail.com/macos/v0.2.7/Dakia-x86_64.app.tar.gz",
  x86_64Signature: encodedSignature,
};

test("builds the two required macOS platform entries", () => {
  const manifest = buildManifest(input);
  assert.equal(
    manifest.platforms["darwin-aarch64"].signature,
    encodedSignature,
  );
  assert.equal(manifest.platforms["darwin-x86_64"].url, input.x86_64Url);
});

test("rejects architecture swaps", () => {
  const manifest = buildManifest(input);
  manifest.platforms["darwin-aarch64"].url = input.x86_64Url;
  assert.throws(() => validateManifest(manifest), /architecture mismatch/);
});

test("rejects missing and empty signatures", () => {
  assert.throws(
    () => buildManifest({ ...input, aarch64Signature: " " }),
    /Invalid updater signature/,
  );
  assert.throws(
    () => buildManifest({ ...input, aarch64Signature: "not-a-signature" }),
    /Invalid updater signature/,
  );
});

test("rejects non-SemVer versions", () => {
  assert.throws(
    () => buildManifest({ ...input, version: "release-seven" }),
    /Invalid updater SemVer/,
  );
  assert.throws(
    () => buildManifest({ ...input, version: "1.0.0-01" }),
    /Invalid updater SemVer/,
  );
});

test("creates structurally valid but cryptographically different signatures", () => {
  const manifest = buildManifest(input);
  const corrupted = corruptManifestSignatures(manifest);

  assert.notEqual(
    corrupted.platforms["darwin-aarch64"].signature,
    manifest.platforms["darwin-aarch64"].signature,
  );
  assert.notEqual(
    corrupted.platforms["darwin-x86_64"].signature,
    manifest.platforms["darwin-x86_64"].signature,
  );
  assert.doesNotThrow(() => validateManifest(corrupted));
});
