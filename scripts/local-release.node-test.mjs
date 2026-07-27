import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const verifier = join(root, "scripts/verify-local-updater-evidence.sh");
const publisher = join(root, "scripts/publish-release-to-r2.sh");
const releaseEnvironment = join(root, "scripts/local-release-env.sh");
const appVerifier = join(root, "scripts/verify-macos-release-app.sh");
const sha256 = (value) => createHash("sha256").update(value).digest("hex");

function createEvidence() {
  const evidenceRoot = mkdtempSync(join(tmpdir(), "dakia-evidence-test-"));
  for (const arch of ["aarch64", "x86_64"]) {
    for (const mode of ["valid", "tampered-archive", "invalid-signature"]) {
      const resultDir = join(evidenceRoot, arch, mode);
      mkdirSync(resultDir, { recursive: true });
      const profile = "accounts=1 messages=2 sha256=test-profile\n";
      const events =
        mode === "valid"
          ? [
              { event: "launched", detail: "0.2.7" },
              { event: "update-available", detail: "0.2.8" },
              { event: "downloaded", detail: "0.2.8" },
              { event: "installing", detail: "0.2.8" },
              { event: "launched", detail: "0.2.8" },
              { event: "completed", detail: "0.2.8" },
            ]
              .map(JSON.stringify)
              .join("\n") + "\n"
          : [
              { event: "launched", detail: "0.2.7" },
              { event: "signature-rejected", detail: mode },
            ]
              .map(JSON.stringify)
              .join("\n") + "\n";
      writeFileSync(join(resultDir, "evidence.jsonl"), events);
      writeFileSync(join(resultDir, "profile-before.txt"), profile);
      writeFileSync(join(resultDir, "profile-after.txt"), profile);
      writeFileSync(
        join(resultDir, "result.json"),
        JSON.stringify({
          schema: "dakia-local-updater-acceptance-v1",
          result: "passed",
          arch,
          mode,
          baseline_tag: "v0.2.7",
          target_tag: "v0.2.8",
          final_version: mode === "valid" ? "0.2.8" : "0.2.7",
          evidence_sha256: sha256(events),
          profile_sha256: sha256(profile),
        }),
      );
    }
  }
  return evidenceRoot;
}

function createStaticAppFixture() {
  const fixtureRoot = mkdtempSync(join(tmpdir(), "dakia-release-app-test-"));
  const app = join(fixtureRoot, "Dakia.app");
  const resources = join(app, "Contents", "Resources");
  const licenses = join(resources, "licenses");
  mkdirSync(join(app, "Contents", "MacOS"), { recursive: true });
  mkdirSync(join(app, "Contents", "Frameworks"), { recursive: true });
  mkdirSync(join(resources, "resources", "email-classifier-v2"), {
    recursive: true,
  });
  mkdirSync(licenses, { recursive: true });
  writeFileSync(join(app, "Contents", "MacOS", "dakia-desktop"), "fixture");
  chmodSync(join(app, "Contents", "MacOS", "dakia-desktop"), 0o755);
  writeFileSync(
    join(app, "Contents", "Frameworks", "libonnxruntime.1.23.2.dylib"),
    "fixture",
  );
  for (const resource of ["MANIFEST.json", "model.onnx", "tokenizer.json"]) {
    writeFileSync(
      join(resources, "resources", "email-classifier-v2", resource),
      "fixture",
    );
  }
  copyFileSync(
    join(root, "THIRD_PARTY_NOTICES.md"),
    join(resources, "THIRD_PARTY_NOTICES.md"),
  );
  for (const filename of [
    "Apache-2.0.txt",
    "MPL-2.0.txt",
    "DAKIA-MPL-2.0-SOURCE-NOTICE.md",
    "mmBERT-small-MIT-NOTICE.txt",
    "ONNX-Runtime-1.23.2-LICENSE.txt",
    "ONNX-Runtime-1.23.2-ThirdPartyNotices.txt",
  ]) {
    copyFileSync(
      join(
        root,
        "apps",
        "desktop",
        "src-tauri",
        "resources",
        "licenses",
        filename,
      ),
      join(licenses, filename),
    );
  }
  return { fixtureRoot, app };
}

