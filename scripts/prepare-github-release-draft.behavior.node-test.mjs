import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const repositoryRoot = new URL("..", import.meta.url).pathname;
const draftSource = join(
  repositoryRoot,
  "scripts",
  "prepare-github-release-draft.sh",
);
const releaseEnvironmentSource = join(
  repositoryRoot,
  "scripts",
  "local-release-env.sh",
);
const tag = "v0.4.2";
const version = tag.slice(1);
const commit = "1234567890abcdef1234567890abcdef12345678";
const tagObject = "abcdef1234567890abcdef1234567890abcdef12";

function executable(path, source) {
  writeFileSync(path, source);
  chmodSync(path, 0o755);
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function createHarness({
  manifestVersion = version,
  corruptAsset = "",
  missingAsset = "",
  liveMainMoves = false,
  releaseExists = true,
} = {}) {
  const fixture = mkdtempSync(join(tmpdir(), "dakia-draft-resume-test-"));
  const root = join(fixture, "repo");
  const scripts = join(root, "scripts");
  const bin = join(fixture, "bin");
  const assets = join(fixture, "assets");
  const createLog = join(fixture, "create.log");
  const liveMainCalls = join(fixture, "live-main-calls");
  const manifestPath = join(fixture, "latest.json");
  const signingKey = join(fixture, "release-signing-key.pub");
  const allowedSigners = join(fixture, "allowed-signers");
  mkdirSync(scripts, { recursive: true });
  mkdirSync(bin);
  mkdirSync(assets);
  writeFileSync(createLog, "");
  writeFileSync(liveMainCalls, "0");
  copyFileSync(draftSource, join(scripts, "prepare-github-release-draft.sh"));
  copyFileSync(releaseEnvironmentSource, join(scripts, "local-release-env.sh"));
  chmodSync(join(scripts, "prepare-github-release-draft.sh"), 0o755);
  writeFileSync(signingKey, "ssh-ed25519 AAAA release@example.test\n");
  writeFileSync(allowedSigners, "release@example.test ssh-ed25519 AAAA\n");

  const files = {
    [`Dakia_${version}_aarch64.dmg`]: "test dmg\n",
    "Dakia-aarch64.app.tar.gz": "test updater\n",
    "Dakia-aarch64.app.tar.gz.sig": "test signature\n",
    "release-notes.md": "Release notes\n",
    "source-commit.txt": `${commit}\n`,
  };
  for (const [filename, body] of Object.entries(files))
    writeFileSync(join(assets, filename), body);
  writeFileSync(
    join(assets, "SHA256SUMS.txt"),
    [
      `Dakia_${version}_aarch64.dmg`,
      "Dakia-aarch64.app.tar.gz",
      "Dakia-aarch64.app.tar.gz.sig",
    ]
      .map((filename) => `${sha256(files[filename])}  ${filename}`)
      .join("\n") + "\n",
  );
  writeFileSync(
    manifestPath,
    `${JSON.stringify({
      version: manifestVersion,
      notes: files["release-notes.md"].trim(),
      platforms: {
        "darwin-aarch64": {
          url: `https://downloads.dakiamail.com/macos/v${manifestVersion}/Dakia-aarch64.app.tar.gz`,
          signature:
            manifestVersion === version
              ? files["Dakia-aarch64.app.tar.gz.sig"].trim()
              : "older-signature",
        },
      },
    })}\n`,
  );

  executable(
    join(bin, "git"),
    `#!/bin/bash
