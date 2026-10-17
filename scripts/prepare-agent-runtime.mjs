#!/usr/bin/env node

import { createHash } from "node:crypto";
import { lstatSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join, relative, resolve, sep } from "node:path";

const manifestName = "MANIFEST.json";
const comparePaths = (left, right) => left.localeCompare(right, "en");
const requiredBasePaths = [
  "dist/cli.js",
  "licenses/npm-production.json",
  "licenses/pi/LICENSE",
  "package-lock.json",
  "package.json",
];
const supplementalLicenses = new Map([
  [
    "@aws-sdk/credential-provider-http@3.972.76",
    ["licenses/npm-supplemental/Apache-2.0.txt"],
  ],
  [
    "@aws-sdk/credential-provider-login@3.972.81",
    ["licenses/npm-supplemental/Apache-2.0.txt"],
  ],
  [
    "@aws-sdk/nested-clients@3.997.48",
    ["licenses/npm-supplemental/Apache-2.0.txt"],
  ],
  ["@earendil-works/pi-agent-core@1.3.0", ["licenses/pi/LICENSE"]],
  ["@earendil-works/pi-ai@1.3.0", ["licenses/pi/LICENSE"]],
  ["@earendil-works/pi-codemode@1.3.0", ["licenses/pi/LICENSE"]],
  ["@earendil-works/pi-coding-agent@1.3.0", ["licenses/pi/LICENSE"]],
  ["@earendil-works/pi-mcp@1.3.0", ["licenses/pi/LICENSE"]],
  ["@earendil-works/pi-telemetry@1.3.0", ["licenses/pi/LICENSE"]],
  ["@earendil-works/pi-tui@1.3.0", ["licenses/pi/LICENSE"]],
  ["@tokenizer/token@0.3.0", ["node_modules/@tokenizer/token/README.md"]],
  ["data-uri-to-buffer@4.0.1", ["node_modules/data-uri-to-buffer/README.md"]],
  ["degenerator@5.0.1", ["node_modules/degenerator/README.md"]],
  [
    "proxy-agent-negotiate@1.1.0",
    ["licenses/npm-supplemental/proxy-agent-negotiate-MIT.txt"],
  ],
  [
    "standardwebhooks@1.1.1",
    [
      "node_modules/standardwebhooks/package.json",
      "node_modules/standardwebhooks/README.md",
      "licenses/npm-supplemental/Apache-2.0.txt",
    ],
  ],
]);
const licenseReviews = new Map([
  [
    "standardwebhooks@1.1.1",
    "Upstream review required: the npm package declares MIT without a standalone license, while its linked source repository distributes Apache-2.0 at the root.",
  ],
]);
const pinnedNode = {
  "darwin-arm64": {
    archive: "node-v22.23.2-darwin-arm64.tar.gz",
    archiveSha256:
      "61130f394c1630d211dd50aecc4353d379480f36d3ac913cd85dbba1aed585c6",
    minimumMacOS: "11.0",
  },
  "darwin-x64": {
    archive: "node-v22.23.2-darwin-x64.tar.gz",
    archiveSha256:
      "58e99022c2ff89395576cc7fd4d98cea24bb68081475d5f88b801ee8729fb026",
    minimumMacOS: "11.0",
  },
  "linux-arm64": {
    archive: "node-v22.23.2-linux-arm64.tar.gz",
    archiveSha256:
      "013b59cfd2819703a6f4a14ab891fc46fc2a4e3f5bcd92de3fb4929b43e35b30",
    minimumMacOS: null,
  },
  "linux-x64": {
    archive: "node-v22.23.2-linux-x64.tar.gz",
    archiveSha256:
      "b294a556e639d64338823920e5866c21c02741742d2e1529ee1a225c1ec9252a",
    minimumMacOS: null,
  },
  "win-arm64": {
    archive: "node-v22.23.2-win-arm64.zip",
    archiveSha256:
      "fec025a6da31757e3b6af84c5a1628e9d38442ca99a2161091d78f2fcfa35ef3",
    minimumMacOS: null,
  },
  "win-x64": {
    archive: "node-v22.23.2-win-x64.zip",
    archiveSha256:
      "1177b4137ba5adaa56354ae40f1080c7450e8ae09cecb47da459d1c52ac99f97",
    minimumMacOS: null,
  },
};

