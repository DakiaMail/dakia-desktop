import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { createHash } from "node:crypto";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const publisher = join(root, "scripts", "publish-github-release.sh");
const tag = "v0.4.1";
const version = tag.slice(1);
const commit = "1234567890abcdef1234567890abcdef12345678";
const tagObject = "abcdef1234567890abcdef1234567890abcdef12";

function writeExecutable(path, source) {
  writeFileSync(path, source);
  chmodSync(path, 0o755);
}

function updaterSignature() {
  const config = JSON.parse(
    readFileSync(
      join(root, "apps", "desktop", "src-tauri", "tauri.conf.json"),
      "utf8",
    ),
  );
  const publicKeyLines = Buffer.from(config.plugins.updater.pubkey, "base64")
    .toString("utf8")
    .trimEnd()
    .split("\n");
  const publicKeyRecord = Buffer.from(publicKeyLines[1], "base64");
  const keyId = publicKeyRecord.subarray(2, 10);
  const signatureRecord = Buffer.concat([
    Buffer.from("ED"),
    keyId,
    Buffer.alloc(64, 1),
  ]).toString("base64");
  const trustedSignature = Buffer.alloc(64, 2).toString("base64");
  return Buffer.from(
    [
      "untrusted comment: signature from test key",
      signatureRecord,
      "trusted comment: timestamp:1",
      trustedSignature,
    ].join("\n") + "\n",
  ).toString("base64");
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function createHarness({
  releaseState = "draft",
  manifestNotes = "Release notes",
  gitScenario = "valid",
} = {}) {
  const fixtureRoot = mkdtempSync(
    join(tmpdir(), "dakia-github-release-behavior-"),
  );
  const bin = join(fixtureRoot, "bin");
  const assets = join(fixtureRoot, "assets");
  mkdirSync(bin);
  mkdirSync(assets);

  const filenames = {
    dmg: `Dakia_${version}_aarch64.dmg`,
    update: "Dakia-aarch64.app.tar.gz",
    signature: "Dakia-aarch64.app.tar.gz.sig",
    checksums: "SHA256SUMS.txt",
    notes: "release-notes.md",
    sourceMarker: "source-commit.txt",
  };
  const content = {
    [filenames.dmg]: Buffer.from("test dmg bytes\n"),
    [filenames.update]: Buffer.from("test updater bytes\n"),
    [filenames.signature]: Buffer.from(updaterSignature()),
    [filenames.notes]: Buffer.from("Release notes\n"),
  };
  for (const [filename, bytes] of Object.entries(content)) {
    writeFileSync(join(assets, filename), bytes);
  }
  writeFileSync(join(assets, filenames.sourceMarker), `${commit}\n`);
  const checksumOrder = [filenames.dmg, filenames.update, filenames.signature];
  writeFileSync(
    join(assets, filenames.checksums),
    checksumOrder
      .map((filename) => `${sha256(content[filename])}  ${filename}`)
      .join("\n") + "\n",
  );

  const manifest = {
    version,
    pub_date: "2026-08-27T00:00:00Z",
    notes: manifestNotes,
    platforms: {
      "darwin-aarch64": {
        url: `https://downloads.dakiamail.com/macos/${tag}/${filenames.update}`,
        signature: content[filenames.signature].toString().trim(),
      },
    },
  };
  const manifestPath = join(fixtureRoot, "latest.json");
  const statePath = join(fixtureRoot, "release-state");
  const editLog = join(fixtureRoot, "edit.log");
  const allowedSigners = join(fixtureRoot, "allowed-signers");
  const signingKey = join(fixtureRoot, "release-signing-key.pub");
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  writeFileSync(statePath, `${releaseState}\n`);
  writeFileSync(allowedSigners, "release@example.test ssh-ed25519 AAAA\n");
  writeFileSync(signingKey, "ssh-ed25519 AAAA release@example.test\n");

  writeExecutable(
    join(bin, "git"),
    `#!/bin/bash
set -euo pipefail
if [[ "\${1:-}" == "-C" ]]; then shift 2; fi
while [[ "\${1:-}" == "-c" ]]; do shift 2; done
case "\${1:-}" in
  status)
    [[ "\${MOCK_GIT_SCENARIO}" == dirty ]] && printf '?? unexpected-file\n'
    ;;
  branch)
    [[ "\${MOCK_GIT_SCENARIO}" == wrong-branch ]] && printf 'feature\n' || printf 'main\n'
    ;;
  remote)
    printf 'git@github.com:DakiaMail/dakia-desktop.git\n'
    ;;
  rev-parse)
    case "\${*: -1}" in
      HEAD) printf '${commit}\n' ;;
      refs/remotes/origin/main) printf '${commit}\n' ;;
      *'^{tag}') printf '${tagObject}\n' ;;
      *'^{commit}') printf '${commit}\n' ;;
      refs/tags/*) printf '${tagObject}\n' ;;
      *) exit 1 ;;
    esac
    ;;
  config)
    case "\${*: -1}" in
      gpg.format) printf 'ssh\n' ;;
      user.signingkey) printf '%s\n' "\${MOCK_SIGNING_KEY}" ;;
      gpg.ssh.allowedSignersFile) printf '%s\n' "\${MOCK_ALLOWED_SIGNERS}" ;;
      *) exit 1 ;;
    esac
    ;;
  cat-file)
    printf '%s\n' 'object ${commit}' 'type commit' 'tag ${tag}' 'tagger Release <release@example.test>' '' 'Dakia ${tag}' '-----BEGIN SSH SIGNATURE-----' 'fixture' '-----END SSH SIGNATURE-----'
    ;;
  verify-tag) ;;
  ls-remote)
    remote_object='${tagObject}'
    [[ "\${MOCK_GIT_SCENARIO}" == tag-mismatch ]] && remote_object='ffffffffffffffffffffffffffffffffffffffff'
    printf '%s\trefs/tags/${tag}\n%s\trefs/tags/${tag}^{}\n' "\$remote_object" '${commit}'
    ;;
  *) exit 64 ;;
esac
exit 0
`,
  );

  writeExecutable(
    join(bin, "ssh-keygen"),
    `#!/bin/sh
printf '%s\n' '256 SHA256:kN9R3QFJZbrE5i2HjEpp+ns5ZNxBTuFySvFx8Ldf/gE release@example.test (ED25519)'
`,
  );

  writeExecutable(
    join(bin, "gh"),
    `#!/bin/bash
set -euo pipefail
case "\${1:-} \${2:-}" in
  'auth status') exit 0 ;;
  'release view')
    state="$(tr -d '\\n' < "\${MOCK_RELEASE_STATE}")"
    if [[ "\$state" == draft ]]; then draft=true; else draft=false; fi
    node - "\$draft" <<'NODE'
const fs = require("node:fs");
const path = require("node:path");
const draft = process.argv[2] === "true";
const names = [
  "Dakia_${version}_aarch64.dmg",
  "Dakia-aarch64.app.tar.gz",
  "Dakia-aarch64.app.tar.gz.sig",
  "SHA256SUMS.txt",
];
process.stdout.write(JSON.stringify({
  tagName: "${tag}",
  targetCommitish: "${commit}",
  name: "Dakia ${tag}",
  body: fs.readFileSync(path.join(process.env.MOCK_ASSET_DIR, "release-notes.md"), "utf8"),
  isDraft: draft,
  isPrerelease: false,
  assets: names.map((name) => ({ name })),
}));
NODE
    ;;
  'release download')
    destination= pattern=
    while [[ "\$#" -gt 0 ]]; do
      case "\$1" in
        --dir) destination="\$2"; shift 2 ;;
        --pattern) pattern="\$2"; shift 2 ;;
        *) shift ;;
      esac
    done
    cp "\${MOCK_ASSET_DIR}/\$pattern" "\$destination/\$pattern"
    ;;
  'release edit')
    printf 'edit\n' >> "\${MOCK_EDIT_LOG}"
    printf 'public\n' > "\${MOCK_RELEASE_STATE}"
    ;;
  *) exit 64 ;;
esac
`,
  );

  writeExecutable(
    join(bin, "curl"),
    `#!/bin/bash
set -euo pipefail
output= url=
while [[ "\$#" -gt 0 ]]; do
  case "\$1" in
    --output) output="\$2"; shift 2 ;;
    --write-out) shift 2 ;;
    --connect-timeout|--max-time|--retry|--retry-delay|--retry-max-time|--proto) shift 2 ;;
    --silent|--show-error|--location) shift ;;
    *) url="\$1"; shift ;;
  esac
done
if [[ "\$url" == *'/macos/latest/latest.json?'* ]]; then
  cp "\${MOCK_MANIFEST}" "\$output"
else
  artifact="\${url##*/}"
  cp "\${MOCK_ASSET_DIR}/\$artifact" "\$output"
fi
printf '200'
`,
  );

  const env = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    MOCK_ALLOWED_SIGNERS: allowedSigners,
    MOCK_ASSET_DIR: assets,
    MOCK_EDIT_LOG: editLog,
    MOCK_GIT_SCENARIO: gitScenario,
    MOCK_MANIFEST: manifestPath,
    MOCK_RELEASE_STATE: statePath,
    MOCK_SIGNING_KEY: signingKey,
  };
  return { fixtureRoot, assets, editLog, env };
}