set -euo pipefail
[[ "\${1:-}" == -C ]] && shift 2
while [[ "\${1:-}" == -c ]]; do shift 2; done
case "\${1:-}" in
  status|verify-tag) ;;
  branch) printf 'main\\n' ;;
  remote) printf 'git@github.com:DakiaMail/dakia-desktop.git\\n' ;;
  rev-parse)
    case "\${*: -1}" in
      HEAD|refs/remotes/origin/main|*'^{commit}') printf '${commit}\\n' ;;
      *'^{tag}'|refs/tags/*) printf '${tagObject}\\n' ;;
      *) exit 1 ;;
    esac ;;
  config)
    case "\${*: -1}" in
      gpg.format) printf 'ssh\\n' ;;
      user.signingkey) printf '%s\\n' "\${MOCK_SIGNING_KEY}" ;;
      gpg.ssh.allowedSignersFile) printf '%s\\n' "\${MOCK_ALLOWED_SIGNERS}" ;;
      *) exit 1 ;;
    esac ;;
  cat-file) printf '%s\\n' '-----BEGIN SSH SIGNATURE-----' 'fixture' '-----END SSH SIGNATURE-----' ;;
  ls-remote)
    if [[ "\${*: -1}" == refs/heads/main ]]; then
      calls="$(cat "\${MOCK_LIVE_MAIN_CALLS}")"; calls=$((calls + 1)); printf '%s' "\$calls" > "\${MOCK_LIVE_MAIN_CALLS}"
      if [[ "\${MOCK_LIVE_MAIN_MOVES}" == true && "\$calls" -gt 1 ]]; then
        printf 'ffffffffffffffffffffffffffffffffffffffff\\trefs/heads/main\\n'
      else
        printf '${commit}\\trefs/heads/main\\n'
      fi
    else
      printf '${tagObject}\\trefs/tags/${tag}\\n${commit}\\trefs/tags/${tag}^{}\\n'
    fi ;;
  *) exit 64 ;;
esac
`,
  );
  executable(
    join(bin, "ssh-keygen"),
    "#!/bin/sh\nprintf '%s\\n' '256 SHA256:kN9R3QFJZbrE5i2HjEpp+ns5ZNxBTuFySvFx8Ldf/gE release@example.test'\n",
  );
  executable(
    join(bin, "jq"),
    `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
const values = {};
for (let index = 0; index < args.length; index += 1) if (args[index] === "--arg") values[args[index + 1]] = args[index + 2];
const filter = args.find((value) => value.startsWith(".")) ?? "";
const file = args.find((value) => value.startsWith("/") && fs.existsSync(value));
const data = JSON.parse(file ? fs.readFileSync(file, "utf8") : fs.readFileSync(0, "utf8"));
if (args.includes("-e")) {
  const platform = values.platform ?? "darwin-aarch64";
  const entry = data.platforms?.[platform];
  process.exit(data.version === values.version && data.notes === values.notes && entry?.url === values.url && entry?.signature === values.signature ? 0 : 1);
}
if (filter === ".assets[].name") process.stdout.write(data.assets.map((asset) => asset.name).join("\\n") + "\\n");
else if (filter === ".body") process.stdout.write(data.body);
else process.stdout.write(String(data[filter.slice(1)]) + "\\n");
`,
  );
  executable(
    join(bin, "gh"),
    `#!/bin/bash
set -euo pipefail
case "\${1:-} \${2:-}" in
  'auth status') ;;
  'release view')
    [[ "\${MOCK_RELEASE_EXISTS}" == true ]] || exit 1
    node - <<'NODE'
const fs = require("node:fs");
const names = ["Dakia_${version}_aarch64.dmg", "Dakia-aarch64.app.tar.gz", "Dakia-aarch64.app.tar.gz.sig", "SHA256SUMS.txt"];
process.stdout.write(JSON.stringify({ tagName: "${tag}", targetCommitish: "${commit}", name: "Dakia ${tag}", body: fs.readFileSync(process.env.MOCK_ASSETS + "/release-notes.md", "utf8"), isDraft: false, isPrerelease: false, assets: names.map((name) => ({ name })) }));
NODE
    ;;
  'release download')
    destination= pattern=
    while [[ "\$#" -gt 0 ]]; do case "\$1" in --dir) destination="\$2"; shift 2 ;; --pattern) pattern="\$2"; shift 2 ;; *) shift ;; esac; done
    cp "\${MOCK_ASSETS}/\$pattern" "\$destination/\$pattern" ;;
  'release create') printf create >> "\${MOCK_CREATE_LOG}" ;;
  *) exit 64 ;;
esac
`,
  );
  executable(
    join(bin, "curl"),
    '#!/bin/bash\nset -euo pipefail\noutput= url=\nwhile [[ "$#" -gt 0 ]]; do case "$1" in --output) output="$2"; shift 2 ;; --write-out|--connect-timeout|--max-time|--proto) shift 2 ;; --silent|--show-error|--location) shift ;; *) url="$1"; shift ;; esac; done\nif [[ "$url" == *\'/macos/latest/latest.json?\'* ]]; then cp "$MOCK_MANIFEST" "$output"; printf 200; exit 0; fi\nname="${url##*/}"\nif [[ "$name" == Dakia-Apple-Silicon.dmg ]]; then source="$MOCK_ASSETS/Dakia_0.4.2_aarch64.dmg"; else source="$MOCK_ASSETS/$name"; fi\nif [[ "$name" == "$MOCK_MISSING_ASSET" ]]; then : > "$output"; printf 404; exit 0; fi\nif [[ "$name" == "$MOCK_CORRUPT_ASSET" ]]; then printf corrupt > "$output"; printf 200; exit 0; fi\ncp "$source" "$output"; printf 200\n',
  );

  return {
    fixture,
    assets,
    createLog,
    command: join(scripts, "prepare-github-release-draft.sh"),
    env: {
      ...process.env,
      PATH: `${bin}:${process.env.PATH}`,
      MOCK_ALLOWED_SIGNERS: allowedSigners,
      MOCK_ASSETS: assets,
      MOCK_CREATE_LOG: createLog,
      MOCK_CORRUPT_ASSET: corruptAsset,
      MOCK_LIVE_MAIN_CALLS: liveMainCalls,
      MOCK_LIVE_MAIN_MOVES: String(liveMainMoves),
      MOCK_MANIFEST: manifestPath,
      MOCK_MISSING_ASSET: missingAsset,
      MOCK_RELEASE_EXISTS: String(releaseExists),
      MOCK_SIGNING_KEY: signingKey,
    },
  };
}

