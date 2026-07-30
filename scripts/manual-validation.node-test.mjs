import assert from "node:assert/strict";
import {
  existsSync,
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
  compareCoverage,
  parseArgs,
  summarizeLcov,
} from "./coverage-ratchet.mjs";
import { validateProviderSmokeContract } from "./provider-smoke-contract.mjs";

const root = new URL("..", import.meta.url).pathname;

const providerConfig = {
  version: 1,
  provider: "example-provider",
  accountEmail: "smoke@example.invalid",
  imap: { host: "imap.example.invalid", port: 993, security: "tls" },
  smtp: { host: "smtp.example.invalid", port: 587, security: "starttls" },
  credentials: { accessToken: "not-a-real-token" },
};

test("LCOV summaries deduplicate records and ratchet exact ratios", () => {
  const summary = summarizeLcov(
    [
      "SF:src/a.rs",
      "DA:1,1",
      "DA:2,0",
      "BRDA:1,0,0,1",
      "BRDA:1,0,1,-",
      "FNDA:1,first",
      "FNDA:0,second",
      "end_of_record",
      "SF:src/a.rs",
      "DA:1,2",
      "end_of_record",
    ].join("\n"),
  );
  assert.deepEqual(summary, {
    lines: { found: 2, hit: 1 },
    branches: { found: 2, hit: 1 },
    functions: { found: 2, hit: 1 },
  });
  const candidate = { components: { rust: summary, frontend: summary } };
  assert.deepEqual(compareCoverage(candidate, candidate), []);
  assert.match(
    compareCoverage(
      {
        components: {
          rust: { ...summary, lines: { found: 2, hit: 0 } },
          frontend: summary,
        },
      },
      candidate,
    ).join("\n"),
    /rust lines regressed/,
  );
});

test("coverage arguments require both implementation reports and an output path", () => {
  assert.throws(() => parseArgs(["--rust-lcov", "rust.lcov"]), /frontend-lcov/);
  assert.deepEqual(
    parseArgs([
      "--rust-lcov",
      "rust.lcov",
      "--frontend-lcov",
      "frontend.lcov",
      "--output",
      "candidate.json",
    ]),
    {
      rust_lcov: "rust.lcov",
      frontend_lcov: "frontend.lcov",
      output: "candidate.json",
    },
  );
});

test("coverage CLI emits a reviewable candidate and never creates a baseline", () => {
  const directory = mkdtempSync(join(tmpdir(), "dakia-coverage-ratchet-"));
  try {
    const lcov = join(directory, "coverage.lcov");
    const output = join(directory, "candidate.json");
    writeFileSync(lcov, "SF:src/a.rs\nDA:1,1\nend_of_record\n");
    execFileSync(
      process.execPath,
      [
        "scripts/coverage-ratchet.mjs",
        "--rust-lcov",
        lcov,
        "--frontend-lcov",
        lcov,
        "--output",
        output,
      ],
      { cwd: root, stdio: "pipe" },
    );
    assert.deepEqual(
      JSON.parse(readFileSync(output, "utf8")).components.rust.lines,
      { found: 1, hit: 1 },
    );
    assert.equal(existsSync(join(directory, "baseline.json")), false);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("provider smoke contract validates secrets without exposing them or connecting", () => {
  assert.deepEqual(validateProviderSmokeContract(providerConfig), {
    version: 1,
    provider: "example-provider",
    accountEmail: "smoke@example.invalid",
    imap: providerConfig.imap,
    smtp: providerConfig.smtp,
    credentialKind: "accessToken",
  });
  assert.throws(
    () =>
      validateProviderSmokeContract({
        ...providerConfig,
        credentials: { accessToken: "x", password: "y" },
      }),
    /exactly one/,
  );
  const secret = "must-not-appear-in-output";
  const result = spawnSync(
    process.execPath,
    ["scripts/provider-smoke-contract.mjs"],
    {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        PROVIDER_SMOKE_CONFIG: JSON.stringify({
          ...providerConfig,
          credentials: { accessToken: secret },
        }),
      },
    },
  );
  assert.equal(result.status, 0, result.stderr);
  assert.doesNotMatch(`${result.stdout}${result.stderr}`, new RegExp(secret));
  assert.doesNotMatch(result.stdout, /smoke@example\.invalid/);
});

test("manual workflow remains dispatch-only and runs bounded infrastructure", () => {
  const workflow = readFileSync(
    join(root, ".github/workflows/manual-validation.yml"),
    "utf8",
  );
  assert.match(workflow, /^\s*workflow_dispatch:/m);
  assert.doesNotMatch(workflow, /^\s*schedule:/m);
  assert.match(
    workflow,
    /cargo install cargo-llvm-cov --version 0\.6\.13 --locked/,
  );
  assert.match(
    workflow,
    /git lfs pull --include='apps\/desktop\/src-tauri\/resources\/email-classifier-v2\/model\.onnx/,
  );
  assert.match(workflow, /run-fixed-seed-property-regressions\.sh 3/);
  assert.match(workflow, /provider-smoke-contract\.mjs/);
  assert.equal(
    [...workflow.matchAll(/npm run bundle:cli:dev/g)].length,
    2,
    "full-source and coverage must prepare Tauri externalBin",
  );
});

test("fixed-seed lane executes the property targets and exact MIME regression", () => {
  const script = readFileSync(
    join(root, "scripts/run-fixed-seed-property-regressions.sh"),
    "utf8",
  );
  assert.match(script, /threadsProperties\.test\.ts/);
  assert.match(script, /--test mail_boundary_properties/);
  assert.match(
    script,
    /mail::tests::parses_the_checked_in_mime_regression_corpus/,
  );
  assert.doesNotMatch(
    script,
    /cargo test -p dakia-core parses_the_checked_in_mime_regression_corpus/,
  );
});
