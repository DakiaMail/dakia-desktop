import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const prepare = join(root, "scripts", "prepare-agent-runtime.sh");
const signer = join(root, "scripts", "sign-macos-release-app.sh");

test("Tauri preserves the complete nested agent runtime layout", () => {
  const config = JSON.parse(
    readFileSync(
      join(root, "apps", "desktop", "src-tauri", "tauri.conf.json"),
      "utf8",
    ),
  );
  assert.equal(config.bundle.resources["resources/agent/"], "agent/");
  assert.equal(config.bundle.resources["resources/agent/**/*"], undefined);
});

test("Windows bundles keep the executable suffix required by CreateProcess", () => {
  const source = readFileSync(prepare, "utf8");
  assert.match(source, /bundled_node="\$bundle\/runtime\/node\.exe"/);
  assert.doesNotMatch(source, /cp "\$node_binary" "\$bundle\/runtime\/node"/);
});

function executable(path, source) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, source);
  chmodSync(path, 0o755);
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function fixture({ includeDist = true } = {}) {
  const fixtureRoot = mkdtempSync(join(tmpdir(), "dakia-agent-runtime-test-"));
  const source = join(fixtureRoot, "agent-source");
  const archiveRoot = join(fixtureRoot, "archive", "node-v22.23.2-linux-x64");
  const archive = join(fixtureRoot, "node.tar.gz");
  const destination = join(fixtureRoot, "output", "agent");
  const mockBin = join(fixtureRoot, "mock-bin");
  mkdirSync(source, { recursive: true });
  mkdirSync(mockBin, { recursive: true });
  writeFileSync(
    join(source, "package.json"),
    JSON.stringify({
      name: "@dakia/agent",
      version: "0.1.0",
      type: "module",
      dependencies: { "fixture-dependency": "1.0.0" },
    }),
  );
  writeFileSync(
    join(source, "package-lock.json"),
    JSON.stringify({
      name: "@dakia/agent",
      version: "0.1.0",
      lockfileVersion: 3,
      packages: {
        "": {
          name: "@dakia/agent",
          version: "0.1.0",
          dependencies: { "fixture-dependency": "1.0.0" },
        },
      },
    }),
  );
  if (includeDist) {
    mkdirSync(join(source, "dist"), { recursive: true });
    writeFileSync(
      join(source, "dist", "cli.js"),
      "console.log('agent fixture');\n",
    );
    writeFileSync(
      join(source, "dist", "worker.js"),
      "export const worker = true;\n",
    );
    mkdirSync(join(source, "dist", "nested"));
    writeFileSync(join(source, "dist", "Zed.js"), "export {};\n");
    writeFileSync(join(source, "dist", "nested", "alpha.js"), "export {};\n");
  }
  executable(
    join(archiveRoot, "bin", "node"),
    `#!/bin/sh
set -eu
if [ "\${1:-}" = --version ]; then
  printf '%s\\n' v22.23.2
  exit 0
fi
test -f "\${1:-}"
printf '%s\\n' BUNDLED_RUNTIME_OK
`,
  );
  writeFileSync(join(archiveRoot, "LICENSE"), "Node.js fixture license\n");
  const archiveResult = spawnSync(
    "tar",
    [
      "-czf",
      archive,
      "-C",
      join(fixtureRoot, "archive"),
      "node-v22.23.2-linux-x64",
    ],
    { encoding: "utf8" },
  );
  assert.equal(archiveResult.status, 0, archiveResult.stderr);
  executable(
    join(mockBin, "npm"),
    `#!/bin/sh
set -eu
case "\${1:-}" in
  ci)
    mkdir -p node_modules/fixture-dependency
    printf '%s\\n' '{"name":"fixture-dependency","version":"1.0.0","license":"MIT"}' > node_modules/fixture-dependency/package.json
    printf '%s\\n' 'MIT fixture license' > node_modules/fixture-dependency/LICENSE
    printf '%s\\n' 'native fixture' > node_modules/fixture-dependency/addon.node
    mkdir -p node_modules/fixture-dependency/node_modules/.bin
    ln -s ../../package.json node_modules/fixture-dependency/node_modules/.bin/fixture-tool
    ;;
  ls) ;;
  *) exit 64 ;;
esac
`,
  );
  const environment = {
    ...process.env,
    PATH: `${mockBin}:${process.env.PATH}`,
    DAKIA_AGENT_RUNTIME_TESTING: "1",
    DAKIA_AGENT_RUNTIME_PLATFORM: "linux",
    DAKIA_AGENT_RUNTIME_ARCH: "x64",
    DAKIA_AGENT_PACKAGE_SOURCE: source,
    DAKIA_AGENT_RUNTIME_DESTINATION: destination,
    DAKIA_AGENT_NODE_ARCHIVE: archive,
    DAKIA_AGENT_NODE_ARCHIVE_SHA256: sha256(archive),
  };
  return { fixtureRoot, source, archive, destination, mockBin, environment };
}

