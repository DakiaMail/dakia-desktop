import assert from "node:assert/strict";
import {
  chmodSync,
  copyFileSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  symlinkSync,
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
const cliBundler = join(root, "scripts/bundle-cli.sh");
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
  writeFileSync(join(app, "Contents", "MacOS", "dakia"), "fixture");
  chmodSync(join(app, "Contents", "MacOS", "dakia"), 0o755);
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

function writeExecutable(path, source) {
  writeFileSync(path, source);
  chmodSync(path, 0o755);
}

function createCliContractFixture({
  invalidInputSucceeds = false,
  missingFrameworkRpath = false,
  missingOauthMarker = false,
} = {}) {
  const fixture = createStaticAppFixture();
  const { app } = fixture;
  const macOS = join(app, "Contents", "MacOS");
  const mockBin = join(fixture.fixtureRoot, "mock-bin");
  mkdirSync(mockBin);
  writeFileSync(
    join(app, "Contents", "Info.plist"),
    `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>0.0.0</string></dict></plist>
`,
  );
  writeExecutable(
    join(macOS, "dakia-desktop"),
    `#!/bin/sh
set -eu
test "\${DAKIA_RELEASE_SMOKE_TEST:-}" = 1
test -n "\${DAKIA_RELEASE_SMOKE_DATA_DIR:-}"
test -z "\${DAKIA_GOOGLE_CLIENT_ID:-}"
test -z "\${DAKIA_GOOGLE_CLIENT_SECRET:-}"
if [ -n "\${DAKIA_TEST_LAUNCH_PATH_FILE:-}" ]; then
  printf '%s' "$0" > "\$DAKIA_TEST_LAUNCH_PATH_FILE"
fi
${missingOauthMarker ? "" : "printf '%s\\\\n' DAKIA_RELEASE_GOOGLE_OAUTH_CONFIG_OK"}
printf '%s\\n' DAKIA_RELEASE_SMOKE_TEST_OK
`,
  );
  writeExecutable(
    join(macOS, "dakia"),
    `#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
  case "$1" in
    --data-dir) shift 2 ;;
    --json) shift ;;
    *) break ;;
  esac
done
case "\${1:-}" in
  --version) printf 'dakia 0.0.0\\n' ;;
  --help)
    printf '%s\\n' 'Search, read, and send mail from the terminal' 'Usage: dakia [OPTIONS] <COMMAND>'
    ;;
  not-a-command)
    ${invalidInputSucceeds ? "exit 0" : 'printf "%s\\n" "error: unrecognized subcommand \'not-a-command\'" "" "For more information, try \'--help\'." >&2; exit 2'}
    ;;
  account)
    test "\${2:-}" = list
    printf '[]\\n'
    ;;
  *) exit 64 ;;
esac
`,
  );
  writeExecutable(join(mockBin, "lipo"), "#!/bin/sh\nprintf '%s\\n' arm64\n");
  writeExecutable(
    join(mockBin, "codesign"),
    `#!/bin/sh
case "$1" in
  --verify) exit 0 ;;
  -dv) printf '%s\\n' TeamIdentifier=fixture-team >&2 ;;
  *) exit 64 ;;
esac
`,
  );
  writeExecutable(
    join(mockBin, "otool"),
    missingFrameworkRpath
      ? "#!/bin/sh\nexit 0\n"
      : `#!/bin/sh
cat <<'OUTPUT'
Load command 1
          cmd LC_RPATH
      cmdsize 48
         path @executable_path/../Frameworks (offset 12)
OUTPUT
`,
  );
  return { ...fixture, mockBin };
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

test("release builder invalidates cached desktop credentials before Tauri compilation", () => {
  const script = readFileSync(releaseBuilder, "utf8");
  const cliBundle = script.indexOf("npm run bundle:cli");
  const desktopClean = script.indexOf(
    "cargo clean -p dakia-desktop --target aarch64-apple-darwin",
  );
  const tauriBuild = script.indexOf('"$root_dir/node_modules/.bin/tauri" build');
  assert.ok(cliBundle >= 0);
  assert.ok(desktopClean > cliBundle);
  assert.ok(tauriBuild > desktopClean);
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
  assert.match(script, /verify-updater-signature\.mjs/);
  assert.match(script, /tar -xzf "\$apple_update"/);
  assert.match(
    script,
    /verify-macos-release-app\.sh" "\$updater_app"/,
  );
  assert.match(script, /Updater app version.*does not match/);
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

test("static packaged-app verification rejects a missing CLI sidecar", () => {
  const { fixtureRoot, app } = createStaticAppFixture();
  try {
    rmSync(join(app, "Contents", "MacOS", "dakia"));
    const result = spawnSync(appVerifier, ["--static-only", app], {
      encoding: "utf8",
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /Contents\/MacOS\/dakia/);
  } finally {
    rmSync(fixtureRoot, { recursive: true, force: true });
  }
});

test("static packaged-app verification rejects a symlinked CLI sidecar", () => {
  const { fixtureRoot, app } = createStaticAppFixture();
  try {
    const sidecar = join(app, "Contents", "MacOS", "dakia");
    rmSync(sidecar);
    symlinkSync("dakia-desktop", sidecar);
    const result = spawnSync(appVerifier, ["--static-only", app], {
      encoding: "utf8",
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /unsafe packaged Dakia executable/);
  } finally {
    rmSync(fixtureRoot, { recursive: true, force: true });
  }
});

test("packaged-app verification declares an isolated CLI contract smoke", () => {
  const verifier = readFileSync(appVerifier, "utf8");
  assert.match(verifier, /lipo -archs "\$cli"/);
  assert.match(verifier, /@executable_path\/\.\.\/Frameworks/);
  assert.match(verifier, /codesign --verify --strict --verbose=2 "\$cli"/);
  assert.match(verifier, /TeamIdentifier/);
  assert.match(verifier, /"\$cli" --data-dir "\$cli_contract_data" --version/);
  assert.match(verifier, /"\$cli" --data-dir "\$cli_contract_data" --help/);
  assert.match(
    verifier,
    /"\$cli" --data-dir "\$cli_contract_data" not-a-command/,
  );
  assert.match(verifier, /cli_invalid_status" -ne 2/);
  assert.match(
    verifier,
    /parse-only commands unexpectedly created profile state/,
  );
  assert.match(
    verifier,
    /"\$cli" --data-dir "\$smoke_root\/cli-data" --json account list/,
  );
  assert.match(verifier, /CFBundleShortVersionString/);
  assert.match(verifier, /JSON\.parse/);
});

test("macOS CLI bundling injects the packaged framework lookup path", () => {
  const bundler = readFileSync(cliBundler, "utf8");
  assert.match(
    bundler,
    /install_name_tool -add_rpath "@executable_path\/\.\.\/Frameworks"/,
  );
});

test(
  "packaged-app verification executes and rejects bundled CLI parse-contract drift",
  { skip: process.platform !== "darwin" },
  () => {
    const goodFixture = createCliContractFixture();
    const badFixture = createCliContractFixture({ invalidInputSucceeds: true });
    const environmentFor = (mockBin) => ({
      PATH: `${mockBin}:${process.env.PATH}`,
      DAKIA_GOOGLE_CLIENT_ID: "runtime-id-must-not-reach-release-smoke",
      DAKIA_GOOGLE_CLIENT_SECRET: "runtime-secret-must-not-reach-release-smoke",
    });
    try {
      const goodResult = spawnSync(appVerifier, [goodFixture.app], {
        encoding: "utf8",
        env: environmentFor(goodFixture.mockBin),
      });
      assert.equal(goodResult.status, 0, goodResult.stderr);
      assert.match(goodResult.stdout, /startup smoke test passed/);
      assert.doesNotMatch(goodResult.stdout, /runtime-secret-must-not-reach/);
      assert.doesNotMatch(goodResult.stderr, /runtime-secret-must-not-reach/);

      const badResult = spawnSync(appVerifier, [badFixture.app], {
        encoding: "utf8",
        env: environmentFor(badFixture.mockBin),
      });
      assert.notEqual(badResult.status, 0);
      assert.match(
        badResult.stderr,
        /invalid-input contract was not rejected as expected/,
      );
    } finally {
      rmSync(goodFixture.fixtureRoot, { recursive: true, force: true });
      rmSync(badFixture.fixtureRoot, { recursive: true, force: true });
    }
  },
);

test(
  "packaged-app verification rejects a CLI without the framework rpath",
  { skip: process.platform !== "darwin" },
  () => {
    const fixture = createCliContractFixture({ missingFrameworkRpath: true });
    try {
      const result = spawnSync(appVerifier, [fixture.app], {
        encoding: "utf8",
        env: { PATH: `${fixture.mockBin}:${process.env.PATH}` },
      });
      assert.notEqual(result.status, 0);
      assert.match(
        result.stderr,
        /cannot resolve the bundled ONNX Runtime framework/,
      );
    } finally {
      rmSync(fixture.fixtureRoot, { recursive: true, force: true });
    }
  },
);

test(
  "packaged-app verification rejects a missing compiled Google OAuth marker",
  { skip: process.platform !== "darwin" },
  () => {
    const fixture = createCliContractFixture({ missingOauthMarker: true });
    try {
      const result = spawnSync(appVerifier, [fixture.app], {
        encoding: "utf8",
        env: { PATH: `${fixture.mockBin}:${process.env.PATH}` },
      });
      assert.notEqual(result.status, 0);
      assert.match(
        result.stderr,
        /missing its compiled Google OAuth configuration/,
      );
    } finally {
      rmSync(fixture.fixtureRoot, { recursive: true, force: true });
    }
  },
);

test(
  "packaged-app verification canonicalizes symlinked temporary launch paths",
  { skip: process.platform !== "darwin" },
  () => {
    const fixture = createCliContractFixture();
    const aliasRoot = mkdtempSync(join(tmpdir(), "dakia-release-alias-"));
    const alias = join(aliasRoot, "bundle");
    const launchPath = join(aliasRoot, "launch-path.txt");
    symlinkSync(fixture.fixtureRoot, alias);
    try {
      const result = spawnSync(appVerifier, [join(alias, "Dakia.app")], {
        encoding: "utf8",
        env: {
          PATH: `${fixture.mockBin}:${process.env.PATH}`,
          DAKIA_TEST_LAUNCH_PATH_FILE: launchPath,
        },
      });
      assert.equal(result.status, 0, result.stderr);
      assert.equal(
        readFileSync(launchPath, "utf8"),
        join(realpathSync(fixture.app), "Contents", "MacOS", "dakia-desktop"),
      );
    } finally {
      rmSync(aliasRoot, { recursive: true, force: true });
      rmSync(fixture.fixtureRoot, { recursive: true, force: true });
    }
  },
);

test("updater packaging executes the extracted signed app and CLI verifier", () => {
  const packager = readFileSync(
    join(root, "scripts", "package-macos-updater.sh"),
    "utf8",
  );
  assert.match(
    packager,
    /verify-macos-release-app\.sh" "\$verify_dir\/Dakia\.app"/,
  );
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

test("Google OAuth secret reaches the Rust library crate that implements OAuth", () => {
  const tempRoot = mkdtempSync(
    join(tmpdir(), "dakia-google-oauth-rustc-test-"),
  );
  const rustcPath = join(tempRoot, "rustc");
  const capturedEnvironment = join(tempRoot, "rustc-environment.txt");
  writeFileSync(
    rustcPath,
    '#!/bin/sh\nprintf "%s|%s\\n" "${DAKIA_GOOGLE_CLIENT_ID:-}" "${DAKIA_GOOGLE_CLIENT_SECRET:-}" >> "$DAKIA_TEST_RUSTC_ENV"\n',
  );
  chmodSync(rustcPath, 0o755);
  try {
    const result = spawnSync(
      "/bin/bash",
      [
        "-c",
        'source "$1"; dakia_load_google_oauth_environment; dakia_prepare_google_oauth_compiler_environment; test -z "$(env | grep ^DAKIA_GOOGLE_CLIENT_)"; "$RUSTC_WRAPPER" "$2" --crate-type rlib --crate-name dakia_desktop_lib; "$RUSTC_WRAPPER" "$2" --crate-type bin --crate-name dakia_desktop; "$RUSTC_WRAPPER" "$2" --crate-type rlib --crate-name third_party_dependency; dakia_clear_google_oauth_compiler_environment',
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
      "test-client|injected-secret\n|\n|\n",
    );
    assert.doesNotMatch(result.stdout, /injected-secret/);
    assert.doesNotMatch(result.stderr, /injected-secret/);
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