test("accepts a complete two-architecture, three-mode evidence set", () => {
  const evidenceRoot = createEvidence();
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /6 passed, 0 waived/);
});

test("accepts an explicit rejection-test waiver without treating it as passed", () => {
  const evidenceRoot = createEvidence();
  const resultDir = join(evidenceRoot, "x86_64", "tampered-archive");
  rmSync(resultDir, { recursive: true });
  mkdirSync(resultDir, { recursive: true });
  writeFileSync(
    join(resultDir, "waiver.json"),
    JSON.stringify({
      schema: "dakia-local-updater-waiver-v1",
      result: "waived",
      arch: "x86_64",
      mode: "tampered-archive",
      target_tag: "v0.2.8",
      reason: "Release owner explicitly chose to skip this test.",
      authorized_by: "release-owner",
      authorized_at: "2026-07-26T00:00:00Z",
    }),
  );
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /5 passed, 1 waived/);
});

test("accepts an explicitly risk-acknowledged install-and-restart waiver", () => {
  const evidenceRoot = createEvidence();
  const resultDir = join(evidenceRoot, "x86_64", "valid");
  rmSync(resultDir, { recursive: true });
  mkdirSync(resultDir, { recursive: true });
  writeFileSync(
    join(resultDir, "waiver.json"),
    JSON.stringify({
      schema: "dakia-local-updater-waiver-v1",
      result: "waived",
      arch: "x86_64",
      mode: "valid",
      target_tag: "v0.2.8",
      reason: "Release owner accepted deferred Intel upgrade verification.",
      authorized_by: "release-owner",
      authorized_at: "2026-07-26T00:00:00Z",
      risk_acknowledged: true,
    }),
  );
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /5 passed, 1 waived/);
});

test("rejects an install-and-restart waiver without risk acknowledgement", () => {
  const evidenceRoot = createEvidence();
  const resultDir = join(evidenceRoot, "x86_64", "valid");
  rmSync(resultDir, { recursive: true });
  mkdirSync(resultDir, { recursive: true });
  writeFileSync(
    join(resultDir, "waiver.json"),
    JSON.stringify({
      schema: "dakia-local-updater-waiver-v1",
      result: "waived",
      arch: "x86_64",
      mode: "valid",
      target_tag: "v0.2.8",
      reason: "Missing explicit risk acknowledgement.",
      authorized_by: "release-owner",
      authorized_at: "2026-07-26T00:00:00Z",
    }),
  );
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.notEqual(result.status, 0);
});

test("rejects evidence changed after the result was recorded", () => {
  const evidenceRoot = createEvidence();
  const events = join(evidenceRoot, "x86_64", "valid", "evidence.jsonl");
  writeFileSync(
    events,
    readFileSync(events, "utf8") + '{"event":"tampered"}\n',
  );
  const result = spawnSync(verifier, ["v0.2.8", evidenceRoot], {
    encoding: "utf8",
  });
  assert.notEqual(result.status, 0);
});

test("low-level publisher refuses direct production publication", () => {
  const result = spawnSync(publisher, ["v0.2.8", tmpdir()], {
    encoding: "utf8",
    env: {
      ...process.env,
      DAKIA_UPDATER_CHANNEL: "production",
    },
  });
  assert.notEqual(result.status, 0);
  assert.match(
    result.stderr,
    /Production publication must run through publish-local-production-release/,
  );
});