function runHarness(options) {
  const harness = createHarness(options);
  const result = spawnSync(publisher, [tag, harness.assets], {
    encoding: "utf8",
    env: harness.env,
  });
  const edits = (() => {
    try {
      return readFileSync(harness.editLog, "utf8").split("\n").filter(Boolean)
        .length;
    } catch {
      return 0;
    }
  })();
  return { ...harness, edits, result };
}

test("valid verified draft reaches exactly one GitHub publication edit", () => {
  const harness = runHarness();
  try {
    assert.equal(
      harness.result.status,
      0,
      JSON.stringify({
        stdout: harness.result.stdout,
        stderr: harness.result.stderr,
        error: harness.result.error?.message,
      }),
    );
    assert.equal(harness.edits, 1);
    assert.match(harness.result.stdout, /Published and independently verified/);
  } finally {
    rmSync(harness.fixtureRoot, { recursive: true, force: true });
  }
});

test("an already-public exact release is verified without another edit", () => {
  const harness = runHarness({ releaseState: "public" });
  try {
    assert.equal(
      harness.result.status,
      0,
      JSON.stringify({
        stdout: harness.result.stdout,
        stderr: harness.result.stderr,
        error: harness.result.error?.message,
      }),
    );
    assert.equal(harness.edits, 0);
  } finally {
    rmSync(harness.fixtureRoot, { recursive: true, force: true });
  }
});

test("wrong public R2 notes fail before GitHub publication", () => {
  const harness = runHarness({ manifestNotes: "Stale release notes" });
  try {
    assert.notEqual(harness.result.status, 0);
    assert.equal(harness.edits, 0);
    assert.match(harness.result.stderr, /exact signed candidate/);
  } finally {
    rmSync(harness.fixtureRoot, { recursive: true, force: true });
  }
});

test("a remote tag-object mismatch fails before GitHub publication", () => {
  const harness = runHarness({ gitScenario: "tag-mismatch" });
  try {
    assert.notEqual(harness.result.status, 0);
    assert.equal(harness.edits, 0);
    assert.match(harness.result.stderr, /Remote release tag object/);
  } finally {
    rmSync(harness.fixtureRoot, { recursive: true, force: true });
  }
});