function requiredPaths(metadata) {
  return [
    ...requiredBasePaths,
    metadata?.platform === "win" ? "runtime/node.exe" : "runtime/node",
  ];
}

function fail(message) {
  console.error(message);
  process.exit(1);
}

function normalizedRelative(root, path) {
  return relative(root, path).split(sep).join("/");
}

function collectFiles(root, directory = root) {
  const files = [];
  for (const entry of readdirSync(directory, { withFileTypes: true }).sort(
    (a, b) => comparePaths(a.name, b.name),
  )) {
    const path = join(directory, entry.name);
    const relativePath = normalizedRelative(root, path);
    if (entry.isSymbolicLink()) {
      fail(`Agent runtime contains a forbidden symbolic link: ${relativePath}`);
    }
    if (entry.isDirectory()) {
      files.push(...collectFiles(root, path));
    } else if (entry.isFile() && relativePath !== manifestName) {
      files.push(relativePath);
    } else if (!entry.isFile()) {
      fail(
        `Agent runtime contains an unsupported filesystem entry: ${relativePath}`,
      );
    }
  }
  return files.sort(comparePaths);
}

function productionPackageRoots(nodeModulesRoot) {
  const packages = [];
  function visit(directory) {
    const currentPackages = [];
    for (const entry of readdirSync(directory, { withFileTypes: true }).sort(
      (a, b) => comparePaths(a.name, b.name),
    )) {
      if (!entry.isDirectory() || entry.name === ".bin") continue;
      const path = join(directory, entry.name);
      if (entry.name.startsWith("@")) {
        for (const scoped of readdirSync(path, { withFileTypes: true }).sort(
          (a, b) => comparePaths(a.name, b.name),
        )) {
          if (scoped.isDirectory())
            currentPackages.push(join(path, scoped.name));
        }
      } else {
        currentPackages.push(path);
      }
    }
    for (const packageRoot of currentPackages) {
      packages.push(packageRoot);
      const nested = join(packageRoot, "node_modules");
      try {
        if (lstatSync(nested).isDirectory()) visit(nested);
      } catch {}
    }
  }
  visit(nodeModulesRoot);
  return packages.sort(comparePaths);
}

function inventory(root) {
  const entries = productionPackageRoots(join(root, "node_modules")).map(
    (packageRoot) => {
      const packageJson = JSON.parse(
        readFileSync(join(packageRoot, "package.json"), "utf8"),
      );
      const license = packageJson.license ?? packageJson.licenses;
      if (!license) {
        fail(
          `Production dependency has no declared license: ${packageJson.name}@${packageJson.version}`,
        );
      }
      const identity = `${packageJson.name}@${packageJson.version}`;
      const licenseFiles = readdirSync(packageRoot)
        .filter((name) => /^(licen[cs]e|copying|notice)(?:[._-]|$)/i.test(name))
        .sort(comparePaths)
        .map((name) => normalizedRelative(root, join(packageRoot, name)));
      licenseFiles.push(...(supplementalLicenses.get(identity) ?? []));
      const uniqueLicenseFiles = [...new Set(licenseFiles)].sort(comparePaths);
      if (uniqueLicenseFiles.length === 0) {
        fail(
          `Production dependency has no packaged license or audited supplemental notice: ${identity}`,
        );
      }
      for (const path of uniqueLicenseFiles) {
        try {
          if (!lstatSync(join(root, path)).isFile()) throw new Error();
        } catch {
          fail(
            `Production dependency license file is missing: ${identity}: ${path}`,
          );
        }
      }
      return {
        name: packageJson.name,
        version: packageJson.version,
        license,
        author: packageJson.author ?? null,
        repository: packageJson.repository ?? null,
        licenseFiles: uniqueLicenseFiles,
        review: licenseReviews.get(identity) ?? null,
      };
    },
  );
  entries.sort((left, right) =>
    comparePaths(
      `${left.name}@${left.version}`,
      `${right.name}@${right.version}`,
    ),
  );
  writeFileSync(
    join(root, "licenses", "npm-production.json"),
    `${JSON.stringify({ schemaVersion: 1, packages: entries }, null, 2)}\n`,
  );
}

