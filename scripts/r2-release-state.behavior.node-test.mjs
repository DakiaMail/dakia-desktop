import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { createHash } from "node:crypto";
import test from "node:test";

const repositoryRoot = new URL("..", import.meta.url).pathname;
const publisherSource = join(
  repositoryRoot,
  "scripts",
  "publish-release-to-r2.sh",
);
const commit = "1234567890abcdef1234567890abcdef12345678";

function executable(path, source) {
  writeFileSync(path, source);
  chmodSync(path, 0o755);
}

function hash(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function objectPath(store, key) {
  return join(store, key);
}

function putInitial(store, key, bytes) {
  const path = objectPath(store, key);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, bytes);
  writeFileSync(`${path}.etag`, `etag-${hash(bytes).slice(0, 16)}`);
}

function putVisibleInitial(harness, key, bytes) {
  putInitial(harness.store, key, bytes);
  putInitial(harness.publicStore, key, bytes);
}

function manifest(version, notes = `Notes ${version}`) {
  return `${JSON.stringify(
    {
      version,
      pub_date: "2026-08-27T00:00:00Z",
      notes,
      platforms: {
        "darwin-aarch64": {
          url: `https://downloads.dakiamail.com/macos/v${version}/Dakia-aarch64.app.tar.gz`,
          signature: `signature-${version}`,
        },
      },
    },
    null,
    2,
  )}\n`;
}

function createHarness() {
  const fixture = mkdtempSync(join(tmpdir(), "dakia-r2-state-test-"));
  const root = join(fixture, "repo");
  const scripts = join(root, "scripts");
  const bin = join(fixture, "bin");
  const store = join(fixture, "store");
  const publicStore = join(fixture, "public-store");
  mkdirSync(scripts, { recursive: true });
  mkdirSync(bin);
  mkdirSync(store);
  mkdirSync(publicStore);
  mkdirSync(join(root, "apps", "desktop", "src-tauri"), { recursive: true });
  writeFileSync(
    join(root, "apps", "desktop", "src-tauri", "tauri.conf.json"),
    "{}\n",
  );
  copyFileSync(publisherSource, join(scripts, "publish-release-to-r2.sh"));
  chmodSync(join(scripts, "publish-release-to-r2.sh"), 0o755);
  for (const verifier of [
    "verify-macos-release-dmg.sh",
    "verify-macos-release-app.sh",
    "prepare-github-release-draft.sh",
  ]) {
    executable(join(scripts, verifier), "#!/bin/sh\nexit 0\n");
  }
  writeFileSync(
    join(scripts, "verify-updater-signature.mjs"),
    "process.exit(0);\n",
  );
  writeFileSync(
    join(scripts, "updater-manifest.mjs"),
    `import { readFileSync, writeFileSync } from "node:fs";
const [command, ...args] = process.argv.slice(2);
const options = Object.fromEntries(Array.from({ length: args.length / 2 }, (_, i) => [args[i * 2].slice(2), args[i * 2 + 1]]));
if (command === "validate") { JSON.parse(readFileSync(options.manifest, "utf8")); process.exit(0); }
const result = {
  version: options.version,
  pub_date: options["pub-date"],
  notes: readFileSync(options["notes-file"], "utf8").trim(),
  platforms: { "darwin-aarch64": { url: options["aarch64-url"], signature: readFileSync(options["aarch64-signature-file"], "utf8").trim() } },
};
writeFileSync(options.output, JSON.stringify(result, null, 2) + "\\n");
`,
  );

  executable(
    join(bin, "git"),
    `#!/bin/bash
