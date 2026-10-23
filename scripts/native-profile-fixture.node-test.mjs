import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  chmod,
  link,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  symlink,
  truncate,
  unlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import test from "node:test";

const run = promisify(execFile);
const tool = new URL("./native-profile-fixture.mjs", import.meta.url).pathname;
const sha256 = (value) => createHash("sha256").update(value).digest("hex");

async function fixture({ future = false, sourceMode = 0o700 } = {}) {
  const root = await mkdtemp(
    join(tmpdir(), "dakia-native-profile-fixture-test-"),
  );
  await chmod(root, 0o700);
  const source = join(root, "sealed");
  await mkdir(source, { mode: 0o700 });
  const files = future
    ? {
        ".dakia-startup-failure-profile": Buffer.from("fictional marker\n"),
        "dakia.db": Buffer.from("opaque fictional sqlite bytes\x00\x01"),
        "vault.key": Buffer.from("opaque fictional key bytes\x00\x02"),
      }
    : {
        ".dakia-owned-profile": Buffer.from("fictional marker\n"),
        "dakia.db": Buffer.from("opaque fictional sqlite bytes\x00\x01"),
        "dakia.db-shm": Buffer.from("opaque shm bytes\x00\x03"),
        "dakia.db-wal": Buffer.alloc(0),
        "vault.key": Buffer.from("opaque fictional key bytes\x00\x02"),
      };
  const entries = [];
  for (const [name, bytes] of Object.entries(files)) {
    const mode = name.startsWith(".") || name === "vault.key" ? 0o400 : 0o444;
    const path = join(source, name);
    await writeFile(path, bytes, { mode });
    await chmod(path, mode);
    entries.push({
      path: name,
      bytes: bytes.length,
      mode,
      sha256: sha256(bytes),
    });
  }
  const manifest = join(root, "fixture-manifest.json");
  await writeFile(
    manifest,
    `${JSON.stringify({
      schemaVersion: 1,
      contract: "dakia-native-profile-fixture-v1",
      ownedMarkers: Object.keys(files).filter((name) => name.startsWith(".")),
      expectedAbsentFiles: future ? ["dakia.db-wal"] : [],
      files: entries,
    })}\n`,
    { mode: 0o600 },
  );
  await chmod(manifest, 0o600);
  await chmod(source, sourceMode);
  return { root, source, manifest, files };
}

async function invoke(...arguments_) {
  return run(process.execPath, [tool, ...arguments_], { encoding: "utf8" });
}

test("prepares faithful opaque bytes in a new private temporary directory", async () => {
  const item = await fixture();
  const output = join(item.root, "prepared");
  const { stdout } = await invoke(
    "prepare",
    "--source",
    item.source,
    "--manifest",
    item.manifest,
    "--output-root",
    output,
  );
  const receipt = JSON.parse(stdout);
  assert.equal(receipt.operation, "prepare");
  assert.equal(receipt.sourceUnchangedAfterCopy, true);
  assert.equal((await lstat(output)).mode & 0o777, 0o700);
  assert.equal((await lstat(join(output, "profile"))).mode & 0o777, 0o700);
  for (const [name, bytes] of Object.entries(item.files)) {
    const copied = join(output, "profile", name);
    assert.deepEqual(await readFile(copied), bytes);
    assert.equal((await lstat(copied)).mode & 0o777, 0o600);
  }
  assert.equal((await lstat(join(output, "receipt.json"))).mode & 0o777, 0o600);
});

test("accepts sealed 0500 sources and rejects unsafe source directory modes", async () => {
  const readable = await fixture({ sourceMode: 0o500 });
  await invoke(
    "prepare",
    "--source",
    readable.source,
    "--manifest",
    readable.manifest,
    "--output-root",
    join(readable.root, "prepared"),
  );
  const unsafe = await fixture({ sourceMode: 0o755 });
  await assert.rejects(
    invoke(
      "prepare",
      "--source",
      unsafe.source,
      "--manifest",
      unsafe.manifest,
      "--output-root",
      join(unsafe.root, "prepared"),
    ),
    /sealed source directory mode is invalid/,
  );
});

test("rejects an existing output root and leaves its file untouched", async () => {
  const item = await fixture();
  const output = join(item.root, "occupied");
  await mkdir(output, { mode: 0o700 });
  await writeFile(join(output, "keep"), "existing", { mode: 0o600 });
  await assert.rejects(
    invoke(
      "prepare",
      "--source",
      item.source,
      "--manifest",
      item.manifest,
      "--output-root",
      output,
    ),
    /already exists/,
  );
  assert.equal(await readFile(join(output, "keep"), "utf8"), "existing");
});

