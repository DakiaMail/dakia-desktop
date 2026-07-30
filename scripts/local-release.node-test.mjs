import assert from "node:assert/strict";
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
const publisher = join(root, "scripts/publish-release-to-r2.sh");
const releaseEnvironment = join(root, "scripts/local-release-env.sh");
const appVerifier = join(root, "scripts/verify-macos-release-app.sh");
const releaseBuilder = join(root, "scripts/build-local-macos-release.sh");
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

test("local installer builds only an app without updater artifacts", () => {
  const packageJson = JSON.parse(
    readFileSync(join(root, "package.json"), "utf8"),
  );
  const installConfig = JSON.parse(
    readFileSync(
      join(root, "apps", "desktop", "src-tauri", "tauri.install.conf.json"),
      "utf8",
    ),
  );
  const installer = readFileSync(
    join(root, "scripts", "install-built-app.sh"),
    "utf8",
  );

  assert.ok(packageJson.scripts["build:install:bundle"]);
  assert.match(packageJson.scripts["build:install:bundle"], /--bundles app/);
  assert.equal(installConfig.bundle.createUpdaterArtifacts, false);
  assert.match(installer, /npm run build:install:bundle/);
});

test("release builder requires tracked human-readable release notes", () => {
  const script = readFileSync(releaseBuilder, "utf8");
  assert.match(script, /docs\/releases\/\$tag\.md/);
  assert.match(script, /git -C "\$root_dir" ls-files --error-unmatch/);
  assert.match(script, /Missing tracked release notes/);
  assert.match(
    script,
    /cp "\$release_notes_source" "\$output_dir\/release-notes\.md"/,
  );
  assert.doesNotMatch(script, /printf 'Dakia %s\\n'/);
});

test("publisher rejects incomplete release assets before requiring publication credentials", () => {
  const result = spawnSync(publisher, ["v0.2.8", tmpdir()], {
    encoding: "utf8",
    env: { PATH: "/usr/bin:/bin" },
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /AWS CLI is required|R2_ACCESS_KEY_ID/);
});

test("publisher resumes only when immutable public bytes match", () => {
  const script = readFileSync(publisher, "utf8");
  assert.match(script, /aws s3api get-object/);
  assert.match(script, /cmp -s "\$source" "\$existing"/);
  assert.match(script, /aws s3api put-object/);
  assert.match(script, /--if-none-match "\*"/);
  assert.match(
    script,
    /Refusing to replace immutable object with different or unreadable bytes/,
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