set -euo pipefail
[[ "\${1:-}" == -C ]] && shift 2
case "\${1:-}" in
  ls-files|status|verify-tag) ;;
  branch) printf 'main\n' ;;
  rev-parse)
    case "\${*: -1}" in
      refs/tags/*) [[ "\${*: -1}" == *'^{'* ]] && printf '${commit}\n' || printf 'abcdef1234567890abcdef1234567890abcdef12\n' ;;
      *) printf '${commit}\n' ;;
    esac ;;
  ls-remote) printf 'abcdef1234567890abcdef1234567890abcdef12\trefs/tags/%s\n${commit}\trefs/tags/%s^{}\n' "\${MOCK_TAG}" "\${MOCK_TAG}" ;;
  *) exit 64 ;;
esac
exit 0
`,
  );

  executable(
    join(bin, "aws"),
    `#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const args = process.argv.slice(2);
const value = (flag) => { const i = args.indexOf(flag); return i < 0 ? undefined : args[i + 1]; };
const keyPath = (key) => path.join(process.env.MOCK_STORE, key);
const writeObject = (key, source, publish = true) => {
  const target = keyPath(key); fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.copyFileSync(source, target);
  const bytes = fs.readFileSync(target);
  fs.writeFileSync(target + ".etag", "etag-" + require("node:crypto").createHash("sha256").update(bytes).digest("hex").slice(0, 16));
  if (publish) {
    const publicTarget = path.join(process.env.MOCK_PUBLIC_STORE, key);
    fs.mkdirSync(path.dirname(publicTarget), { recursive: true });
    fs.copyFileSync(source, publicTarget);
  }
};
if (args[0] === "s3" && args[1] === "cp") {
  const cpKey = new URL(args[3]).pathname.slice(1);
  const stableHook = path.join(process.env.MOCK_STORE, ".stable-hook");
  if (cpKey === "macos/latest/Dakia-Apple-Silicon.dmg" && process.env.MOCK_LATEST_MODE === "stable-write-fails" && !fs.existsSync(stableHook)) {
    fs.writeFileSync(stableHook, "used"); process.exit(1);
  }
  writeObject(cpKey, args[2]); process.exit(0);
}
const operation = args[1];
const key = value("--key");
if (operation === "get-object") {
  const source = keyPath(key); if (!fs.existsSync(source)) process.exit(1);
  const destination = args.at(-1); fs.copyFileSync(source, destination);
  process.stdout.write(fs.readFileSync(source + ".etag", "utf8")); process.exit(0);
}
if (operation !== "put-object") process.exit(64);
const source = value("--body");
const current = keyPath(key);
const currentEtag = fs.existsSync(current + ".etag") ? fs.readFileSync(current + ".etag", "utf8") : undefined;
const ifNone = value("--if-none-match");
const ifMatch = value("--if-match");
if ((ifNone === "*" && fs.existsSync(current)) || (ifMatch !== undefined && ifMatch !== currentEtag)) process.exit(1);
const hook = path.join(process.env.MOCK_STORE, ".latest-hook");
if (key === "macos/latest/latest.json" && !fs.existsSync(hook)) {
  if (process.env.MOCK_LATEST_MODE === "fail-once") { fs.writeFileSync(hook, "used"); process.exit(1); }
  if (process.env.MOCK_LATEST_MODE === "same-candidate-wins") { writeObject(key, source); fs.writeFileSync(hook, "used"); process.exit(1); }
  if (process.env.MOCK_LATEST_MODE === "different-candidate-wins") {
    writeObject(key, process.env.MOCK_WINNER_MANIFEST);
    writeObject("macos/v0.4.2/Dakia-Apple-Silicon.dmg", process.env.MOCK_WINNER_DMG);
    fs.writeFileSync(hook, "used"); process.exit(1);
  }
  if (process.env.MOCK_LATEST_MODE === "different-candidate-wins-public-lags") {
    writeObject(key, process.env.MOCK_WINNER_MANIFEST, false);
    writeObject("macos/v0.4.2/Dakia-Apple-Silicon.dmg", process.env.MOCK_WINNER_DMG);
    fs.writeFileSync(hook, "used"); process.exit(1);
  }
  if (process.env.MOCK_LATEST_MODE === "crash-before-manifest-cas") {
    fs.writeFileSync(hook, "used"); process.kill(process.ppid, "SIGKILL"); process.exit(1);
  }
}
writeObject(key, source);
`,
  );

  executable(
    join(bin, "curl"),
    `#!/bin/bash
set -euo pipefail
output= url=
while [[ "\$#" -gt 0 ]]; do
  case "\$1" in
    --output) output="\$2"; shift 2 ;;
    --write-out|--connect-timeout|--max-time|--retry|--retry-delay|--retry-max-time|--proto) shift 2 ;;
    --silent|--show-error|--location) shift ;;
    *) url="\$1"; shift ;;
  esac
done
key="\${url#https://downloads.dakiamail.com/}"; key="\${key%%\\?*}"
source="\${MOCK_PUBLIC_STORE}/\$key"
if [[ -f "\$source" ]]; then cp "\$source" "\$output"; printf '200'; else : >"\$output"; printf '404'; fi
`,
  );

  executable(
    join(bin, "tar"),
    `#!/bin/bash
set -euo pipefail
destination=
while [[ "\$#" -gt 0 ]]; do [[ "\$1" == -C ]] && { destination="\$2"; break; }; shift; done
mkdir -p "\$destination/Dakia.app/Contents"
cp "\${MOCK_INFO_PLIST}" "\$destination/Dakia.app/Contents/Info.plist"
`,
  );
  return { fixture, root, scripts, bin, store, publicStore };
}

function candidate(harness, version, bytes = `dmg-${version}\n`) {
  const tag = `v${version}`;
  const assets = join(harness.fixture, `assets-${version}`);
  const docs = join(harness.root, "docs", "releases");
  mkdirSync(assets);
  mkdirSync(docs, { recursive: true });
  const files = {
    [`Dakia_${version}_aarch64.dmg`]: bytes,
    "Dakia-aarch64.app.tar.gz": `archive-${version}\n`,
    "Dakia-aarch64.app.tar.gz.sig": `signature-${version}\n`,
  };
  for (const [name, content] of Object.entries(files))
    writeFileSync(join(assets, name), content);
  writeFileSync(join(assets, "release-notes.md"), `Notes ${version}\n`);
  writeFileSync(join(assets, "source-commit.txt"), `${commit}\n`);
  writeFileSync(join(docs, `${tag}.md`), `Notes ${version}\n`);
  writeFileSync(
    join(assets, "SHA256SUMS.txt"),
    Object.entries(files)
      .map(([name, content]) => `${hash(content)}  ${name}`)
      .join("\n") + "\n",
  );
  const plist = join(harness.fixture, `Info-${version}.plist`);
  writeFileSync(
    plist,
    `<?xml version="1.0"?><plist><dict><key>CFBundleShortVersionString</key><string>${version}</string></dict></plist>`,
  );
  return {
    tag,
    version,
    assets,
    plist,
    dmg: files[`Dakia_${version}_aarch64.dmg`],
  };
}

function run(harness, release, mode = "normal", extraEnvironment = {}) {
  return spawnSync(
    join(harness.scripts, "publish-release-to-r2.sh"),
    [release.tag, release.assets],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        PATH: `${harness.bin}:${process.env.PATH}`,
        CLOUDFLARE_ACCOUNT_ID: "fixture-account",
        MOCK_INFO_PLIST: release.plist,
        MOCK_LATEST_MODE: mode,
        MOCK_PUBLIC_STORE: harness.publicStore,
        MOCK_STORE: harness.store,
        MOCK_TAG: release.tag,
        R2_ACCESS_KEY_ID: "fixture-access",
        R2_SECRET_ACCESS_KEY: "fixture-secret",
        ...extraEnvironment,
      },
    },
  );
}

function currentVersion(store) {
  return JSON.parse(
    readFileSync(objectPath(store, "macos/latest/latest.json"), "utf8"),
  ).version;
}

test("a competing candidate loses before either mutable release object changes", () => {
  const harness = createHarness();
  const release = candidate(harness, "0.4.1");
  try {
    putVisibleInitial(harness, "macos/latest/latest.json", manifest("0.4.0"));
    putVisibleInitial(
      harness,
      "macos/latest/Dakia-Apple-Silicon.dmg",
      "dmg-0.4.0\n",
    );
    putInitial(
      harness.store,
      "macos/latest/publication.json",
      `${JSON.stringify({ tag: "v0.4.2", version: "0.4.2", source: "other" })}\n`,
    );
    const result = run(harness, release);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /Another incomplete release owns/);
    assert.equal(currentVersion(harness.store), "0.4.0");
    assert.equal(
      readFileSync(
        objectPath(harness.store, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      "dmg-0.4.0\n",
    );
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("a stable alias write failure leaves latest.json on the preceding release", () => {
  const harness = createHarness();
  const release = candidate(harness, "0.4.1");
  try {
    putVisibleInitial(harness, "macos/latest/latest.json", manifest("0.4.0"));
    putVisibleInitial(
      harness,
      "macos/latest/Dakia-Apple-Silicon.dmg",
      "dmg-0.4.0\n",
    );
    const failed = run(harness, release, "stable-write-fails");
    assert.notEqual(failed.status, 0);
    assert.equal(currentVersion(harness.store), "0.4.0");
    assert.equal(
      readFileSync(
        objectPath(harness.store, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      "dmg-0.4.0\n",
      failed.stderr,
    );
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("a manifest CAS loser repairs the stable alias to a different winner before failing", () => {
  const harness = createHarness();
  const release = candidate(harness, "0.4.1");
  const winnerManifest = join(harness.fixture, "winner-latest.json");
  const winnerDmg = join(harness.fixture, "winner.dmg");
  try {
    putVisibleInitial(harness, "macos/latest/latest.json", manifest("0.4.0"));
    putVisibleInitial(
      harness,
      "macos/latest/Dakia-Apple-Silicon.dmg",
      "dmg-0.4.0\n",
    );
    writeFileSync(winnerManifest, manifest("0.4.2"));
    writeFileSync(winnerDmg, "dmg-0.4.2\n");
    const result = run(harness, release, "different-candidate-wins", {
      MOCK_WINNER_DMG: winnerDmg,
      MOCK_WINNER_MANIFEST: winnerManifest,
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /repaired its stable DMG alias/);
    assert.equal(currentVersion(harness.store), "0.4.2");
    assert.equal(
      readFileSync(
        objectPath(harness.store, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      "dmg-0.4.2\n",
    );
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("a lagging public manifest cannot make a CAS loser repair to the stale release", () => {
  const harness = createHarness();
  const release = candidate(harness, "0.4.1");
  const winnerManifest = join(harness.fixture, "lagged-winner-latest.json");
  const winnerDmg = join(harness.fixture, "lagged-winner.dmg");
  try {
    putVisibleInitial(harness, "macos/latest/latest.json", manifest("0.4.0"));
    putVisibleInitial(
      harness,
      "macos/latest/Dakia-Apple-Silicon.dmg",
      "dmg-0.4.0\n",
    );
    writeFileSync(winnerManifest, manifest("0.4.2"));
    writeFileSync(winnerDmg, "dmg-0.4.2\n");
    const result = run(
      harness,
      release,
      "different-candidate-wins-public-lags",
      {
        MOCK_WINNER_DMG: winnerDmg,
        MOCK_WINNER_MANIFEST: winnerManifest,
      },
    );
    assert.notEqual(result.status, 0);
    assert.equal(currentVersion(harness.store), "0.4.2");
    assert.equal(currentVersion(harness.publicStore), "0.4.0");
    assert.equal(
      readFileSync(
        objectPath(harness.store, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      "dmg-0.4.2\n",
      `publisher must pair the stable alias to the authenticated winner, not the lagging public manifest: ${result.stderr}`,
    );
    assert.equal(
      readFileSync(
        objectPath(harness.publicStore, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      "dmg-0.4.2\n",
    );
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("a conditional-write loser accepts only the same winning candidate and stays consistent", () => {
  const harness = createHarness();
  const release = candidate(harness, "0.4.1");
  try {
    putVisibleInitial(harness, "macos/latest/latest.json", manifest("0.4.0"));
    putVisibleInitial(
      harness,
      "macos/latest/Dakia-Apple-Silicon.dmg",
      "dmg-0.4.0\n",
    );
    const result = run(harness, release, "same-candidate-wins");
    assert.equal(result.status, 0, result.stderr);
    assert.equal(currentVersion(harness.store), "0.4.1");
    assert.equal(
      readFileSync(
        objectPath(harness.store, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      release.dmg,
    );
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});

test("the exact candidate safely resumes after stopping between stable verification and final manifest CAS", () => {
  const harness = createHarness();
  const release = candidate(harness, "0.4.1");
  try {
    putVisibleInitial(harness, "macos/latest/latest.json", manifest("0.4.0"));
    putVisibleInitial(
      harness,
      "macos/latest/Dakia-Apple-Silicon.dmg",
      "dmg-0.4.0\n",
    );
    const interrupted = run(harness, release, "crash-before-manifest-cas");
    assert.notEqual(interrupted.status, 0);
    assert.equal(currentVersion(harness.store), "0.4.0");
    assert.equal(
      readFileSync(
        objectPath(harness.store, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      release.dmg,
    );
    const resumed = run(harness, release, "crash-before-manifest-cas");
    assert.equal(resumed.status, 0, resumed.stderr);
    assert.equal(currentVersion(harness.store), "0.4.1");
    assert.equal(
      readFileSync(
        objectPath(harness.store, "macos/latest/Dakia-Apple-Silicon.dmg"),
        "utf8",
      ),
      release.dmg,
    );
  } finally {
    rmSync(harness.fixture, { recursive: true, force: true });
  }
});