test("rejects traversal, source symlinks, and source hard links", async () => {
  const item = await fixture();
  await assert.rejects(
    invoke(
      "prepare",
      "--source",
      item.source,
      "--manifest",
      item.manifest,
      "--output-root",
      join(item.root, "..", "..", "dakia-escape"),
    ),
    /OS temporary directory/,
  );
  const symlinked = await fixture();
  await unlink(join(symlinked.source, "vault.key"));
  await symlink(
    join(symlinked.source, "dakia.db"),
    join(symlinked.source, "vault.key"),
  );
  await assert.rejects(
    invoke(
      "prepare",
      "--source",
      symlinked.source,
      "--manifest",
      symlinked.manifest,
      "--output-root",
      join(symlinked.root, "out"),
    ),
    /regular file|canonical/,
  );
  const linked = await fixture();
  await unlink(join(linked.source, "dakia.db-shm"));
  await link(
    join(linked.source, "dakia.db"),
    join(linked.source, "dakia.db-shm"),
  );
  await assert.rejects(
    invoke(
      "prepare",
      "--source",
      linked.source,
      "--manifest",
      linked.manifest,
      "--output-root",
      join(linked.root, "out"),
    ),
    /regular file|differs from manifest/,
  );
});

test("rejects a source that differs from its sealed manifest", async () => {
  const item = await fixture();
  await chmod(join(item.source, "dakia.db"), 0o600);
  await writeFile(join(item.source, "dakia.db"), "mutated opaque bytes");
  await chmod(join(item.source, "dakia.db"), 0o444);
  await assert.rejects(
    invoke(
      "prepare",
      "--source",
      item.source,
      "--manifest",
      item.manifest,
      "--output-root",
      join(item.root, "out"),
    ),
    /differs from manifest/,
  );
});

test("future durable comparison preserves database key marker and absent WAL", async () => {
  const item = await fixture({ future: true });
  const output = join(item.root, "prepared");
  const prepared = JSON.parse(
    (
      await invoke(
        "prepare",
        "--source",
        item.source,
        "--manifest",
        item.manifest,
        "--output-root",
        output,
      )
    ).stdout,
  );
  const profile = join(output, "profile");
  const passed = JSON.parse(
    (
      await invoke(
        "compare",
        "--baseline",
        item.manifest,
        "--after",
        profile,
        "--policy",
        "future-profile-durable",
      )
    ).stdout,
  );
  assert.equal(passed.result, "passed");
  await writeFile(join(profile, "dakia.db"), "changed");
  await assert.rejects(
    invoke(
      "compare",
      "--baseline",
      item.manifest,
      "--after",
      profile,
      "--policy",
      "future-profile-durable",
    ),
    /durable file changed/,
  );
});

test("future durable comparison rejects WAL creation while generic only reports inventory", async () => {
  const item = await fixture({ future: true });
  const output = join(item.root, "prepared");
  const prepared = JSON.parse(
    (
      await invoke(
        "prepare",
        "--source",
        item.source,
        "--manifest",
        item.manifest,
        "--output-root",
        output,
      )
    ).stdout,
  );
  const profile = join(output, "profile");
  await writeFile(join(profile, "dakia.db-wal"), "unexpected", { mode: 0o600 });
  await assert.rejects(
    invoke(
      "compare",
      "--baseline",
      item.manifest,
      "--after",
      profile,
      "--policy",
      "future-profile-durable",
    ),
    /remain absent/,
  );
  const generic = JSON.parse(
    (
      await invoke(
        "compare",
        "--baseline",
        item.manifest,
        "--after",
        profile,
        "--policy",
        "generic",
      )
    ).stdout,
  );
  assert.equal(generic.result, "inventory-only");
  assert.equal(
    generic.inventory.find((entry) => entry.path === "dakia.db-wal")
      ?.recognized,
    false,
  );
});

test("comparison bounds entry count and aggregate bytes before hashing", async () => {
  const item = await fixture({ future: true });
  const output = join(item.root, "prepared");
  await invoke(
    "prepare",
    "--source",
    item.source,
    "--manifest",
    item.manifest,
    "--output-root",
    output,
  );
  const profile = join(output, "profile");
  for (let index = 0; index < 30; index += 1) {
    await writeFile(join(profile, `extra-${index}`), "x", { mode: 0o600 });
  }
  await assert.rejects(
    invoke(
      "compare",
      "--baseline",
      item.manifest,
      "--after",
      profile,
      "--policy",
      "generic",
    ),
    /file count is invalid/,
  );

  const second = await fixture({ future: true });
  const secondOutput = join(second.root, "prepared");
  await invoke(
    "prepare",
    "--source",
    second.source,
    "--manifest",
    second.manifest,
    "--output-root",
    secondOutput,
  );
  const secondProfile = join(secondOutput, "profile");
  for (let index = 0; index < 3; index += 1) {
    const path = join(secondProfile, `large-${index}`);
    await writeFile(path, "", { mode: 0o600 });
    await truncate(path, 300 * 1024 * 1024);
  }
  await assert.rejects(
    invoke(
      "compare",
      "--baseline",
      second.manifest,
      "--after",
      secondProfile,
      "--policy",
      "generic",
    ),
    /total size is invalid/,
  );
});
