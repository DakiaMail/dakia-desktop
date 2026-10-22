import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  readdir,
  readFile,
  realpath,
  writeFile,
} from "node:fs/promises";
import { basename, join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

export async function seedMailSafetyFuzz(input, output) {
  const source = await realpath(input);
  const destination = await realpath(output);
  if (destination === source || destination.startsWith(`${source}/`)) {
    throw new Error("Fuzz output must be outside the source corpus");
  }
  const targets = ["mime_budget", "attachment_metadata"];
  for (const target of targets) {
    const directory = join(destination, target);
    await mkdir(directory, { recursive: true, mode: 0o700 });
    if (
      !(await lstat(directory)).isDirectory() ||
      (await lstat(directory)).isSymbolicLink()
    ) {
      throw new Error("Fuzz target must be a real directory");
    }
    if ((await readdir(directory)).length) {
      throw new Error("Fuzz target must be empty before seeding");
    }
  }
  const fixtures = [];
  async function visit(directory) {
    for (const entry of (
      await readdir(directory, { withFileTypes: true })
    ).sort((a, b) => a.name.localeCompare(b.name, "en"))) {
      const path = join(directory, entry.name);
      if (entry.isSymbolicLink())
        throw new Error("Fuzz source cannot contain symlinks");
      if (entry.isDirectory()) await visit(path);
      else if (entry.isFile() && entry.name.endsWith(".eml"))
        fixtures.push(path);
    }
  }
  await visit(source);
  if (!fixtures.length) throw new Error("No MIME seeds found");
  const seeds = [];
  for (const path of fixtures) {
    const sourcePath = relative(source, path);
    const bytes = await readFile(path);
    const seedName = `${sha256(sourcePath)}-${basename(path)}`;
    for (const target of targets) {
      await writeFile(join(destination, target, seedName), bytes, {
        flag: "wx",
        mode: 0o600,
      });
    }
    seeds.push({
      sourcePath,
      seedName,
      bytes: bytes.length,
      sha256: sha256(bytes),
    });
  }
  const manifest = {
    schemaVersion: 1,
    source,
    targets,
    seedCount: seeds.length,
    seeds,
  };
  await writeFile(
    join(destination, "seed-manifest.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
    { flag: "wx", mode: 0o600 },
  );
  return manifest;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  try {
    if (process.argv.length !== 4)
      throw new Error("Use source-corpus output-root");
    const manifest = await seedMailSafetyFuzz(process.argv[2], process.argv[3]);
    console.log(
      `Seeded ${manifest.seedCount} recursive MIME fixtures into both fuzz targets`,
    );
    console.log(
      `Seed manifest SHA-256: ${sha256(`${JSON.stringify(manifest, null, 2)}\n`)}`,
    );
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
