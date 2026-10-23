import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmodSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { execFileSync, spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  createRustCoverageProvenance,
  prepareRustCoverageSource,
} from "./create-rust-coverage-provenance.mjs";
import { normalizeRustCoverageLcov } from "./normalize-rust-coverage-lcov.mjs";

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function fixture({ lcovSkipFunctionsArgument = "--skip-functions" } = {}) {
  const directory = mkdtempSync(
    join(tmpdir(), "dakia-rust-coverage-provenance-"),
  );
  const profile = join(directory, "coverage.profdata");
  const object = join(directory, "instrumented-test");
  const fakeLlvmCov = join(directory, "fake-llvm-cov.mjs");
  const log = join(directory, "exports.jsonl");
  const lcov = join(directory, "rust.lcov");
  const json = join(directory, "rust-functions.json");
  writeFileSync(profile, "profile");
  writeFileSync(object, "instrumented object");
  writeFileSync(
    fakeLlvmCov,
    "#!/usr/bin/env node\nprocess.stdout.write('{}');\n",
  );
  chmodSync(fakeLlvmCov, 0o755);
  writeFileSync(lcov, "SF:src/a.rs\nDA:1,1\nBRDA:1,0,0,1\nend_of_record\n");
  writeFileSync(
    json,
    JSON.stringify({
      data: [
        {
          functions: [
            {
              filenames: ["src/a.rs"],
              name: "a::observed",
              count: 1,
              regions: [[1, 1, 1, 2, 1, 0, 0, 0]],
            },
          ],
          totals: {
            instantiations: { count: 1, covered: 1 },
            functions: { count: 1, covered: 1 },
          },
        },
      ],
    }),
  );
  execFileSync("git", ["init", "--quiet"], { cwd: directory });
  writeFileSync(join(directory, "tracked.rs"), "fn checked_in() {}\n");
  execFileSync("git", ["add", "tracked.rs"], { cwd: directory });
  const sourceManifest = join(directory, "source-manifest.json");
  const sourceDigest = join(directory, "source-digest.txt");
  prepareRustCoverageSource({
    source_root: directory,
    source_manifest: sourceManifest,
    source_digest_output: sourceDigest,
  });
  execFileSync(
    process.execPath,
    [
      "scripts/coverage-llvm-cov-proxy.mjs",
      "export",
      "-format=lcov",
      `-instr-profile=${profile}`,
      "-object",
      object,
      lcovSkipFunctionsArgument,
    ],
    {
      cwd: new URL("..", import.meta.url).pathname,
      env: {
        ...process.env,
        RUST_COVERAGE_REAL_LLVM_COV: fakeLlvmCov,
        RUST_COVERAGE_EXPORT_LOG: log,
      },
    },
  );
  execFileSync(
    process.execPath,
    [
      "scripts/coverage-llvm-cov-proxy.mjs",
      "export",
      "-format=text",
      `-instr-profile=${profile}`,
      "-object",
      object,
    ],
    {
      cwd: new URL("..", import.meta.url).pathname,
      env: {
        ...process.env,
        RUST_COVERAGE_REAL_LLVM_COV: fakeLlvmCov,
        RUST_COVERAGE_EXPORT_LOG: log,
      },
    },
  );
  return {
    directory,
    profile,
    object,
    log,
    lcov,
    json,
    sourceManifest,
    sourceDigest,
  };
}

function options(paths) {
  return {
    source_root: paths.directory,
    source_manifest: paths.sourceManifest,
    source_digest_output: paths.sourceDigest,
    export_log: paths.log,
    line_branch_lcov: paths.lcov,
    functions_json: paths.json,
    line_branch_receipt: join(paths.directory, "line.receipt.json"),
    functions_receipt: join(paths.directory, "functions.receipt.json"),
    toolchain: "nightly-fixture",
  };
}

