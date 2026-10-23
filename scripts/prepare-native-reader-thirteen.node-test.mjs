#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFile, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import {
  requireExternalOutputPath,
  resolveExternalOutputPath,
  transformHarness,
} from "./prepare-native-reader-thirteen.mjs";

const sourceRoot = resolve(process.argv[2] ?? process.cwd());
const stock = await readFile(
  `${sourceRoot}/scripts/native-provider-reader-acceptance.sh`,
  "utf8",
);
const prepareSource = await readFile(
  new URL("./prepare-native-reader-thirteen.mjs", import.meta.url),
  "utf8",
);
assert.match(prepareSource, /frozen source manifest omits native Reader input/);
assert.match(prepareSource, /\.\.\.FIXTURES\.map/);
const manifest = {
  algorithm: "sha256-path-and-content-manifest",
  digest: "a".repeat(64),
  fileCount: 999,
  files: Array(999),
};
const scriptRoot = dirname(new URL(import.meta.url).pathname);
const overlayPath = join(scriptRoot, "native-reader-thirteen-overlay.mjs");
const seederPath = join(scriptRoot, "seed-native-reader-thirteen-fixtures.mjs");
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const overlaySha256 = sha256(await readFile(overlayPath));
const seederSha256 = sha256(await readFile(seederPath));
const transformOptions = {
  sourceRoot,
  manifestPath: join(tmpdir(), "future-frozen-source-manifest.json"),
  manifestSha256: "b".repeat(64),
  manifest,
  overlayPath,
  overlaySourcePath: "scripts/native-reader-thirteen-overlay.mjs",
  overlaySha256,
  seederPath,
  seederSourcePath: "scripts/seed-native-reader-thirteen-fixtures.mjs",
  seederSha256,
};
const generated = transformHarness(stock, {
  ...transformOptions,
});
assert.match(generated, /native-reader-thirteen-inner-tls/);
assert.match(generated, /seed-native-reader-thirteen-fixtures\.mjs/);
assert.match(generated, /set\(range\(1, 17\)\)/);
assert.match(generated, /count != \[b'16'\]/);
assert.match(generated, /native_reader_thirteen_overlay_pid=\$!/);
assert.match(
  generated,
  /stop_owned_child "\$native_reader_thirteen_overlay_pid"/,
);
assert.match(
  generated,
  /cannot be combined with the thirteen-fixture Reader overlay/,
);
assert.equal(
  generated.includes(
    "Fictional provider fixture accepted normal and malformed MIME messages.",
  ),
  false,
);
assert.match(
  generated,
  /Fictional provider fixture and native Reader thirteen-fixture proxies are ready/,
);
assert.equal(
  generated.includes(
    "Fictional provider fixture and attachment interruption proxies are ready.",
  ),
  false,
);
const inner = generated.indexOf('node "$owned_overlay_run"');
const outer = generated.indexOf(
  'node "$repo_root/scripts/native-fictional-imap-proxy.mjs"',
  inner,
);
assert(
  inner > 0 && outer > inner,
  "stock controlled proxy must wrap the owned inner TLS overlay",
);
assert(generated.includes('\\"imapProxyPid\\": $proxy_pid'));
assert(generated.includes('\\"imapBackendPort\\": $backend_imap_port'));
assert.match(
  generated,
  /native-fictional-imap-proxy\.mjs --control-root .* --action disconnect/,
);
assert.match(generated, /frozen_source_digest="a{64}"/);
assert.match(generated, /frozen_source_file_count=999/);
assert.match(generated, /NATIVE_READER_THIRTEEN_SOURCE_BINDING/);
assert.match(generated, /NATIVE_READER_COPY_OWNED_INPUTS/);
assert.match(
  generated,
  /copyFile\(source, destination, constants\.COPYFILE_EXCL\)/,
);
assert.match(generated, /node "\$owned_seeder_run"/);
assert.match(generated, /native-reader-thirteen-owned-inputs\.json/);
assert.match(
  generated,
  /sourcePath.*scripts\/native-reader-thirteen-overlay\.mjs/,
);
const syntax = spawnSync("bash", ["-n"], {
  input: generated,
  encoding: "utf8",
});
assert.equal(syntax.status, 0, syntax.stderr);
const ownedBindingEnd =
  generated.indexOf("\nNATIVE_READER_OWNED_INPUTS\n") +
  "\nNATIVE_READER_OWNED_INPUTS\n".length;
assert(ownedBindingEnd > 0);
const ownedBinding = generated.slice(0, ownedBindingEnd);
const validOwnedBinding = spawnSync("bash", ["-s"], {
  input: ownedBinding,
  encoding: "utf8",
});
assert.equal(validOwnedBinding.status, 0, validOwnedBinding.stderr);
const mutatedOverlay = join(scriptRoot, ".mutated-overlay.mjs");
await copyFile(overlayPath, mutatedOverlay);
await writeFile(mutatedOverlay, "\n// mutation\n", { flag: "a" });
try {
  const changedInput = spawnSync("bash", ["-s"], {
    input: ownedBinding.replaceAll(overlayPath, mutatedOverlay),
    encoding: "utf8",
  });
  assert.notEqual(changedInput.status, 0);
  assert.match(changedInput.stderr, /owned overlay changed after review/);
} finally {
  await rm(mutatedOverlay, { force: true });
}
const changedHash = spawnSync("bash", ["-s"], {
  input: ownedBinding.replace(overlaySha256, "0".repeat(64)),
  encoding: "utf8",
});
assert.notEqual(changedHash.status, 0);
assert.match(changedHash.stderr, /owned overlay changed after review/);
assert.throws(
  () => requireExternalOutputPath(sourceRoot, join(sourceRoot, "generated.sh")),
  /outside the source checkout/,
);
assert.doesNotThrow(() =>
  requireExternalOutputPath(
    sourceRoot,
    join(tmpdir(), "native-reader-thirteen.sh"),
  ),
);
assert.equal(
  await resolveExternalOutputPath(
    sourceRoot,
    join(tmpdir(), "native-reader-thirteen.sh"),
  ),
  join(await realpath(tmpdir()), "native-reader-thirteen.sh"),
);

for (const [label, before, after] of [
  [
    "source anchor",
    'repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"',
    "repo_root=changed",
  ],
  [
    "proxy anchor",
    'node "$repo_root/scripts/native-fictional-imap-proxy.mjs" \\\n  --root "$run_root" --backend-port "$backend_imap_port" >"$run_root/imap-proxy.log" 2>&1 &',
    "node changed",
  ],
  [
    "seed anchor",
    "PYTHON\n\nattachment_interruption_binding=''",
    "PYTHON\n\nchanged=''",
  ],
]) {
  assert.throws(
    () =>
      transformHarness(stock.replace(before, after), {
        ...transformOptions,
      }),
    new RegExp(`anchor is not unique: .*${label.split(" ")[0]}`, "i"),
  );
}
assert.throws(
  () =>
    transformHarness(
      `${stock}\n${'fixture_pid=""\nproxy_pid=""\nattachment_interruption_pid=""'}\n`,
      {
        ...transformOptions,
      },
    ),
  /anchor is not unique: overlay process variable/,
);
console.log(
  "native Reader thirteen-fixture generator: layering, cleanup, read-neutral UID range, future source binding, prepare-only ordering, and anchor sensitivity passed",
);