function verifyLicenseInventory(root, files) {
  let inventory;
  try {
    inventory = JSON.parse(
      readFileSync(join(root, "licenses", "npm-production.json"), "utf8"),
    );
  } catch (error) {
    fail(
      `Cannot read production dependency license inventory: ${error.message}`,
    );
  }
  if (!Array.isArray(inventory.packages) || inventory.packages.length === 0) {
    fail("Production dependency license inventory is empty or invalid.");
  }
  for (const entry of inventory.packages) {
    const identity = `${entry.name}@${entry.version}`;
    if (!Array.isArray(entry.licenseFiles) || entry.licenseFiles.length === 0) {
      fail(`Production dependency has no packaged license notice: ${identity}`);
    }
    for (const path of entry.licenseFiles) {
      if (typeof path !== "string" || !(path in files)) {
        fail(
          `Production dependency license notice is absent from the manifest: ${identity}: ${String(path)}`,
        );
      }
    }
  }
}

function digest(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function packageIdentity(root) {
  let value;
  try {
    value = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  } catch (error) {
    fail(`Cannot read bundled agent package.json: ${error.message}`);
  }
  if (typeof value.name !== "string" || typeof value.version !== "string") {
    fail(
      "Bundled agent package.json must contain string name and version fields.",
    );
  }
  return { name: value.name, version: value.version };
}

function build(root, metadataPath) {
  const metadata = JSON.parse(readFileSync(metadataPath, "utf8"));
  writeManifest(root, metadata);
}

function writeManifest(root, metadata) {
  const files = Object.fromEntries(
    collectFiles(root).map((path) => [path, digest(join(root, path))]),
  );
  for (const path of requiredPaths(metadata)) {
    if (!(path in files)) {
      fail(`Agent runtime is missing required file: ${path}`);
    }
  }
  const manifest = {
    schemaVersion: 1,
    package: packageIdentity(root),
    node: metadata,
    files,
  };
  writeFileSync(
    join(root, manifestName),
    `${JSON.stringify(manifest, null, 2)}\n`,
    { mode: 0o644 },
  );
}

function refresh(root, allowedPathsFile) {
  if (!allowedPathsFile) {
    fail(
      "Refreshing an agent runtime manifest requires an allowed-paths file.",
    );
  }
  let manifest;
  try {
    manifest = JSON.parse(readFileSync(join(root, manifestName), "utf8"));
  } catch (error) {
    fail(`Cannot refresh agent runtime manifest: ${error.message}`);
  }
  if (manifest.schemaVersion !== 1 || typeof manifest.node !== "object") {
    fail("Agent runtime manifest has an unsupported schema.");
  }
  const allowedPaths = new Set(
    readFileSync(allowedPathsFile, "utf8").split("\n").filter(Boolean),
  );
  const actualFiles = collectFiles(root);
  const expectedFiles = Object.keys(manifest.files).sort(comparePaths);
  if (JSON.stringify(actualFiles) !== JSON.stringify(expectedFiles)) {
    fail("Agent runtime file set changed while signing.");
  }
  for (const path of expectedFiles) {
    const actual = digest(join(root, path));
    if (actual !== manifest.files[path] && !allowedPaths.has(path)) {
      fail(`Unsigned agent runtime file changed while signing: ${path}`);
    }
    if (allowedPaths.has(path)) manifest.files[path] = actual;
  }
  for (const path of allowedPaths) {
    if (!(path in manifest.files)) {
      fail(`Signed agent runtime path is absent from the manifest: ${path}`);
    }
  }
  writeFileSync(
    join(root, manifestName),
    `${JSON.stringify(manifest, null, 2)}\n`,
    { mode: 0o644 },
  );
}

function verify(root, expectedPlatform, expectedArchitecture) {
  const manifestPath = join(root, manifestName);
  let manifest;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  } catch (error) {
    fail(`Cannot read agent runtime manifest: ${error.message}`);
  }
  if (manifest.schemaVersion !== 1 || typeof manifest.files !== "object") {
    fail("Agent runtime manifest has an unsupported schema.");
  }
  const nodeKey = `${manifest.node?.platform}-${manifest.node?.architecture}`;
  const pinned = pinnedNode[nodeKey];
  if (!pinned || manifest.node?.version !== "22.23.2") {
    fail(
      "Agent runtime manifest does not identify a supported pinned Node.js runtime.",
    );
  }
  if (
    process.env.DAKIA_AGENT_RUNTIME_TESTING !== "1" &&
    (manifest.node.archive !== pinned.archive ||
      manifest.node.archiveSha256 !== pinned.archiveSha256 ||
      manifest.node.minimumMacOS !== pinned.minimumMacOS)
  ) {
    fail(
      "Agent runtime manifest does not match the pinned Node.js release metadata.",
    );
  }
  if (
    (expectedPlatform && manifest.node.platform !== expectedPlatform) ||
    (expectedArchitecture &&
      manifest.node.architecture !== expectedArchitecture)
  ) {
    fail(
      `Agent runtime target ${nodeKey} does not match expected ${expectedPlatform}-${expectedArchitecture}.`,
    );
  }
  const actualFiles = collectFiles(root);
  const expectedFiles = Object.keys(manifest.files).sort(comparePaths);
  if (JSON.stringify(actualFiles) !== JSON.stringify(expectedFiles)) {
    const missing = expectedFiles.filter((path) => !actualFiles.includes(path));
    const unexpected = actualFiles.filter(
      (path) => !expectedFiles.includes(path),
    );
    fail(
      `Agent runtime file set does not match its manifest.` +
        (missing.length ? ` Missing: ${missing.join(", ")}.` : "") +
        (unexpected.length ? ` Unexpected: ${unexpected.join(", ")}.` : ""),
    );
  }
  for (const path of expectedFiles) {
    const actual = digest(join(root, path));
    if (actual !== manifest.files[path]) {
      fail(`Agent runtime checksum mismatch: ${path}`);
    }
  }
  for (const path of requiredPaths(manifest.node)) {
    if (!(path in manifest.files)) {
      fail(`Agent runtime manifest is missing required file: ${path}`);
    }
  }
  verifyLicenseInventory(root, manifest.files);
  const runtimePath = join(
    root,
    "runtime",
    manifest.node?.platform === "win" ? "node.exe" : "node",
  );
  if (
    process.platform !== "win32" &&
    (lstatSync(runtimePath).mode & 0o111) === 0
  ) {
    fail("Bundled Node.js runtime is not executable.");
  }
  const identity = packageIdentity(root);
  if (
    identity.name !== manifest.package?.name ||
    identity.version !== manifest.package?.version
  ) {
    fail("Bundled agent package identity does not match its manifest.");
  }
}

const [command, rawRoot, metadataPath, expectedArchitecture] =
  process.argv.slice(2);
if (command === "sha256" && rawRoot) {
  process.stdout.write(`${digest(resolve(rawRoot))}\n`);
  process.exit(0);
}
if (
  !command ||
  !rawRoot ||
  !["build", "inventory", "refresh", "sha256", "verify"].includes(command)
) {
  fail(
    "Usage: prepare-agent-runtime.mjs build <runtime-dir> <node-metadata.json> | inventory <runtime-dir> | refresh <runtime-dir> <allowed-paths-file> | sha256 <file> | verify <runtime-dir> [expected-platform expected-architecture]",
  );
}
const root = resolve(rawRoot);
if (command === "build") {
  if (!metadataPath)
    fail("Building an agent runtime manifest requires Node metadata.");
  build(root, resolve(metadataPath));
} else if (command === "inventory") {
  inventory(root);
} else if (command === "refresh") {
  refresh(root, metadataPath && resolve(metadataPath));
} else {
  verify(root, metadataPath, expectedArchitecture);
}