test("coverage provenance binds cargo's captured paired exports to source, profile, object and output receipts", () => {
  const paths = fixture();
  try {
    const result = createRustCoverageProvenance(options(paths));
    assert.equal(result.manifest.sourceTree.fileCount, 1);
    assert.equal(result.lineBranchReceipt.profilePath, paths.profile);
    assert.equal(result.lineBranchReceipt.profileSha256, sha256(paths.profile));
    assert.deepEqual(
      result.lineBranchReceipt.observedInputState.objectStateBefore,
      result.functionsReceipt.observedInputState.objectStateBefore,
    );
    assert.equal(result.lineBranchReceipt.outputSha256, sha256(paths.lcov));
    assert.equal(result.functionsReceipt.outputSha256, sha256(paths.json));
    assert.match(
      readFileSync(paths.log, "utf8"),
      /rust-coverage-export-invocation/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("coverage provenance accepts Cargo's captured single-dash LCOV skip-functions argument", () => {
  const paths = fixture({ lcovSkipFunctionsArgument: "-skip-functions" });
  try {
    const events = readFileSync(paths.log, "utf8")
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    assert.deepEqual(
      events[0].arguments.slice(-3),
      ["-object", paths.object, "-skip-functions"].slice(-3),
    );
    assert.equal(events[0].arguments.includes("--skip-functions"), false);
    const result = createRustCoverageProvenance(options(paths));
    assert.equal(result.lineBranchReceipt.format, "lcov");
    assert.equal(result.functionsReceipt.format, "json");
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("single-dash Cargo export flows through paired provenance receipts into normalization", () => {
  const paths = fixture({ lcovSkipFunctionsArgument: "-skip-functions" });
  try {
    const provenance = createRustCoverageProvenance(options(paths));
    const normalized = normalizeRustCoverageLcov({
      lineBranchLcov: paths.lcov,
      functionsJson: paths.json,
      profile: paths.profile,
      expectedProfileSha256: sha256(paths.profile),
      sourceDigest: provenance.manifest.sourceTree.digest,
      sourceManifest: paths.sourceManifest,
      lineBranchReceipt: options(paths).line_branch_receipt,
      functionsReceipt: options(paths).functions_receipt,
    });
    assert.match(normalized.output, /^SF:src\/a\.rs\nFN:1,a::observed/m);
    assert.deepEqual(normalized.summary.linesBranches.branches, {
      found: 1,
      hit: 1,
    });
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("coverage provenance rejects either skip-functions spelling in the paired JSON export", () => {
  for (const skipFunctionsArgument of ["--skip-functions", "-skip-functions"]) {
    const paths = fixture();
    try {
      const events = readFileSync(paths.log, "utf8")
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line));
      events[1].arguments.push(skipFunctionsArgument);
      writeFileSync(
        paths.log,
        `${events.map((event) => JSON.stringify(event)).join("\n")}\n`,
      );
      assert.throws(
        () => createRustCoverageProvenance(options(paths)),
        /Function export must be JSON-compatible and include functions/,
      );
    } finally {
      rmSync(paths.directory, { recursive: true, force: true });
    }
  }
});

test("coverage provenance fails closed when the proxy log records distinct object cohorts", () => {
  const paths = fixture();
  try {
    const events = readFileSync(paths.log, "utf8")
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    const differentObject = join(paths.directory, "other-instrumented-test");
    writeFileSync(differentObject, "other instrumented object");
    for (const state of [
      events[1].objectStateBefore,
      events[1].objectStateAfter,
    ]) {
      state[0] = {
        path: differentObject,
        bytes: readFileSync(differentObject).byteLength,
        sha256: sha256(differentObject),
      };
    }
    events[1].arguments[events[1].arguments.indexOf(paths.object)] =
      differentObject;
    writeFileSync(
      paths.log,
      `${events.map((event) => JSON.stringify(event)).join("\n")}\n`,
    );
    assert.throws(
      () => createRustCoverageProvenance(options(paths)),
      /Recorded exports do not use one llvm-cov program, profile, and object cohort/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("coverage provenance rejects tracked source mutations after the pre-instrumentation snapshot", () => {
  const paths = fixture();
  try {
    writeFileSync(
      join(paths.directory, "tracked.rs"),
      "fn changed_after_snapshot() {}\n",
    );
    assert.throws(
      () => createRustCoverageProvenance(options(paths)),
      /Tracked source inventory changed during coverage instrumentation or export/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("coverage provenance rejects a profile changed after its captured export", () => {
  const paths = fixture();
  try {
    writeFileSync(paths.profile, "profile changed after export");
    assert.throws(
      () => createRustCoverageProvenance(options(paths)),
      /Line-and-branch profile no longer matches the export observation/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("coverage provenance rejects a failed, duplicate, or stale proxy export event", () => {
  const paths = fixture();
  try {
    const events = readFileSync(paths.log, "utf8")
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    events[0].exitCode = 7;
    writeFileSync(
      paths.log,
      `${events.map((event) => JSON.stringify(event)).join("\n")}\n`,
    );
    assert.throws(
      () => createRustCoverageProvenance(options(paths)),
      /Line-and-branch proxy invocation is invalid or failed/,
    );
    events[0].exitCode = 0;
    events.push(events[0]);
    writeFileSync(
      paths.log,
      `${events.map((event) => JSON.stringify(event)).join("\n")}\n`,
    );
    assert.throws(
      () => createRustCoverageProvenance(options(paths)),
      /Line-and-branch requires exactly one recorded export invocation/,
    );
    events.pop();
    events[1].arguments[events[1].arguments.indexOf("-format=text")] =
      "-format=lcov";
    writeFileSync(
      paths.log,
      `${events.map((event) => JSON.stringify(event)).join("\n")}\n`,
    );
    assert.throws(
      () => createRustCoverageProvenance(options(paths)),
      /Line-and-branch requires exactly one recorded export invocation/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("proxy records failed exports and rejects positional objects outside -object pairs", () => {
  const paths = fixture();
  try {
    const failed = join(paths.directory, "failing-llvm-cov.mjs");
    writeFileSync(failed, "#!/usr/bin/env node\nprocess.exitCode = 7;\n");
    chmodSync(failed, 0o755);
    const failedResult = spawnSync(
      process.execPath,
      [
        "scripts/coverage-llvm-cov-proxy.mjs",
        "export",
        "-format=lcov",
        `-instr-profile=${paths.profile}`,
        "-object",
        paths.object,
        "--skip-functions",
      ],
      {
        cwd: new URL("..", import.meta.url).pathname,
        encoding: "utf8",
        env: {
          ...process.env,
          RUST_COVERAGE_REAL_LLVM_COV: failed,
          RUST_COVERAGE_EXPORT_LOG: paths.log,
        },
      },
    );
    assert.equal(failedResult.status, 7);
    assert.match(readFileSync(paths.log, "utf8"), /"exitCode":7/);
    const positional = spawnSync(
      process.execPath,
      [
        "scripts/coverage-llvm-cov-proxy.mjs",
        "export",
        "-format=lcov",
        `-instr-profile=${paths.profile}`,
        "-object",
        paths.object,
        paths.object,
        "--skip-functions",
      ],
      {
        cwd: new URL("..", import.meta.url).pathname,
        encoding: "utf8",
        env: {
          ...process.env,
          RUST_COVERAGE_REAL_LLVM_COV: failed,
          RUST_COVERAGE_EXPORT_LOG: paths.log,
        },
      },
    );
    assert.notEqual(positional.status, 0);
    assert.match(
      `${positional.stdout}${positional.stderr}`,
      /outside a -object argument pair/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});
