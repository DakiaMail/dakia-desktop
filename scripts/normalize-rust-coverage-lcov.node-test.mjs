import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { summarizeLcov } from "./coverage-ratchet.mjs";
import { normalizeRustCoverageLcov } from "./normalize-rust-coverage-lcov.mjs";

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function sha256File(path) {
  return sha256(readFileSync(path));
}

function fixture({
  mutateCoverage = (value) => value,
  mutateReceipts = (value) => value,
} = {}) {
  const directory = mkdtempSync(join(tmpdir(), "dakia-lcov-normalizer-"));
  const profile = join(directory, "coverage.profdata");
  const lcov = join(directory, "skip-functions.lcov");
  const json = join(directory, "coverage.json");
  const manifest = join(directory, "source-manifest.json");
  const lineBranchReceipt = join(directory, "line-branch-receipt.json");
  const functionsReceipt = join(directory, "functions-receipt.json");
  const sourceDigest = "a".repeat(64);
  const objectA = join(directory, "object-a");
  const objectB = join(directory, "object-b");
  writeFileSync(objectA, "fixture object A");
  writeFileSync(objectB, "fixture object B with different bytes");
  const objectState = [
    {
      path: objectA,
      bytes: readFileSync(objectA).byteLength,
      sha256: sha256File(objectA),
    },
    {
      path: objectB,
      bytes: readFileSync(objectB).byteLength,
      sha256: sha256File(objectB),
    },
  ];
  const objectSetSha256 = sha256(JSON.stringify(objectState));
  writeFileSync(profile, "fictional profile\n");
  writeFileSync(
    manifest,
    JSON.stringify({ sourceTree: { digest: sourceDigest }, files: [] }),
  );
  writeFileSync(
    lcov,
    [
      "SF:/repo/a.rs",
      "FNF:1",
      "FNH:1",
      "DA:1,3",
      "BRDA:1,0,0,3",
      "BRDA:1,0,1,0",
      "LF:1",
      "LH:1",
      "BRF:2",
      "BRH:1",
      "end_of_record",
      "SF:/repo/b.rs",
      "FNF:1",
      "FNH:0",
      "DA:5,0",
      "LF:1",
      "LH:0",
      "end_of_record",
      "",
    ].join("\n"),
  );
  const data = {
    data: [
      {
        functions: [
          {
            filenames: ["/repo/a.rs"],
            name: "a::first",
            count: 3,
            regions: [[2, 1, 2, 5, 3, 0, 0, 0]],
          },
          {
            filenames: ["/repo/b.rs"],
            name: "b::second",
            count: 0,
            regions: [[5, 1, 5, 5, 0, 0, 0, 0]],
          },
          {
            filenames: ["/repo/a.rs"],
            name: "a::empty_nongen",
            count: 0,
            regions: [],
            branches: [[3, 1, 3, 2, 0, 0]],
          },
          {
            filenames: ["/third-party/dependency.rs"],
            name: "dependency::ignored_by_lcov_filter",
            count: 9,
            regions: [[1, 1, 1, 2, 9, 0, 0, 0]],
          },
        ],
        totals: {
          instantiations: { count: 2, covered: 1 },
          functions: { count: 2, covered: 1 },
        },
      },
    ],
  };
  writeFileSync(json, JSON.stringify(mutateCoverage(data), null, 2));
  const common = {
    version: 1,
    kind: "rust-coverage-export",
    sourceDigest,
    sourceManifestSha256: sha256File(manifest),
    profilePath: profile,
    profileSha256: sha256File(profile),
    objectSetSha256,
    observedInputState: {
      profileSha256Before: sha256File(profile),
      profileSha256After: sha256File(profile),
      objectStateBefore: objectState,
      objectStateAfter: objectState,
    },
    producer: { program: "llvm-cov", toolchain: "nightly-test", arguments: [] },
  };
  const receipts = mutateReceipts({
    lineBranch: {
      ...common,
      format: "lcov",
      outputSha256: sha256File(lcov),
      producer: {
        ...common.producer,
        arguments: [
          "export",
          "-format=lcov",
          `-instr-profile=${profile}`,
          "-object",
          objectA,
          "-object",
          objectB,
          "--skip-functions",
        ],
      },
    },
    functions: {
      ...common,
      format: "json",
      outputSha256: sha256File(json),
      producer: {
        ...common.producer,
        arguments: [
          "export",
          `-instr-profile=${profile}`,
          "-object",
          objectA,
          "-object",
          objectB,
        ],
      },
    },
  });
  writeFileSync(
    lineBranchReceipt,
    JSON.stringify(receipts.lineBranch, null, 2),
  );
  writeFileSync(functionsReceipt, JSON.stringify(receipts.functions, null, 2));
  return {
    directory,
    profile,
    lcov,
    json,
    manifest,
    lineBranchReceipt,
    functionsReceipt,
    sourceDigest,
    objectA,
    objectB,
    profileSha256: sha256File(profile),
  };
}

