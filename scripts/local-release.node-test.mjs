import assert from "node:assert/strict";
import {
  chmodSync,
  copyFileSync,
  existsSync,
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
const onnxRuntimeThinner = join(root, "scripts/thin-macos-onnx-runtime.sh");
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
  appArchitecture = "arm64",
  wrongTeam = false,
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
if [ -n "\${DAKIA_TEST_LAUNCH_PATH_FILE:-}" ]; then
  printf '%s' "$0" > "\$DAKIA_TEST_LAUNCH_PATH_FILE"
fi
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
  writeExecutable(
    join(mockBin, "lipo"),
    `#!/bin/sh
case "$2" in
  *dakia-desktop) printf '%s\\n' '${appArchitecture}' ;;
  *) printf '%s\\n' arm64 ;;
esac
`,
  );
  writeExecutable(
    join(mockBin, "codesign"),
    `#!/bin/sh
case "$1" in
  --verify) exit 0 ;;
  -dv) printf '%s\\n' TeamIdentifier=${wrongTeam ? "unexpected-team" : "34T9L3FGZC"} >&2 ;;
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

test("release builder requires exact clean main provenance and all version authorities", () => {
  const script = readFileSync(releaseBuilder, "utf8");
  assert.match(script, /package-lock\.json/);
  assert.match(script, /lock\.version === expected/);
  assert.match(script, /lock\.packages\?\.\[""\]\?\.version === expected/);
  for (const packageName of ["dakia-cli", "dakia-core", "dakia-desktop"]) {
    assert.match(script, new RegExp(`${packageName}=\\$version`));
  }
  assert.match(script, /branch --show-current/);
  assert.match(script, /dakia_require_live_main_provenance "\$root_dir"/);
  assert.match(script, /dakia_require_expected_release_origin "\$root_dir"/);
  assert.match(script, /status --porcelain=v1 --untracked-files=all/);
  assert.match(script, /source_commit=/);
  assert.match(script, /source-commit\.txt/);
  assert.match(
    script,
    /Release source changed while artifacts were being built/,
  );
  assert.ok(
    script.lastIndexOf('dakia_require_expected_release_origin "$root_dir"') >
      script.indexOf("npm run setup:worktree"),
    "builder must recheck its origin after the long build path",
  );
});

test("release builder invokes the normal Tauri build without credential-specific overrides", () => {
  const script = readFileSync(releaseBuilder, "utf8");
  const cliBundle = script.indexOf("npm run bundle:cli");
  const tauriBuild = script.indexOf(
    '"$root_dir/node_modules/.bin/tauri" build',
  );
  assert.ok(cliBundle >= 0);
  assert.ok(tauriBuild > cliBundle);
  assert.doesNotMatch(script, /cargo clean -p dakia-desktop/);
  assert.doesNotMatch(script, /release-tauri-config/);
});

test("release builder thins only the packaged ONNX dylib before final signing", () => {
  const script = readFileSync(releaseBuilder, "utf8");
  const tauriBuild = script.indexOf(
    '"$root_dir/node_modules/.bin/tauri" build',
  );
  const thin = script.indexOf(
    '"$root_dir/scripts/thin-macos-onnx-runtime.sh" "$app"',
  );
  const finalSigning = script.indexOf(
    '"$root_dir/scripts/sign-macos-release-app.sh" "$app" "$APPLE_SIGNING_IDENTITY"',
  );

  assert.ok(tauriBuild >= 0);
  assert.ok(thin > tauriBuild);
  assert.ok(finalSigning > thin);
  const thinner = readFileSync(onnxRuntimeThinner, "utf8");
  assert.match(
    thinner,
    /mktemp -d "\$framework_dir\/\.dakia-onnx-thin\.XXXXXX"/,
  );
  assert.match(thinner, /require_runtime_install_name "\$thin_runtime"/);
});

test("ONNX thinning makes a universal packaged runtime arm64 without changing its install name", () => {
  const fixtureRoot = mkdtempSync(join(tmpdir(), "dakia-onnx-thinning-test-"));
  const app = join(fixtureRoot, "Dakia.app");
  const frameworkDir = join(app, "Contents", "Frameworks");
  const runtime = join(frameworkDir, "libonnxruntime.1.23.2.dylib");
  const mockBin = join(fixtureRoot, "mock-bin");
  mkdirSync(frameworkDir, { recursive: true });
  mkdirSync(mockBin);
  writeFileSync(
    runtime,
    "architectures=x86_64 arm64\ninstall_name=@rpath/libonnxruntime.1.23.2.dylib\n",
  );
  writeExecutable(
    join(mockBin, "lipo"),
    `#!/bin/sh
set -eu
case "$1" in
  -archs) sed -n 's/^architectures=//p' "$2" ;;
  *)
    test "$2" = -thin
    test "$3" = arm64
    test "$4" = -output
    sed 's/^architectures=.*/architectures=arm64/' "$1" > "$5"
    ;;