test("static packaged-app verification covers legal resources without native startup", () => {
  const { fixtureRoot, app } = createStaticAppFixture();
  try {
    const result = spawnSync(appVerifier, ["--static-only", app], {
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(
      result.stdout,
      /static app\/resource\/legal verification passed/,
    );
  } finally {
    rmSync(fixtureRoot, { recursive: true, force: true });
  }
});

test("static packaged-app verification rejects a missing Dakia MPL source notice", () => {
  const { fixtureRoot, app } = createStaticAppFixture();
  try {
    rmSync(
      join(
        app,
        "Contents",
        "Resources",
        "licenses",
        "DAKIA-MPL-2.0-SOURCE-NOTICE.md",
      ),
    );
    const result = spawnSync(appVerifier, ["--static-only", app], {
      encoding: "utf8",
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /DAKIA-MPL-2.0-SOURCE-NOTICE/);
  } finally {
    rmSync(fixtureRoot, { recursive: true, force: true });
  }
});

test("release environment preserves an explicitly injected Google OAuth secret", () => {
  const result = spawnSync(
    "/bin/bash",
    [
      "-c",
      'source "$1"; dakia_keychain_read() { return 1; }; dakia_google_oauth_probe() { return 0; }; dakia_require_google_oauth_environment; test "$DAKIA_GOOGLE_CLIENT_SECRET" = injected-secret; test -z "$(env | grep ^DAKIA_GOOGLE_CLIENT_)"',
      "bash",
      releaseEnvironment,
    ],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        DAKIA_GOOGLE_CLIENT_SECRET: "injected-secret",
      },
    },
  );
  assert.equal(result.status, 0, result.stderr);
});

test("Google OAuth preflight keeps the secret out of curl arguments", () => {
  const tempRoot = mkdtempSync(join(tmpdir(), "dakia-google-oauth-curl-test-"));
  const curlPath = join(tempRoot, "curl");
  const capturedArgs = join(tempRoot, "curl-args.txt");
  writeFileSync(
    curlPath,
    '#!/bin/sh\nprintf "%s\\n" "$@" > "$DAKIA_TEST_CURL_ARGS"\nprintf \'{"error":"invalid_grant"}\'\n',
  );
  chmodSync(curlPath, 0o755);
  try {
    const result = spawnSync(
      "/bin/bash",
      [
        "-c",
        'source "$1"; dakia_load_google_oauth_environment; dakia_google_oauth_probe test-client injected-secret; test -z "$(env | grep ^DAKIA_GOOGLE_CLIENT_)"',
        "bash",
        releaseEnvironment,
      ],
      {
        encoding: "utf8",
        env: {
          ...process.env,
          PATH: `${tempRoot}:${process.env.PATH}`,
          DAKIA_GOOGLE_CLIENT_ID: "test-client",
          DAKIA_GOOGLE_CLIENT_SECRET: "injected-secret",
          DAKIA_TEST_CURL_ARGS: capturedArgs,
        },
      },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.doesNotMatch(readFileSync(capturedArgs, "utf8"), /injected-secret/);
  } finally {
    rmSync(tempRoot, { recursive: true, force: true });
  }
});

test("Google OAuth secret reaches only the Dakia Rust compiler invocation", () => {
  const tempRoot = mkdtempSync(
    join(tmpdir(), "dakia-google-oauth-rustc-test-"),
  );
  const rustcPath = join(tempRoot, "rustc");
  const capturedEnvironment = join(tempRoot, "rustc-environment.txt");
  writeFileSync(
    rustcPath,
    '#!/bin/sh\nprintf "%s|%s" "${DAKIA_GOOGLE_CLIENT_ID:-}" "${DAKIA_GOOGLE_CLIENT_SECRET:-}" > "$DAKIA_TEST_RUSTC_ENV"\n',
  );
  chmodSync(rustcPath, 0o755);
  try {
    const result = spawnSync(
      "/bin/bash",
      [
        "-c",
        'source "$1"; dakia_load_google_oauth_environment; dakia_prepare_google_oauth_compiler_environment; test -z "$(env | grep ^DAKIA_GOOGLE_CLIENT_)"; "$RUSTC_WRAPPER" "$2" --crate-type bin --crate-name dakia_desktop; dakia_clear_google_oauth_compiler_environment',
        "bash",
        releaseEnvironment,
        rustcPath,
      ],
      {
        encoding: "utf8",
        env: {
          ...process.env,
          DAKIA_GOOGLE_CLIENT_ID: "test-client",
          DAKIA_GOOGLE_CLIENT_SECRET: "injected-secret",
          DAKIA_TEST_RUSTC_ENV: capturedEnvironment,
        },
      },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.equal(
      readFileSync(capturedEnvironment, "utf8"),
      "test-client|injected-secret",
    );
  } finally {
    rmSync(tempRoot, { recursive: true, force: true });
  }
});

test("release environment loads the Google OAuth secret from Keychain", () => {
  const result = spawnSync(
    "/bin/bash",
    [
      "-c",
      'source "$1"; dakia_keychain_read() { test "$1" = dev.dakia.mail.google-oauth; test "$2" = client-secret; printf keychain-secret; }; dakia_google_oauth_probe() { test "$1" = 77400090557-np3jvrl1d13oec7i9evs0i9c89u7q3hg.apps.googleusercontent.com; test "$2" = keychain-secret; }; unset DAKIA_GOOGLE_CLIENT_SECRET; dakia_require_google_oauth_environment; test "$DAKIA_GOOGLE_CLIENT_SECRET" = keychain-secret',
      "bash",
      releaseEnvironment,
    ],
    { encoding: "utf8", env: { ...process.env } },
  );
  assert.equal(result.status, 0, result.stderr);
});

test("release environment rejects a missing Google OAuth secret", () => {
  const result = spawnSync(
    "/bin/bash",
    [
      "-c",
      'source "$1"; dakia_keychain_read() { return 1; }; dakia_google_oauth_probe() { return 0; }; unset DAKIA_GOOGLE_CLIENT_SECRET; dakia_require_google_oauth_environment',
      "bash",
      releaseEnvironment,
    ],
    { encoding: "utf8", env: { ...process.env } },
  );
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /Missing Google OAuth client secret/);
});

test("release environment rejects whitespace-only Google OAuth material", () => {
  const result = spawnSync(
    "/bin/bash",
    [
      "-c",
      'source "$1"; dakia_keychain_read() { return 1; }; dakia_google_oauth_probe() { return 0; }; dakia_require_google_oauth_environment',
      "bash",
      releaseEnvironment,
    ],
    {
      encoding: "utf8",
      env: { ...process.env, DAKIA_GOOGLE_CLIENT_SECRET: "   " },
    },
  );
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /Missing Google OAuth client secret/);
});

test("release environment rejects a mismatched Google OAuth client pairing", () => {
  const result = spawnSync(
    "/bin/bash",
    [
      "-c",
      'source "$1"; dakia_keychain_read() { return 1; }; dakia_google_oauth_probe() { return 1; }; dakia_require_google_oauth_environment',
      "bash",
      releaseEnvironment,
    ],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        DAKIA_GOOGLE_CLIENT_SECRET: "stale-secret",
      },
    },
  );
  assert.notEqual(result.status, 0);
  assert.match(
    result.stderr,
    /Google rejected the configured OAuth client ID and secret pairing/,
  );
});

test("release environment canonicalizes and unexports an empty client ID override", () => {
  const result = spawnSync(
    "/bin/bash",
    [
      "-c",
      'source "$1"; dakia_keychain_read() { return 1; }; dakia_google_oauth_probe() { test "$1" = 77400090557-np3jvrl1d13oec7i9evs0i9c89u7q3hg.apps.googleusercontent.com; }; dakia_require_google_oauth_environment; test "$DAKIA_GOOGLE_CLIENT_ID" = 77400090557-np3jvrl1d13oec7i9evs0i9c89u7q3hg.apps.googleusercontent.com; test -z "$(env | grep ^DAKIA_GOOGLE_CLIENT_)"',
      "bash",
      releaseEnvironment,
    ],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        DAKIA_GOOGLE_CLIENT_ID: "",
        DAKIA_GOOGLE_CLIENT_SECRET: "injected-secret",
      },
    },
  );
  assert.equal(result.status, 0, result.stderr);
});
