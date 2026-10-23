#!/usr/bin/env node

import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";

import { summarizeLcov } from "./coverage-ratchet.mjs";

function fail(message) {
  throw new Error(message);
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function isSha256(value) {
  return /^[a-f0-9]{64}$/i.test(value);
}

function objectArguments(argumentsList, label) {
  const paths = [];
  for (let index = 0; index < argumentsList.length; index += 1) {
    if (argumentsList[index] !== "-object") continue;
    const path = argumentsList[index + 1];
    if (typeof path !== "string" || !path || path.startsWith("-")) {
      fail(`${label} export receipt has an invalid -object argument`);
    }
    paths.push(path);
    index += 1;
  }
  if (new Set(paths).size !== paths.length) {
    fail(`${label} export receipt has duplicate -object arguments`);
  }
  return paths;
}

function skipFunctionsArgumentState(argumentsList) {
  const supported = ["--skip-functions", "-skip-functions"];
  return {
    count: argumentsList.filter((argument) => supported.includes(argument))
      .length,
    hasUnsupportedEqualsForm: argumentsList.some(
      (argument) =>
        argument.startsWith("--skip-functions=") ||
        argument.startsWith("-skip-functions="),
    ),
  };
}

function observedObjectState(value, label) {
  if (!Array.isArray(value) || value.length === 0) {
    fail(`${label} export receipt has no observed object state`);
  }
  const state = value.map((entry) => {
    if (
      !entry ||
      typeof entry.path !== "string" ||
      !entry.path ||
      !Number.isSafeInteger(entry.bytes) ||
      entry.bytes < 0 ||
      !isSha256(entry.sha256)
    ) {
      fail(`${label} export receipt has an invalid observed object state`);
    }
    return { path: entry.path, bytes: entry.bytes, sha256: entry.sha256 };
  });
  if (new Set(state.map((entry) => entry.path)).size !== state.length) {
    fail(`${label} export receipt has duplicate observed object paths`);
  }
  return state;
}

function canonicalObjectState(state) {
  return JSON.stringify(state);
}

function verifyCurrentObjectState(state, label) {
  const current = state.map((entry) => {
    let bytes;
    try {
      bytes = statSync(entry.path).size;
    } catch (error) {
      fail(
        `${label} export receipt object is unavailable: ${entry.path} (${error instanceof Error ? error.message : String(error)})`,
      );
    }
    return { path: entry.path, bytes, sha256: sha256(entry.path) };
  });
  if (canonicalObjectState(current) !== canonicalObjectState(state)) {
    fail(
      `${label} export receipt current object state does not match export state`,
    );
  }
}

function parseSourceManifest(path, sourceDigest) {
  let manifest;
  try {
    manifest = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(
      `Source manifest is invalid: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (
    (manifest?.sourceTree?.digest ?? manifest?.digest) !== sourceDigest ||
    !Array.isArray(manifest?.files)
  ) {
    fail(
      "Source manifest digest or file inventory does not match --source-digest",
    );
  }
  return { path, sha256: sha256(path) };
}

function parseExportReceipt({
  path,
  label,
  format,
  outputPath,
  profileSha256,
  profilePath,
  sourceDigest,
  sourceManifestSha256,
  allowReferenceOnlyReceipts,
}) {
  let receipt;
  try {
    receipt = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(
      `${label} export receipt is invalid: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (
    receipt?.version !== 1 ||
    receipt?.kind !== "rust-coverage-export" ||
    receipt?.format !== format
  ) {
    fail(`${label} export receipt has an unsupported schema`);
  }
  if (
    receipt.constructedForReferenceMappingOnly === true &&
    !allowReferenceOnlyReceipts
  ) {
    fail(
      `${label} export receipt is reference-only and cannot establish coverage provenance`,
    );
  }
  const observed = receipt.observedInputState;
  if (!observed || typeof observed !== "object") {
    fail(`${label} export receipt has no observed input state`);
  }
  if (
    observed.profileSha256Before !== profileSha256 ||
    observed.profileSha256After !== profileSha256
  ) {
    fail(`${label} export receipt profile changed during export`);
  }
  const objectStateBefore = observedObjectState(
    observed.objectStateBefore,
    label,
  );
  const objectStateAfter = observedObjectState(
    observed.objectStateAfter,
    label,
  );
  if (
    canonicalObjectState(objectStateBefore) !==
    canonicalObjectState(objectStateAfter)
  ) {
    fail(`${label} export receipt object state changed during export`);
  }
  if (
    receipt.objectSetSha256 !==
    createHash("sha256")
      .update(canonicalObjectState(objectStateBefore))
      .digest("hex")
  ) {
    fail(
      `${label} export receipt object-set digest does not match observed state`,
    );
  }
  for (const [name, actual, expected] of [
    ["source digest", receipt.sourceDigest, sourceDigest],
    [
      "source manifest digest",
      receipt.sourceManifestSha256,
      sourceManifestSha256,
    ],
    ["profile digest", receipt.profileSha256, profileSha256],
    ["output digest", receipt.outputSha256, sha256(outputPath)],
  ]) {
    if (actual !== expected) {
      fail(
        `${label} export receipt ${name} does not match the supplied cohort`,
      );
    }
  }
  if (!isSha256(receipt.objectSetSha256)) {
    fail(`${label} export receipt has no object-set SHA-256`);
  }
  if (
    typeof receipt.profilePath !== "string" ||
    receipt.profilePath !== profilePath ||
    typeof receipt.producer?.program !== "string" ||
    !receipt.producer.program.includes("llvm-cov") ||
    typeof receipt.producer?.toolchain !== "string" ||
    !Array.isArray(receipt.producer?.arguments)
  ) {
    fail(`${label} export receipt has no llvm-cov producer binding`);
  }
  const argumentsSet = new Set(receipt.producer.arguments);
  if (!argumentsSet.has(`-instr-profile=${receipt.profilePath}`)) {
    fail(
      `${label} export receipt does not bind the expected llvm-cov profile path`,
    );
  }
  if (format === "lcov" && !argumentsSet.has("-format=lcov")) {
    fail("Line-and-branch export receipt does not require LCOV output");
  }
  if (
    format === "json" &&
    receipt.producer.arguments.some(
      (argument) =>
        argument.startsWith("-format=") && argument !== "-format=text",
    )
  ) {
    fail(
      "Function JSON export receipt must use llvm-cov export's JSON-compatible output",
    );
  }
  const skipFunctions = skipFunctionsArgumentState(receipt.producer.arguments);
  if (
    format === "lcov" &&
    (skipFunctions.count !== 1 || skipFunctions.hasUnsupportedEqualsForm)
  ) {
    fail(
      "Line-and-branch export receipt requires exactly one bare skip-functions argument",
    );
  }
  if (
    format === "json" &&
    (skipFunctions.count !== 0 || skipFunctions.hasUnsupportedEqualsForm)
  ) {
    fail("Function JSON export receipt must not skip functions");
  }
  const objectPaths = objectArguments(receipt.producer.arguments, label);
  const observedObjectPaths = objectStateBefore.map((entry) => entry.path);
  if (JSON.stringify(objectPaths) !== JSON.stringify(observedObjectPaths)) {
    fail(
      `${label} export receipt -object arguments do not match observed state`,
    );
  }
  verifyCurrentObjectState(objectStateAfter, label);
  return {
    path,
    sha256: sha256(path),
    objectSetSha256: receipt.objectSetSha256,
    producer: receipt.producer,
    arguments: receipt.producer.arguments,
    objectStateSha256: receipt.objectSetSha256,
  };
}

function parseLcovRecords(source) {
  const records = [];
  const seen = new Set();
  let current;

  for (const line of source.split(/\r?\n/)) {
    if (!line) continue;
    if (line.startsWith("SF:")) {
      if (current)
        fail(`LCOV record for ${current.path} is missing end_of_record`);
      const path = line.slice(3);
      if (!path) fail("LCOV record has an empty SF path");
      if (seen.has(path)) fail(`LCOV has duplicate SF record: ${path}`);
      seen.add(path);
      current = { path, lines: [line] };
      continue;
    }
    if (line === "end_of_record") {
      if (!current) fail("LCOV has end_of_record without SF");
      current.lines.push(line);
      records.push(current);
      current = undefined;
      continue;
    }
    if (!current) fail(`LCOV data outside a record: ${line}`);
    current.lines.push(line);
  }

  if (current) fail(`LCOV record for ${current.path} is missing end_of_record`);
  if (records.length === 0) fail("LCOV has no SF records");
  return records;
}

function parseCoverageJson(source) {
  let parsed;
  try {
    parsed = JSON.parse(source);
  } catch (error) {
    fail(
      `Coverage JSON is invalid: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (!Array.isArray(parsed.data) || parsed.data.length !== 1) {
    fail("Coverage JSON must contain exactly one data record");
  }
  const data = parsed.data[0];
  if (
    !Array.isArray(data.functions) ||
    !data.totals?.instantiations ||
    !data.totals?.functions
  ) {
    fail("Coverage JSON is missing functions or totals");
  }
  for (const [name, metric] of Object.entries({
    instantiations: data.totals.instantiations,
    functions: data.totals.functions,
  })) {
    if (
      !Number.isInteger(metric.count) ||
      !Number.isInteger(metric.covered) ||
      metric.count < 0 ||
      metric.covered < 0 ||
      metric.covered > metric.count
    ) {
      fail(`Coverage JSON has invalid ${name} totals`);
    }
  }
  return data;
}

function executableFunction(record, sourceFiles, empty) {
  if (!record || typeof record !== "object")
    fail("Coverage JSON contains a non-object function record");
  if (!Array.isArray(record.filenames) || record.filenames.length === 0) {
    fail("Coverage JSON function has no filename");
  }
  const includedNames = record.filenames.filter((name) =>
    sourceFiles.has(name),
  );
  if (includedNames.length === 0) return undefined;
  if (record.filenames.length !== 1 || includedNames.length !== 1) {
    fail(
      `Coverage JSON function has ambiguous source filename: ${record.name ?? "<unnamed>"}`,
    );
  }
  if (typeof record.name !== "string" || !record.name) {
    fail("Coverage JSON function has no name");
  }
  if (!Number.isInteger(record.count) || record.count < 0) {
    fail(`Coverage JSON function has invalid count: ${record.name}`);
  }
  if (!Array.isArray(record.regions)) {
    fail(`Coverage JSON function has no regions array: ${record.name}`);
  }
  if (record.regions.length === 0) {
    if (record.count !== 0) {
      fail(
        `Coverage JSON function mapping is unavailable but invocation count is nonzero: ${record.name}`,
      );
    }
    empty.push({
      name: record.name,
      filename: record.filenames[0],
      invocationCount: record.count,
      branchRecordCount: Array.isArray(record.branches)
        ? record.branches.length
        : 0,
    });
    return undefined;
  }
  const region = record.regions[0];
  if (
    !Array.isArray(region) ||
    !Number.isInteger(region[0]) ||
    region[0] <= 0
  ) {
    fail(`Coverage JSON function has invalid first region: ${record.name}`);
  }
  return {
    filename: record.filenames[0],
    line: region[0],
    name: record.name,
    count: record.count,
  };
}

function functionsForRecords(records, data) {
  const sourceFiles = new Set(records.map((record) => record.path));
  const empty = [];
  const functions = [];
  const outsideSourceFiles = [];
  for (const record of data.functions) {
    const mapped = executableFunction(record, sourceFiles, empty);
    if (mapped) {
      functions.push(mapped);
    } else if (record?.filenames?.every((name) => !sourceFiles.has(name))) {
      outsideSourceFiles.push(record);
    }
  }

  const byFile = new Map(records.map((record) => [record.path, []]));
  for (const entry of functions) {
    const entries = byFile.get(entry.filename);
    if (!entries)
      fail(`Function target is absent from LCOV SF records: ${entry.filename}`);
    entries.push(entry);
  }
  for (const [filename, entries] of byFile) {
    if (entries.length === 0)
      fail(`LCOV SF record has no executable JSON functions: ${filename}`);
    const names = new Set();
    for (const entry of entries) {
      if (names.has(entry.name))
        fail(`Duplicate function name in ${filename}: ${entry.name}`);
      names.add(entry.name);
    }
  }

  const expected = data.totals.instantiations;
  const hit = functions.filter((entry) => entry.count > 0).length;
  if (functions.length !== expected.count || hit !== expected.covered) {
    fail(
      `Coverage JSON instantiation totals do not match emitted functions: ${functions.length}/${hit} versus ${expected.count}/${expected.covered}`,
    );
  }

  return { byFile, empty, outsideSourceFiles: outsideSourceFiles.length };
}

function renderRecord(record, functions) {
  const preserved = [];
  for (const line of record.lines) {
    if (line.startsWith("FN:") || line.startsWith("FNDA:")) {
      fail(
        `Expected skip-functions LCOV but found function record in ${record.path}`,
      );
    }
    if (line.startsWith("FNF:") || line.startsWith("FNH:")) continue;
    preserved.push(line);
  }
  if (
    preserved[0] !== `SF:${record.path}` ||
    preserved.at(-1) !== "end_of_record"
  ) {
    fail(`LCOV record structure changed while normalizing ${record.path}`);
  }
  const functionLines = [
    ...functions.map((entry) => `FN:${entry.line},${entry.name}`),
    ...functions.map((entry) => `FNDA:${entry.count},${entry.name}`),
  ];
  return [preserved[0], ...functionLines, ...preserved.slice(1)].join("\n");
}

function sameExportCohort(lineBranch, functions) {
  if (lineBranch.producer.program !== functions.producer.program) {
    fail("Coverage export receipts use different llvm-cov programs");
  }
  if (lineBranch.producer.toolchain !== functions.producer.toolchain) {
    fail("Coverage export receipts use different llvm-cov toolchains");
  }
  const normalizeArguments = (argumentsList) =>
    argumentsList.filter(
      (argument) =>
        argument !== "-format=lcov" &&
        argument !== "-format=text" &&
        argument !== "--skip-functions" &&
        argument !== "-skip-functions",
    );
  if (
    JSON.stringify(normalizeArguments(lineBranch.arguments)) !==
    JSON.stringify(normalizeArguments(functions.arguments))
  ) {
    fail("Coverage export receipts use different command cohorts");
  }
}

function sameMetric(left, right, name) {
  if (left.found !== right.found || left.hit !== right.hit) {
    fail(
      `Normalizing functions changed ${name}: ${left.hit}/${left.found} became ${right.hit}/${right.found}`,
    );
  }
}

export function normalizeRustCoverageLcov({
  lineBranchLcov,
  functionsJson,
  profile,
  expectedProfileSha256,
  sourceDigest,
  sourceManifest,
  lineBranchReceipt,
  functionsReceipt,
  allowReferenceOnlyReceipts = false,
}) {
  if (!isSha256(expectedProfileSha256))
    fail("--expected-profile-sha256 must be a SHA-256 digest");
  if (!isSha256(sourceDigest)) fail("--source-digest must be a SHA-256 digest");
  const actualProfileSha256 = sha256(profile);
  if (actualProfileSha256 !== expectedProfileSha256.toLowerCase()) {
    fail(
      `Profile digest mismatch: expected ${expectedProfileSha256}, got ${actualProfileSha256}`,
    );
  }

  const manifest = parseSourceManifest(sourceManifest, sourceDigest);
  const lineBranchProvenance = parseExportReceipt({
    path: lineBranchReceipt,
    label: "Line-and-branch",
    format: "lcov",
    outputPath: lineBranchLcov,
    profileSha256: actualProfileSha256,
    profilePath: profile,
    sourceDigest,
    sourceManifestSha256: manifest.sha256,
    allowReferenceOnlyReceipts,
  });
  const functionsProvenance = parseExportReceipt({
    path: functionsReceipt,
    label: "Function JSON",
    format: "json",
    outputPath: functionsJson,
    profileSha256: actualProfileSha256,
    profilePath: profile,
    sourceDigest,
    sourceManifestSha256: manifest.sha256,
    allowReferenceOnlyReceipts,
  });
  if (
    lineBranchProvenance.objectSetSha256 !== functionsProvenance.objectSetSha256
  ) {
    fail("Coverage export receipts use different object sets");
  }
  sameExportCohort(lineBranchProvenance, functionsProvenance);

  const input = readFileSync(lineBranchLcov, "utf8");
  const records = parseLcovRecords(input);
  const data = parseCoverageJson(readFileSync(functionsJson, "utf8"));
  const mapping = functionsForRecords(records, data);
  const output = `${records.map((record) => renderRecord(record, mapping.byFile.get(record.path))).join("\n")}\n`;

  const before = summarizeLcov(input);
  const after = summarizeLcov(output);
  sameMetric(before.lines, after.lines, "line observations");
  sameMetric(before.branches, after.branches, "branch observations");
  if (
    after.functions.found !== data.totals.instantiations.count ||
    after.functions.hit !== data.totals.instantiations.covered
  ) {
    fail(
      "Rendered LCOV function observations do not match JSON instantiation totals",
    );
  }

  return {
    output,
    summary: {
      version: 1,
      sourceDigest,
      profileSha256: actualProfileSha256,
      input: {
        sourceManifestSha256: manifest.sha256,
        lineBranchLcovSha256: sha256(lineBranchLcov),
        lineBranchReceiptSha256: lineBranchProvenance.sha256,
        functionsJsonSha256: sha256(functionsJson),
        functionsReceiptSha256: functionsProvenance.sha256,
        objectSetSha256: lineBranchProvenance.objectSetSha256,
        sourceFileCount: records.length,
      },
      output: {
        lcovSha256: createHash("sha256").update(output).digest("hex"),
      },
      linesBranches: {
        producer: "llvm-cov lcov with skip-functions",
        lines: after.lines,
        branches: after.branches,
        unchangedFromInput: true,
      },
      functions: {
        producer: "llvm-cov json",
        metric: "llvm-instrumentation-instantiations",
        instantiations: after.functions,
        distinctFunctionDefinitions: data.totals.functions,
        unavailableFunctionMappings: mapping.empty,
        outsideLineBranchSourceFileRecords: mapping.outsideSourceFiles,
      },
      referenceOnlyReceiptException: allowReferenceOnlyReceipts,
      limitations: [
        "The JSON distinct-function total is reported separately and is not substituted for LCOV instrumentation instantiations.",
        "Function records without regions are reported as unavailable mappings; their official DA and BRDA observations remain in the line-and-branch LCOV input.",
        "Only functions whose sole source filename is present in the official line-and-branch LCOV report are emitted.",
      ],
    },
  };
}

export function parseArgs(args) {
  const options = {};
  const allowed = new Set([
    "--line-branch-lcov",
    "--functions-json",
    "--profile",
    "--expected-profile-sha256",
    "--source-digest",
    "--source-manifest",
    "--line-branch-receipt",
    "--functions-receipt",
    "--output",
    "--summary",
  ]);
  for (let index = 0; index < args.length; index += 1) {
    const option = args[index];
    if (!allowed.has(option)) fail(`Unknown option: ${option}`);
    const value = args[index + 1];
    if (!value || value.startsWith("--")) fail(`Missing value for ${option}`);
    options[option.slice(2).replaceAll("-", "_")] = value;
    index += 1;
  }
  for (const name of [
    "line_branch_lcov",
    "functions_json",
    "profile",
    "expected_profile_sha256",
    "source_digest",
    "source_manifest",
    "line_branch_receipt",
    "functions_receipt",
    "output",
    "summary",
  ]) {
    if (!options[name]) fail(`Missing --${name.replaceAll("_", "-")}`);
  }
  return options;
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  const result = normalizeRustCoverageLcov({
    lineBranchLcov: options.line_branch_lcov,
    functionsJson: options.functions_json,
    profile: options.profile,
    expectedProfileSha256: options.expected_profile_sha256,
    sourceDigest: options.source_digest,
    sourceManifest: options.source_manifest,
    lineBranchReceipt: options.line_branch_receipt,
    functionsReceipt: options.functions_receipt,
  });
  for (const [path, contents] of [
    [options.output, result.output],
    [options.summary, `${JSON.stringify(result.summary, null, 2)}\n`],
  ]) {
    const resolved = resolve(path);
    mkdirSync(dirname(resolved), { recursive: true });
    writeFileSync(resolved, contents);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    main();
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
