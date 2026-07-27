import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const assetDirectory = join(
  repositoryRoot,
  "apps",
  "desktop",
  "src-tauri",
  "resources",
  "email-classifier-v2",
);
const manifest = JSON.parse(
  await readFile(join(assetDirectory, "MANIFEST.json"), "utf8"),
);

for (const [filename, expected] of Object.entries(manifest.files)) {
  if (
    typeof expected !== "string" ||
    !/^[a-f0-9]{64}$/.test(expected) ||
    filename.includes("/") ||
    filename.includes("\\") ||
    filename === "." ||
    filename === ".."
  ) {
    throw new Error(`Invalid classifier manifest entry: ${filename}`);
  }
  const bytes = await readFile(join(assetDirectory, filename));
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== expected) {
    throw new Error(
      `Classifier asset hash mismatch for ${filename}: expected ${expected}, got ${actual}`,
    );
  }
}

console.log(
  `Verified ${Object.keys(manifest.files).length} classifier assets against MANIFEST.json`,
);
