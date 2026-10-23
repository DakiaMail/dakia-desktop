import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { verifyVendoredCrates } from "./verify-vendored-crates.mjs";

function hash(value) {
  return createHash("sha256").update(value).digest("hex");
}

function write(path, value) {
  mkdirSync(join(path, ".."), { recursive: true });
  writeFileSync(path, value);
}

function fixture({ approvedChange = false } = {}) {
  const root = mkdtempSync(join(tmpdir(), "dakia-vendor-integrity-"));
  const cargoToml = `[workspace]\nmembers = []\n\n[patch.crates-io]\nexample-crate = { path = "vendor/example-crate" }\n`;
  const upstreamCargo = `[package]\nname = "example-crate"\nversion = "1.2.3"\n`;
  const upstreamLib = "pub fn upstream() {}\n";
  const currentLib = approvedChange ? "pub fn dakia_patch() {}\n" : upstreamLib;
  write(join(root, "Cargo.toml"), cargoToml);
  write(join(root, "vendor/example-crate/Cargo.toml"), upstreamCargo);
  write(join(root, "vendor/example-crate/src/lib.rs"), currentLib);

  const manifest = {
    schemaVersion: 1,
    name: "example-crate",
    version: "1.2.3",
    vendorPath: "vendor/example-crate",
    registryChecksum: "a".repeat(64),
    upstreamRepository: "https://example.invalid/example-crate",
    upstreamRevision: "b".repeat(40),
    license: "MIT",
    review: { required: "independent-review", evidence: "fixture-review" },
    upstreamFiles: [
      { path: "Cargo.toml", sha256: hash(upstreamCargo) },
      { path: "src/lib.rs", sha256: hash(upstreamLib) },
    ],
    allowedChanges: approvedChange
      ? [
          {
            path: "src/lib.rs",
            upstreamSha256: hash(upstreamLib),
            patchedSha256: hash(currentLib),
            reason: "fixture patch",
          },
        ]
      : [],
    vendorOnlyFiles: [],
    omittedUpstreamFiles: [],
  };
  write(
    join(root, "testdata/vendored-crates/example-crate.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
  );
  const metadata = {
    packages: [
      {
        name: "example-crate",
        version: "1.2.3",
        source: null,
        manifest_path: join(root, "vendor/example-crate/Cargo.toml"),
      },
    ],
  };
  return { root, metadata };
}

test("accepts a path crate whose complete vendor tree matches its reviewed manifest", () => {
  const { root, metadata } = fixture({ approvedChange: true });
  try {
    const result = verifyVendoredCrates({ root, metadata });
    assert.equal(result.length, 1);
    assert.equal(result[0].name, "example-crate");
    assert.equal(result[0].version, "1.2.3");
    assert.match(result[0].manifestPath, /testdata\/vendored-crates\/example-crate\.json$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects an unlisted local source change instead of treating a version-only audit as sufficient", () => {
  const { root, metadata } = fixture();
  try {
    write(join(root, "vendor/example-crate/src/lib.rs"), "pub fn changed() {}\n");
    assert.throws(
      () => verifyVendoredCrates({ root, metadata }),
      /unapproved upstream file change: src\/lib\.rs/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects an unreviewed file added to a vendor tree", () => {
  const { root, metadata } = fixture();
  try {
    write(join(root, "vendor/example-crate/local-note.txt"), "not upstream\n");
    assert.throws(
      () => verifyVendoredCrates({ root, metadata }),
      /unexpected vendor-only file: local-note\.txt/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects metadata that resolves the crate from a registry instead of its reviewed path", () => {
  const { root, metadata } = fixture();
  try {
    metadata.packages[0].source = "registry+https://github.com/rust-lang/crates.io-index";
    assert.throws(
      () => verifyVendoredCrates({ root, metadata }),
      /must not include a competing registry or version-resolved package/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects a registry package that competes with the reviewed path package", () => {
  const { root, metadata } = fixture();
  try {
    metadata.packages.push({
      name: "example-crate",
      version: "1.2.3",
      source: "registry+https://github.com/rust-lang/crates.io-index",
      manifest_path: "/registry/example-crate/Cargo.toml",
    });
    assert.throws(
      () => verifyVendoredCrates({ root, metadata }),
      /must not include a competing registry or version-resolved package/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects a path patch with no independent review evidence", () => {
  const { root, metadata } = fixture();
  try {
    const manifestPath = join(
      root,
      "testdata/vendored-crates/example-crate.json",
    );
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.review.evidence = "";
    write(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    assert.throws(
      () =>
        verifyVendoredCrates({
          root,
          metadata: {
            "Cargo.toml": metadata,
            "fuzz/Cargo.toml": { packages: [] },
          },
        }),
      /review\.evidence must be a non-empty string/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects an imprecise upstream revision", () => {
  const { root, metadata } = fixture();
  try {
    const manifestPath = join(
      root,
      "testdata/vendored-crates/example-crate.json",
    );
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.upstreamRevision = "c".repeat(41);
    write(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    assert.throws(
      () => verifyVendoredCrates({ root, metadata }),
      /upstreamRevision must be a lowercase 40-hex commit/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("discovers an unmanifested path patch in a standalone workspace", () => {
  const { root, metadata } = fixture();
  try {
    write(
      join(root, "fuzz/Cargo.toml"),
      `[package]\nname = "fuzz-fixture"\nversion = "0.0.0"\n\n[patch.crates-io]\nsecond-crate = { path = "../vendor/second-crate" }\n`,
    );
    write(
      join(root, "vendor/second-crate/Cargo.toml"),
      `[package]\nname = "second-crate"\nversion = "1.0.0"\n`,
    );
    assert.throws(
      () => verifyVendoredCrates({ root, metadata }),
      /cannot read provenance manifest .*second-crate\.json/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("dependency audit verifies path-crate provenance before running cargo-audit", () => {
  const auditScript = readFileSync(
    new URL("./audit-dependencies.sh", import.meta.url),
    "utf8",
  );
  const integrityGate = auditScript.indexOf(
    'node "$repo_root/scripts/verify-vendored-crates.mjs"',
  );
  const cargoAudit = auditScript.indexOf('"$cargo_audit_bin" audit');
  assert.ok(integrityGate >= 0, "expected audit script to invoke the integrity gate");
  assert.ok(cargoAudit >= 0, "expected audit script to invoke cargo-audit");
  assert.ok(
    integrityGate < cargoAudit,
    "vendored provenance must be checked before cargo-audit reports an advisory result",
  );
});