esac
`,
  );
  writeExecutable(
    join(mockBin, "otool"),
    `#!/bin/sh
set -eu
test "$1" = -D
printf '%s:\\n' "$2"
sed -n 's/^install_name=//p' "$2"
`,
  );
  try {
    const result = spawnSync(onnxRuntimeThinner, [app], {
      encoding: "utf8",
      env: { ...process.env, PATH: `${mockBin}:${process.env.PATH}` },
    });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(
      readFileSync(runtime, "utf8"),
      "architectures=arm64\ninstall_name=@rpath/libonnxruntime.1.23.2.dylib\n",
    );
  } finally {
    rmSync(fixtureRoot, { recursive: true, force: true });
  }
});

test("ONNX thinning rejects an unexpected packaged runtime architecture", () => {
  const fixtureRoot = mkdtempSync(join(tmpdir(), "dakia-onnx-thinning-test-"));
  const app = join(fixtureRoot, "Dakia.app");
  const frameworkDir = join(app, "Contents", "Frameworks");
  const runtime = join(frameworkDir, "libonnxruntime.1.23.2.dylib");
  const mockBin = join(fixtureRoot, "mock-bin");
  mkdirSync(frameworkDir, { recursive: true });
  mkdirSync(mockBin);
  writeFileSync(
    runtime,
    "architectures=arm64e\ninstall_name=@rpath/libonnxruntime.1.23.2.dylib\n",
  );
  writeExecutable(
    join(mockBin, "lipo"),
    "#!/bin/sh\nsed -n 's/^architectures=//p' \"$2\"\n",
  );
  try {
    const result = spawnSync(onnxRuntimeThinner, [app], {
      encoding: "utf8",
      env: { ...process.env, PATH: `${mockBin}:${process.env.PATH}` },
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /unsupported architecture set: arm64e/);
    assert.match(readFileSync(runtime, "utf8"), /architectures=arm64e/);
  } finally {
    rmSync(fixtureRoot, { recursive: true, force: true });
  }
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
  assert.match(script, /verify-macos-release-app\.sh" "\$updater_app"/);
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
  assert.match(
    verifier,
    /for architecture_target in "\$executable" "\$cli" "\$runtime"/,
  );
  assert.match(verifier, /lipo -archs "\$architecture_target"/);
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
    });
    try {
      const goodResult = spawnSync(appVerifier, [goodFixture.app], {
        encoding: "utf8",
        env: environmentFor(goodFixture.mockBin),
      });
      assert.equal(goodResult.status, 0, goodResult.stderr);
      assert.match(goodResult.stdout, /startup smoke test passed/);

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
        env: {
          PATH: `${fixture.mockBin}:${process.env.PATH}`,
        },
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
  "packaged-app verification rejects a non-arm64 app and an unexpected signing team",
  { skip: process.platform !== "darwin" },
  () => {
    const wrongArchitecture = createCliContractFixture({
      appArchitecture: "x86_64 arm64",
    });
    const wrongTeam = createCliContractFixture({ wrongTeam: true });
    const environmentFor = (mockBin) => ({
      PATH: `${mockBin}:${process.env.PATH}`,
    });
    try {
      const architectureResult = spawnSync(
        appVerifier,
        [wrongArchitecture.app],
        {
          encoding: "utf8",
          env: environmentFor(wrongArchitecture.mockBin),
        },
      );
      assert.notEqual(architectureResult.status, 0);
      assert.match(
        architectureResult.stderr,
        /not exactly Apple Silicon arm64/,
      );

      const teamResult = spawnSync(appVerifier, [wrongTeam.app], {
        encoding: "utf8",
        env: environmentFor(wrongTeam.mockBin),
      });
      assert.notEqual(teamResult.status, 0);
      assert.match(teamResult.stderr, /expected TeamIdentifier 34T9L3FGZC/);
    } finally {
      rmSync(wrongArchitecture.fixtureRoot, { recursive: true, force: true });
      rmSync(wrongTeam.fixtureRoot, { recursive: true, force: true });
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

test("release tooling has no Google OAuth secret, compiler wrapper, or startup marker contract", () => {
  const sources = [
    releaseEnvironment,
    releaseBuilder,
    appVerifier,
    join(root, ".github", "workflows", "production-release.yml"),
    join(root, "scripts", "store-local-release-secret.sh"),
    join(root, "docs", "publishing-macos-release.md"),
  ].map((path) => readFileSync(path, "utf8"));

  for (const source of sources) {
    assert.doesNotMatch(source, /DAKIA_GOOGLE_CLIENT_(?:ID|SECRET)/);
    assert.doesNotMatch(source, /google-oauth/i);
    assert.doesNotMatch(source, /GOOGLE_OAUTH_CONFIG_OK/);
  }
  assert.equal(existsSync(join(root, ".env.example")), false);
});

test("provider documentation directs new Gmail accounts to Google app passwords", () => {
  const providers = readFileSync(join(root, "docs", "providers.md"), "utf8");
  assert.match(providers, /Google app password/);
  assert.match(
    providers,
    /https:\/\/support\.google\.com\/accounts\/answer\/185833\?hl=en/,
  );
  assert.match(
    providers,
    /Do not use your personal Gmail or regular Google Account password in Dakia\./,
  );
  assert.match(
    providers,
    /Google Advanced Protection can disable app\s+passwords/,
  );
  assert.match(
    providers,
    /Existing Dakia accounts that already use Google OAuth continue to work/,
  );
});

test("release environment pins the exact Developer ID identity and team", () => {
  const script = readFileSync(releaseEnvironment, "utf8");
  assert.match(
    script,
    /Developer ID Application: Mashal Tech OU \(34T9L3FGZC\)/,
  );
  assert.match(script, /DAKIA_EXPECTED_APPLE_TEAM_ID/);
  assert.match(
    script,
    /APPLE_SIGNING_IDENTITY.*DAKIA_EXPECTED_APPLE_SIGNING_IDENTITY/,
  );
});

test("release environment pins the expected Dakia origin before a release", () => {
  const script = readFileSync(releaseEnvironment, "utf8");
  assert.match(script, /dakia_require_expected_release_origin\(\)/);
  assert.match(script, /git@github\.com:DakiaMail\/dakia-desktop\.git/);
  assert.match(script, /origin does not identify the expected repository/);
});

test("release environment rejects stale cached origin/main even when HEAD matches it", () => {
  const tempRoot = mkdtempSync(join(tmpdir(), "dakia-live-main-test-"));
  const gitPath = join(tempRoot, "git");
  writeExecutable(
    gitPath,
    `#!/bin/sh
if [ "$1" = -C ]; then shift 2; fi
case "$1 $*" in
  "rev-parse"*" HEAD"|"rev-parse"*" refs/remotes/origin/main") printf '%s\\n' 1234567890abcdef1234567890abcdef12345678 ;;
  "ls-remote"*" refs/heads/main") printf '%s\\t%s\\n' ffffffffffffffffffffffffffffffffffffffff refs/heads/main ;;
esac
`,
  );
  try {
    const result = spawnSync(
      "/bin/bash",
      [
        "-c",
        'source "$1"; dakia_require_live_main_provenance /fixture',
        "bash",
        releaseEnvironment,
      ],
      {
        encoding: "utf8",
        env: { ...process.env, PATH: `${tempRoot}:${process.env.PATH}` },
      },
    );
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /HEAD, cached origin\/main, and live origin\/main must all match/,
    );
  } finally {
    rmSync(tempRoot, { recursive: true, force: true });
  }
});