test("prepares a locked, self-contained runtime that works without system Node.js", () => {
  const value = fixture();
  try {
    const result = spawnSync(prepare, [], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /Prepared self-contained Dakia agent runtime/);
    const manifest = JSON.parse(
      readFileSync(join(value.destination, "MANIFEST.json"), "utf8"),
    );
    assert.equal(manifest.node.version, "22.23.2");
    assert.equal(manifest.node.archiveSha256, sha256(value.archive));
    assert.ok(manifest.files["dist/cli.js"]);
    assert.ok(manifest.files["dist/worker.js"]);
    assert.ok(manifest.files["dist/Zed.js"]);
    assert.ok(manifest.files["dist/nested/alpha.js"]);
    assert.ok(manifest.files["licenses/npm-production.json"]);
    const inventory = JSON.parse(
      readFileSync(
        join(value.destination, "licenses", "npm-production.json"),
        "utf8",
      ),
    );
    assert.equal(inventory.packages.length, 1);
    assert.deepEqual(inventory.packages[0].licenseFiles, [
      "node_modules/fixture-dependency/LICENSE",
    ]);
    assert.ok(manifest.files["licenses/pi/LICENSE"]);
    assert.ok(manifest.files["package-lock.json"]);
    assert.ok(manifest.files["node_modules/fixture-dependency/LICENSE"]);
    assert.ok(manifest.files["runtime/node"]);
    assert.equal(
      existsSync(
        join(
          value.destination,
          "node_modules",
          "fixture-dependency",
          "node_modules",
          ".bin",
        ),
      ),
      false,
    );

    const emptyPath = join(value.fixtureRoot, "empty-path");
    mkdirSync(emptyPath);
    const offline = spawnSync(
      join(value.destination, "runtime", "node"),
      [join(value.destination, "dist", "cli.js")],
      {
        encoding: "utf8",
        env: { PATH: emptyPath },
      },
    );
    assert.equal(offline.status, 0, offline.stderr);
    assert.equal(offline.stdout.trim(), "BUNDLED_RUNTIME_OK");
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("fails before replacing output when the official runtime checksum is wrong", () => {
  const value = fixture();
  try {
    mkdirSync(value.destination, { recursive: true });
    writeFileSync(join(value.destination, "previous"), "keep");
    const result = spawnSync(prepare, [], {
      encoding: "utf8",
      env: {
        ...value.environment,
        DAKIA_AGENT_NODE_ARCHIVE_SHA256: "0".repeat(64),
      },
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /FAILED|checksum/i);
    assert.equal(
      readFileSync(join(value.destination, "previous"), "utf8"),
      "keep",
    );
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("fails clearly when compiled agent assets are missing", () => {
  const value = fixture({ includeDist: false });
  try {
    const result = spawnSync(prepare, [], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /Missing agent packaging input: .*dist\/cli\.js/,
    );
    assert.match(result.stderr, /npm run build:agent/);
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("fails when a production dependency has no packaged license notice", () => {
  const value = fixture();
  try {
    const npm = join(value.mockBin, "npm");
    writeFileSync(
      npm,
      readFileSync(npm, "utf8").replace(
        "    printf '%s\\n' 'MIT fixture license' > node_modules/fixture-dependency/LICENSE\n",
        "",
      ),
    );
    const result = spawnSync(prepare, [], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /no packaged license or audited supplemental notice: fixture-dependency@1\.0\.0/,
    );
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("verification detects runtime corruption and unexpected files", () => {
  const value = fixture();
  try {
    const prepared = spawnSync(prepare, [], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.equal(prepared.status, 0, prepared.stderr);
    writeFileSync(join(value.destination, "dist", "cli.js"), "corrupted\n");
    let result = spawnSync(prepare, ["--verify", value.destination], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /checksum mismatch: dist\/cli\.js/);

    cpSync(value.source, join(value.fixtureRoot, "unused-copy"), {
      recursive: true,
    });
    writeFileSync(join(value.destination, "unexpected"), "unexpected\n");
    result = spawnSync(prepare, ["--verify", value.destination], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /Unexpected: unexpected/);
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("verification rejects a runtime prepared for a different target", () => {
  const value = fixture();
  try {
    const prepared = spawnSync(prepare, [], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.equal(prepared.status, 0, prepared.stderr);
    const result = spawnSync(prepare, ["--verify", value.destination], {
      encoding: "utf8",
      env: {
        ...value.environment,
        DAKIA_AGENT_RUNTIME_PLATFORM: "windows",
        DAKIA_AGENT_RUNTIME_ARCH: "x64",
      },
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /target linux-x64 does not match expected win-x64/,
    );
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("preserves the Windows Node executable and archive license layout", () => {
  const value = fixture();
  const archiveParent = join(value.fixtureRoot, "windows-archive");
  const archiveRoot = join(archiveParent, "node-v22.23.2-win-x64");
  const archive = join(value.fixtureRoot, "node-windows.zip");
  try {
    executable(
      join(archiveRoot, "node.exe"),
      `#!/bin/sh
if [ "\${1:-}" = --version ]; then printf '%s\\n' v22.23.2; else exit 64; fi
`,
    );
    writeFileSync(join(archiveRoot, "LICENSE"), "Node.js fixture license\n");
    const packed = spawnSync("zip", ["-qr", archive, "node-v22.23.2-win-x64"], {
      cwd: archiveParent,
      encoding: "utf8",
    });
    assert.equal(packed.status, 0, packed.stderr);
    const result = spawnSync(prepare, [], {
      encoding: "utf8",
      env: {
        ...value.environment,
        DAKIA_AGENT_RUNTIME_PLATFORM: "windows",
        DAKIA_AGENT_RUNTIME_ARCH: "x64",
        DAKIA_AGENT_NODE_ARCHIVE: archive,
        DAKIA_AGENT_NODE_ARCHIVE_SHA256: sha256(archive),
      },
    });
    assert.equal(result.status, 0, result.stderr);
    const manifest = JSON.parse(
      readFileSync(join(value.destination, "MANIFEST.json"), "utf8"),
    );
    assert.ok(manifest.files["runtime/node.exe"]);
    assert.equal(manifest.files["runtime/node"], undefined);
    assert.ok(manifest.files["licenses/node/LICENSE"]);
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("rejects a macOS runtime newer than Dakia's deployment target", () => {
  const value = fixture();
  const archiveRoot = join(
    value.fixtureRoot,
    "darwin-archive",
    "node-v22.23.2-darwin-arm64",
  );
  const archive = join(value.fixtureRoot, "node-darwin.tar.gz");
  try {
    executable(
      join(archiveRoot, "bin", "node"),
      `#!/bin/sh
if [ "\${1:-}" = --version ]; then printf '%s\\n' v22.23.2; else exit 64; fi
`,
    );
    writeFileSync(join(archiveRoot, "LICENSE"), "Node.js fixture license\n");
    const packed = spawnSync(
      "tar",
      [
        "-czf",
        archive,
        "-C",
        join(value.fixtureRoot, "darwin-archive"),
        "node-v22.23.2-darwin-arm64",
      ],
      { encoding: "utf8" },
    );
    assert.equal(packed.status, 0, packed.stderr);
    executable(
      join(value.mockBin, "lipo"),
      "#!/bin/sh\nprintf '%s\\n' arm64\n",
    );
    executable(
      join(value.mockBin, "otool"),
      "#!/bin/sh\nprintf '%s\\n' 'Load command 1' '      cmd LC_BUILD_VERSION' '    minos 12.0'\n",
    );
    const result = spawnSync(prepare, [], {
      encoding: "utf8",
      env: {
        ...value.environment,
        DAKIA_AGENT_RUNTIME_PLATFORM: "darwin",
        DAKIA_AGENT_RUNTIME_ARCH: "arm64",
        DAKIA_AGENT_NODE_ARCHIVE: archive,
        DAKIA_AGENT_NODE_ARCHIVE_SHA256: sha256(archive),
      },
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /requires macOS 12\.0, newer than Dakia's 11\.0 minimum/,
    );
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});

test("release signing gives bundled Node.js the required JIT entitlements", () => {
  const source = readFileSync(signer, "utf8");
  assert.match(source, /com\.apple\.security\.cs\.allow-jit/);
  assert.match(
    source,
    /com\.apple\.security\.cs\.allow-unsigned-executable-memory/,
  );
  assert.match(
    source,
    /codesign --force --options runtime --timestamp --entitlements "\$entitlements" --sign "\$identity" "\$agent_node"/,
  );
  assert.match(source, /find "\$agent\/node_modules" -type f/);
  assert.match(source, /prepare-agent-runtime\.mjs" refresh "\$agent"/);
  assert.ok(
    source.indexOf('refresh "$agent"') <
      source.lastIndexOf(
        'codesign --force --options runtime --timestamp --sign "$identity" "$app"',
      ),
    "the signed-byte manifest must be refreshed before sealing the app",
  );
});

test("macOS packaging gates target native dependencies on architecture and deployment target", () => {
  const preparer = readFileSync(prepare, "utf8");
  const verifier = readFileSync(
    join(root, "scripts", "verify-macos-release-app.sh"),
    "utf8",
  );
  for (const source of [preparer, verifier]) {
    assert.match(source, /lipo -archs "\$(?:native_module|native)"/);
    assert.match(source, /Target-selected native agent dependency lacks arm64/);
    assert.match(source, /Native agent dependency requires macOS/);
    assert.match(source, /otool -l -arch/);
  }
});

test("release signing seals native dependencies and refreshes their checksums", () => {
  const value = fixture();
  try {
    const prepared = spawnSync(prepare, [], {
      encoding: "utf8",
      env: value.environment,
    });
    assert.equal(prepared.status, 0, prepared.stderr);
    const macMetadata = join(value.fixtureRoot, "mac-node-metadata.json");
    writeFileSync(
      macMetadata,
      '{"version":"22.23.2","platform":"darwin","architecture":"arm64","archive":"node-v22.23.2-darwin-arm64.tar.gz","archiveSha256":"61130f394c1630d211dd50aecc4353d379480f36d3ac913cd85dbba1aed585c6","minimumMacOS":"11.0"}\n',
    );
    const resealed = spawnSync(
      process.execPath,
      [
        join(root, "scripts", "prepare-agent-runtime.mjs"),
        "build",
        value.destination,
        macMetadata,
      ],
      { encoding: "utf8" },
    );
    assert.equal(resealed.status, 0, resealed.stderr);

    const app = join(value.fixtureRoot, "Dakia.app");
    const agent = join(app, "Contents", "Resources", "agent");
    mkdirSync(join(app, "Contents", "MacOS"), { recursive: true });
    mkdirSync(join(app, "Contents", "Frameworks"), { recursive: true });
    cpSync(value.destination, agent, { recursive: true });
    for (const path of [
      join(app, "Contents", "MacOS", "dakia"),
      join(app, "Contents", "MacOS", "dakia-desktop"),
      join(app, "Contents", "Frameworks", "libonnxruntime.1.23.2.dylib"),
    ]) {
      executable(path, "#!/bin/sh\nexit 0\n");
    }
    const native = join(
      agent,
      "node_modules",
      "fixture-dependency",
      "addon.node",
    );

    const signLog = join(value.fixtureRoot, "codesign.log");
    executable(
      join(value.mockBin, "file"),
      `#!/bin/sh
case "\${2:-\${1:-}}" in
  *.node) printf '%s\\n' 'Mach-O 64-bit bundle arm64' ;;
  *) /usr/bin/file "$@" ;;
esac
`,
    );
    executable(
      join(value.mockBin, "codesign"),
      `#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$DAKIA_SIGN_LOG"
case "\${1:-}" in
  --verify) exit 0 ;;
  -d)
    printf '%s\\n' '<key>com.apple.security.cs.allow-jit</key>' '<key>com.apple.security.cs.allow-unsigned-executable-memory</key>' >&2
    ;;
  --force)
    last=
    for argument in "$@"; do last=$argument; done
    if [ -f "$last" ]; then printf '%s\\n' SIGNED >> "$last"; fi
    ;;
  *) exit 64 ;;
esac
`,
    );
    const result = spawnSync(signer, [app, "Fixture Identity"], {
      encoding: "utf8",
      env: {
        ...process.env,
        PATH: `${value.mockBin}:${process.env.PATH}`,
        DAKIA_SIGN_LOG: signLog,
      },
    });
    assert.equal(result.status, 0, result.stderr);
    const log = readFileSync(signLog, "utf8");
    assert.match(
      log,
      /--entitlements .* --sign Fixture Identity .*\/runtime\/node/,
    );
    assert.ok(
      log.indexOf(native) < log.indexOf(join(agent, "runtime", "node")),
    );

    const verified = spawnSync(prepare, ["--verify", agent], {
      encoding: "utf8",
      env: {
        ...value.environment,
        DAKIA_AGENT_RUNTIME_PLATFORM: "darwin",
        DAKIA_AGENT_RUNTIME_ARCH: "arm64",
      },
    });
    assert.equal(verified.status, 0, verified.stderr);
  } finally {
    rmSync(value.fixtureRoot, { recursive: true, force: true });
  }
});