function normalize(paths, overrides = {}) {
  return normalizeRustCoverageLcov({
    lineBranchLcov: paths.lcov,
    functionsJson: paths.json,
    profile: paths.profile,
    expectedProfileSha256: paths.profileSha256,
    sourceDigest: paths.sourceDigest,
    sourceManifest: paths.manifest,
    lineBranchReceipt: paths.lineBranchReceipt,
    functionsReceipt: paths.functionsReceipt,
    ...overrides,
  });
}

test("merges official line and branch observations with receipt-bound JSON instrumentation functions", () => {
  const paths = fixture();
  try {
    const result = normalize(paths);
    assert.match(
      result.output,
      /^SF:\/repo\/a\.rs\nFN:2,a::first\nFNDA:3,a::first/m,
    );
    assert.match(
      result.output,
      /^SF:\/repo\/b\.rs\nFN:5,b::second\nFNDA:0,b::second/m,
    );
    assert.doesNotMatch(result.output, /^FNF:|^FNH:/m);
    assert.match(result.output, /^DA:1,3$/m);
    assert.match(result.output, /^BRDA:1,0,1,0$/m);
    const withoutFunctionRecords = (value) =>
      value
        .split("\n")
        .filter(
          (line) =>
            !line.startsWith("FN:") &&
            !line.startsWith("FNDA:") &&
            !line.startsWith("FNF:") &&
            !line.startsWith("FNH:"),
        )
        .join("\n");
    assert.equal(
      withoutFunctionRecords(result.output),
      withoutFunctionRecords(readFileSync(paths.lcov, "utf8")),
      "normalization must preserve every non-function LCOV record byte-for-byte",
    );
    assert.deepEqual(summarizeLcov(result.output), {
      lines: { found: 2, hit: 1 },
      branches: { found: 2, hit: 1 },
      functions: { found: 2, hit: 1 },
    });
    assert.equal(
      result.summary.functions.metric,
      "llvm-instrumentation-instantiations",
    );
    assert.deepEqual(result.summary.functions.instantiations, {
      found: 2,
      hit: 1,
    });
    assert.deepEqual(result.summary.functions.distinctFunctionDefinitions, {
      count: 2,
      covered: 1,
    });
    assert.equal(
      result.summary.functions.outsideLineBranchSourceFileRecords,
      1,
    );
    assert.deepEqual(result.summary.functions.unavailableFunctionMappings, [
      {
        name: "a::empty_nongen",
        filename: "/repo/a.rs",
        invocationCount: 0,
        branchRecordCount: 1,
      },
    ]);
    assert.equal(
      result.summary.input.sourceManifestSha256,
      sha256File(paths.manifest),
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("accepts cargo-llvm-cov's JSON-compatible -format=text export invocation", () => {
  const paths = fixture({
    mutateReceipts: (receipts) => ({
      ...receipts,
      functions: {
        ...receipts.functions,
        producer: {
          ...receipts.functions.producer,
          arguments: [
            "export",
            "-format=text",
            ...receipts.functions.producer.arguments.slice(1),
          ],
        },
      },
    }),
  });
  try {
    assert.equal(
      normalize(paths).summary.functions.metric,
      "llvm-instrumentation-instantiations",
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("accepts cargo-llvm-cov's single-dash LCOV skip-functions argument", () => {
  const paths = fixture({
    mutateReceipts: (receipts) => ({
      ...receipts,
      lineBranch: {
        ...receipts.lineBranch,
        producer: {
          ...receipts.lineBranch.producer,
          arguments: receipts.lineBranch.producer.arguments.map((argument) =>
            argument === "--skip-functions" ? "-skip-functions" : argument,
          ),
        },
      },
    }),
  });
  try {
    assert.equal(
      normalize(paths).summary.functions.metric,
      "llvm-instrumentation-instantiations",
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("rejects a profile typo before accepting either export", () => {
  const paths = fixture();
  try {
    assert.throws(
      () => normalize(paths, { expectedProfileSha256: "c".repeat(64) }),
      /Profile digest mismatch/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("rejects swapped profile, source, object-set and output provenance", () => {
  const cases = [
    {
      name: "swapped profile",
      mutateReceipts: (r) => ({
        ...r,
        functions: { ...r.functions, profileSha256: "d".repeat(64) },
      }),
      error: /Function JSON export receipt profile digest/,
    },
    {
      name: "profile changed during export",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          observedInputState: {
            ...r.functions.observedInputState,
            profileSha256After: "e".repeat(64),
          },
        },
      }),
      error: /Function JSON export receipt profile changed during export/,
    },
    {
      name: "object state changed during export",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          observedInputState: {
            ...r.functions.observedInputState,
            objectStateAfter:
              r.functions.observedInputState.objectStateAfter.map(
                (entry, index) =>
                  index === 0 ? { ...entry, sha256: "e".repeat(64) } : entry,
              ),
          },
        },
      }),
      error: /Function JSON export receipt object state changed during export/,
    },
    {
      name: "object argument drift",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: {
            ...r.functions.producer,
            arguments: r.functions.producer.arguments.map((argument) =>
              argument ===
              r.functions.observedInputState.objectStateAfter[1].path
                ? "/fixture/other"
                : argument,
            ),
          },
        },
      }),
      error:
        /Function JSON export receipt -object arguments do not match observed state/,
    },
    {
      name: "source digest",
      mutateReceipts: (r) => ({
        ...r,
        lineBranch: { ...r.lineBranch, sourceDigest: "e".repeat(64) },
      }),
      error: /Line-and-branch export receipt source digest/,
    },
    {
      name: "manifest digest",
      mutateReceipts: (r) => ({
        ...r,
        functions: { ...r.functions, sourceManifestSha256: "f".repeat(64) },
      }),
      error: /Function JSON export receipt source manifest digest/,
    },
    {
      name: "fabricated shared object set",
      mutateReceipts: (r) => ({
        ...r,
        lineBranch: { ...r.lineBranch, objectSetSha256: "d".repeat(64) },
        functions: { ...r.functions, objectSetSha256: "d".repeat(64) },
      }),
      error: /object-set digest does not match observed state/,
    },
    {
      name: "toolchain drift",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: { ...r.functions.producer, toolchain: "other-nightly" },
        },
      }),
      error: /different llvm-cov toolchains/,
    },
    {
      name: "command filter drift",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: {
            ...r.functions.producer,
            arguments: [...r.functions.producer.arguments, "--name=wrong"],
          },
        },
      }),
      error: /different command cohorts/,
    },
    {
      name: "output",
      mutateReceipts: (r) => ({
        ...r,
        lineBranch: { ...r.lineBranch, outputSha256: "c".repeat(64) },
      }),
      error: /Line-and-branch export receipt output digest/,
    },
    {
      name: "explicit non-JSON function format",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: {
            ...r.functions.producer,
            arguments: [
              "export",
              "-format=lcov",
              `-instr-profile=${r.functions.profilePath}`,
            ],
          },
        },
      }),
      error: /must use llvm-cov export's JSON-compatible output/,
    },
    {
      name: "missing skip functions",
      mutateReceipts: (r) => ({
        ...r,
        lineBranch: {
          ...r.lineBranch,
          producer: {
            ...r.lineBranch.producer,
            arguments: [
              "export",
              "-format=lcov",
              `-instr-profile=${r.lineBranch.profilePath}`,
            ],
          },
        },
      }),
      error: /requires exactly one bare skip-functions argument/,
    },
    {
      name: "ambiguous bare skip functions",
      mutateReceipts: (r) => ({
        ...r,
        lineBranch: {
          ...r.lineBranch,
          producer: {
            ...r.lineBranch.producer,
            arguments: [...r.lineBranch.producer.arguments, "-skip-functions"],
          },
        },
      }),
      error: /requires exactly one bare skip-functions argument/,
    },
    {
      name: "equals-form LCOV skip functions",
      mutateReceipts: (r) => ({
        ...r,
        lineBranch: {
          ...r.lineBranch,
          producer: {
            ...r.lineBranch.producer,
            arguments: r.lineBranch.producer.arguments.map((argument) =>
              argument === "--skip-functions"
                ? "--skip-functions=true"
                : argument,
            ),
          },
        },
      }),
      error: /requires exactly one bare skip-functions argument/,
    },
    {
      name: "single-dash equals-form LCOV skip functions",
      mutateReceipts: (r) => ({
        ...r,
        lineBranch: {
          ...r.lineBranch,
          producer: {
            ...r.lineBranch.producer,
            arguments: r.lineBranch.producer.arguments.map((argument) =>
              argument === "--skip-functions"
                ? "-skip-functions=true"
                : argument,
            ),
          },
        },
      }),
      error: /requires exactly one bare skip-functions argument/,
    },
    {
      name: "bare JSON skip functions",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: {
            ...r.functions.producer,
            arguments: [...r.functions.producer.arguments, "-skip-functions"],
          },
        },
      }),
      error: /Function JSON export receipt must not skip functions/,
    },
    {
      name: "long-form bare JSON skip functions",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: {
            ...r.functions.producer,
            arguments: [...r.functions.producer.arguments, "--skip-functions"],
          },
        },
      }),
      error: /Function JSON export receipt must not skip functions/,
    },
    {
      name: "equals-form JSON skip functions",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: {
            ...r.functions.producer,
            arguments: [
              ...r.functions.producer.arguments,
              "-skip-functions=true",
            ],
          },
        },
      }),
      error: /Function JSON export receipt must not skip functions/,
    },
    {
      name: "long-form equals JSON skip functions",
      mutateReceipts: (r) => ({
        ...r,
        functions: {
          ...r.functions,
          producer: {
            ...r.functions.producer,
            arguments: [
              ...r.functions.producer.arguments,
              "--skip-functions=true",
            ],
          },
        },
      }),
      error: /Function JSON export receipt must not skip functions/,
    },
  ];
  for (const entry of cases) {
    const paths = fixture({ mutateReceipts: entry.mutateReceipts });
    try {
      assert.throws(() => normalize(paths), entry.error, entry.name);
    } finally {
      rmSync(paths.directory, { recursive: true, force: true });
    }
  }
});

