import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  mkdtemp,
  mkdir,
  readdir,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { seedMailSafetyFuzz } from "./seed-mail-safety-fuzz.mjs";

test("nested raw messages seed both real fuzz inputs without basename collisions", async () => {
  const root = await mkdtemp(join(tmpdir(), "dakia-fuzz-seeds-"));
  try {
    const input = join(root, "input");
    await mkdir(join(input, "obsolete-receive"), { recursive: true });
    await writeFile(join(input, "same.eml"), "Subject: outer\r\n\r\nouter\r\n");
    await writeFile(
      join(input, "obsolete-receive", "same.eml"),
      "Subject: nested\r\n\r\nnested\r\n",
    );
    await writeFile(join(input, "ignored.json"), "{}");
    const first = join(root, "first");
    const second = join(root, "second");
    await mkdir(first);
    await mkdir(second);
    const result = await seedMailSafetyFuzz(input, first);
    assert.equal(result.seedCount, 2);
    assert.equal(new Set(result.seeds.map((seed) => seed.seedName)).size, 2);
    assert.deepEqual(result.seeds.map((seed) => seed.sourcePath).sort(), [
      "obsolete-receive/same.eml",
      "same.eml",
    ]);
    for (const seed of result.seeds) {
      const bytes = await readFile(join(input, seed.sourcePath));
      assert.equal(seed.bytes, bytes.length);
      assert.equal(
        seed.sha256,
        createHash("sha256").update(bytes).digest("hex"),
      );
    }
    for (const target of result.targets) {
      assert.equal((await readdir(join(first, target))).length, 2);
      for (const seed of result.seeds) {
        assert.deepEqual(
          await readFile(join(first, target, seed.seedName)),
          await readFile(join(input, seed.sourcePath)),
        );
      }
    }
    assert.deepEqual(
      (await seedMailSafetyFuzz(input, second)).seeds,
      result.seeds,
    );
    assert.deepEqual(
      JSON.parse(await readFile(join(first, "seed-manifest.json"), "utf8")),
      result,
    );
    const cli = join(root, "cli");
    await mkdir(cli);
    const child = spawnSync(
      process.execPath,
      [
        fileURLToPath(new URL("./seed-mail-safety-fuzz.mjs", import.meta.url)),
        input,
        cli,
      ],
      { encoding: "utf8", timeout: 5000 },
    );
    assert.equal(child.status, 0, child.stderr);
    const manifestBytes = await readFile(join(cli, "seed-manifest.json"));
    const digest = createHash("sha256").update(manifestBytes).digest("hex");
    assert.match(child.stdout, /Seeded 2 recursive MIME fixtures/);
    assert.ok(child.stdout.includes(`Seed manifest SHA-256: ${digest}\n`));
    await assert.rejects(seedMailSafetyFuzz(input, first), /must be empty/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("a linked source or target cannot redirect corpus reads or writes", async () => {
  const root = await mkdtemp(join(tmpdir(), "dakia-fuzz-seed-links-"));
  try {
    const input = join(root, "input");
    const output = join(root, "output");
    const other = join(root, "other");
    await mkdir(input);
    await mkdir(output);
    await mkdir(other);
    await writeFile(join(other, "private.eml"), "fictional outside canary");
    await symlink(join(other, "private.eml"), join(input, "linked.eml"));
    await assert.rejects(
      seedMailSafetyFuzz(input, output),
      /cannot contain symlinks/,
    );
    assert.deepEqual(await readdir(join(output, "mime_budget")), []);
    await rm(join(input, "linked.eml"));
    await writeFile(join(input, "valid.eml"), "fictional seed");
    await rm(join(output, "mime_budget"), { recursive: true });
    await symlink(other, join(output, "mime_budget"));
    await assert.rejects(
      seedMailSafetyFuzz(input, output),
      /must be a real directory/,
    );
    assert.deepEqual(await readdir(other), ["private.eml"]);
    await assert.rejects(
      seedMailSafetyFuzz(input, input),
      /outside the source/,
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