test("an already-public GitHub release cannot precede a newer updater candidate", () => {
  const harness = createHarness({ manifestVersion: "0.4.1" });
  try {
    const result = spawnSync(harness.command, [tag, harness.assets], {
      encoding: "utf8",
      env: harness.env,
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /Existing GitHub Release is public while public latest\.json is not the exact R2 resume candidate/,
    );
    assert.equal(readFileSync(harness.createLog, "utf8"), "");
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("an already-public GitHub release is accepted only for its exact R2 resume", () => {
  const harness = createHarness();
  try {
    const result = spawnSync(harness.command, [tag, harness.assets], {
      encoding: "utf8",
      env: harness.env,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(
      result.stdout,
      /Verified exact existing public GitHub Release/,
    );
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("an exact public resume rejects corrupt versioned R2 bytes", () => {
  const harness = createHarness({ corruptAsset: "Dakia-aarch64.app.tar.gz" });
  try {
    const result = spawnSync(harness.command, [tag, harness.assets], {
      encoding: "utf8",
      env: harness.env,
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /Public R2 resume updater archive is missing or differs/,
    );
    assert.equal(readFileSync(harness.createLog, "utf8"), "");
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("an exact public resume rejects a missing versioned R2 DMG", () => {
  const harness = createHarness({ missingAsset: "Dakia-Apple-Silicon.dmg" });
  try {
    const result = spawnSync(harness.command, [tag, harness.assets], {
      encoding: "utf8",
      env: harness.env,
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /Public R2 resume Apple Silicon DMG is missing or differs/,
    );
    assert.equal(readFileSync(harness.createLog, "utf8"), "");
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("a moved live main stops draft creation immediately before mutation", () => {
  const harness = createHarness({ liveMainMoves: true, releaseExists: false });
  try {
    const result = spawnSync(harness.command, [tag, harness.assets], {
      encoding: "utf8",
      env: harness.env,
    });
    assert.notEqual(result.status, 0);
    assert.match(
      result.stderr,
      /HEAD, cached origin\/main, and live origin\/main must all match/,
    );
    assert.equal(readFileSync(harness.createLog, "utf8"), "");
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});