test("rejects a current object that no longer matches the export receipt", () => {
  const paths = fixture();
  try {
    writeFileSync(paths.objectA, "fixture object Z");
    assert.throws(
      () => normalize(paths),
      /Line-and-branch export receipt current object state does not match export state/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("rejects reference-only receipts and mismatched source manifests", () => {
  const paths = fixture({
    mutateReceipts: (receipts) => ({
      ...receipts,
      lineBranch: {
        ...receipts.lineBranch,
        constructedForReferenceMappingOnly: true,
      },
    }),
  });
  try {
    assert.throws(
      () => normalize(paths),
      /reference-only and cannot establish coverage provenance/,
    );
    writeFileSync(
      paths.manifest,
      JSON.stringify({ sourceTree: { digest: "d".repeat(64) }, files: [] }),
    );
    assert.throws(
      () => normalize(paths),
      /Source manifest digest or file inventory does not match/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});

test("rejects ambiguous, covered-empty, missing and duplicated function mappings", () => {
  const cases = [
    {
      name: "ambiguous filename",
      mutateCoverage: (data) => {
        data.data[0].functions[0].filenames.push("/repo/b.rs");
        return data;
      },
      error: /ambiguous source filename/,
    },
    {
      name: "covered empty region",
      mutateCoverage: (data) => {
        data.data[0].functions[2].count = 1;
        return data;
      },
      error: /function mapping is unavailable but invocation count is nonzero/,
    },
    {
      name: "missing source mapping",
      mutateCoverage: (data) => {
        data.data[0].functions = data.data[0].functions.filter(
          (entry) => entry.filenames[0] !== "/repo/b.rs",
        );
        data.data[0].totals.instantiations = { count: 1, covered: 1 };
        return data;
      },
      error: /LCOV SF record has no executable JSON functions: \/repo\/b\.rs/,
    },
    {
      name: "duplicate function",
      mutateCoverage: (data) => {
        data.data[0].functions.push({ ...data.data[0].functions[1] });
        data.data[0].totals.instantiations = { count: 3, covered: 1 };
        return data;
      },
      error: /Duplicate function name in \/repo\/b\.rs/,
    },
  ];
  for (const entry of cases) {
    const paths = fixture({ mutateCoverage: entry.mutateCoverage });
    try {
      assert.throws(() => normalize(paths), entry.error, entry.name);
    } finally {
      rmSync(paths.directory, { recursive: true, force: true });
    }
  }
});

test("rejects LCOV that still contains function records after skip-functions export", () => {
  const paths = fixture();
  try {
    writeFileSync(
      paths.lcov,
      `${readFileSync(paths.lcov, "utf8").replace("FNF:1", "FN:2,a::stale\nFNF:1")}`,
    );
    const receipt = JSON.parse(readFileSync(paths.lineBranchReceipt, "utf8"));
    receipt.outputSha256 = sha256File(paths.lcov);
    writeFileSync(paths.lineBranchReceipt, JSON.stringify(receipt, null, 2));
    assert.throws(
      () => normalize(paths),
      /Expected skip-functions LCOV but found function record/,
    );
  } finally {
    rmSync(paths.directory, { recursive: true, force: true });
  }
});
