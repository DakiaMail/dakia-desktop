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
  credentials: { appPassword: "not-a-real-app-password" },
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
    credentialKind: "appPassword",
  });
  assert.throws(
    () =>
      validateProviderSmokeContract({
        ...providerConfig,
        credentials: { appPassword: "x", password: "y" },
      }),
    /exactly one/,
  );
  assert.throws(
    () =>
      validateProviderSmokeContract({
        ...providerConfig,
        credentials: { accessToken: "not-a-real-token" },
      }),
    /password or appPassword/,
    "the live harness must reject OAuth-like credential fields instead of guessing a flow",
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
          credentials: { appPassword: secret },
        }),
      },
    },
  );
  assert.equal(result.status, 0, result.stderr);
  assert.doesNotMatch(`${result.stdout}${result.stderr}`, new RegExp(secret));
  assert.doesNotMatch(result.stdout, /smoke@example\.invalid/);
  assert.doesNotMatch(
    result.stdout,
    /example-provider|imap\.example\.invalid|smtp\.example\.invalid/,
  );

  const rejected = spawnSync(
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
  assert.notEqual(rejected.status, 0);
  assert.doesNotMatch(
    `${rejected.stdout}${rejected.stderr}`,
    new RegExp(secret),
  );
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
  assert.match(workflow, /dakia-provider-smoke/);
  assert.match(workflow, /RUSTUP_TOOLCHAIN=1\.89\.0/);
  assert.match(
    workflow,
    /restore-keys: \|\n\s+npm-\$\{\{ runner\.os \}\}-\$\{\{ runner\.arch \}\}-node-20\.19\.0-/,
  );
  assert.match(workflow, /environment: sccache-r2/);
  assert.match(workflow, /RUSTC_WRAPPER: sccache/);
  assert.match(workflow, /CARGO_INCREMENTAL: 0/);
  assert.match(workflow, /SCCACHE_BUCKET: dakia-sccache/);
  assert.match(
    workflow,
    /SCCACHE_ENDPOINT: https:\/\/b225fd2027198472b627795dd126aa15\.r2\.cloudflarestorage\.com/,
  );
  assert.match(workflow, /SCCACHE_REGION: auto/);
  assert.match(
    workflow,
    /SCCACHE_S3_KEY_PREFIX: dakia\/linux-x64\/rust-1\.89\.0\//,
  );
  assert.match(workflow, /SCCACHE_S3_RW_MODE: READ_WRITE/);
  assert.match(
    workflow,
    /AWS_ACCESS_KEY_ID: \$\{\{ secrets\.R2_SCCACHE_ACCESS_KEY_ID \}\}/,
  );
  assert.match(
    workflow,
    /AWS_SECRET_ACCESS_KEY: \$\{\{ secrets\.R2_SCCACHE_SECRET_ACCESS_KEY \}\}/,
  );
  assert.match(
    workflow,
    /name: Report Rust compiler cache statistics\n\s+if: always\(\)\n\s+run: sccache --show-stats/,
  );
  assert.match(
    workflow,
    /sccache-v0\.17\.0-x86_64-unknown-linux-musl\.tar\.gz/,
  );
  assert.match(
    workflow,
    /67c4a96dd237c1f518f6b36083f270f9976d516f1e57fce891755ea782e50006/,
  );
  assert.doesNotMatch(workflow, /SCCACHE_GHA_ENABLED|SCCACHE_GHA_RW_MODE/);
  assert.doesNotMatch(workflow, /cargo install sccache/);
  assert.doesNotMatch(
    workflow,
    /path: \|\n\s+~\/\.cargo\/registry\n\s+~\/\.cargo\/git\n\s+target/,
    "the immutable Actions cache must not store the entire target directory",
  );
  assert.match(
    workflow,
    /rustup toolchain install 1\.89\.0 --profile minimal --component rustfmt --component clippy/,
    "rustup requires one --component flag per requested component",
  );
  const providerJob = workflow.slice(workflow.indexOf("\n  provider-smoke:"));
  const fullSourceJob = workflow.slice(
    workflow.indexOf("\n  full-source:"),
    workflow.indexOf("\n  coverage:"),
  );
  assert.match(fullSourceJob, /environment: sccache-r2/);
  assert.match(fullSourceJob, /RUSTC_WRAPPER: sccache/);
  assert.match(fullSourceJob, /CARGO_INCREMENTAL: 0/);
  assert.equal(
    [
      ...fullSourceJob.matchAll(
        /R2_SCCACHE_(?:ACCESS_KEY_ID|SECRET_ACCESS_KEY)/g,
      ),
    ].length,
    2,
    "R2 credentials must be scoped to the one compiler-cache execution step",
  );
  assert.doesNotMatch(
    workflow.slice(0, workflow.indexOf("\n  full-source:")),
    /R2_SCCACHE_|RUSTC_WRAPPER: sccache/,
  );
  assert.doesNotMatch(providerJob, /R2_SCCACHE_|RUSTC_WRAPPER: sccache/);
  assert.match(
    providerJob,
    /cargo build --locked -p dakia-cli --bin dakia-provider-smoke/,
  );
  assert.doesNotMatch(
    providerJob,
    /cargo run .*dakia-provider-smoke/,
    "the protected execution step must run the prebuilt artifact, never compile with a secret in scope",
  );
  const buildIndex = providerJob.indexOf(
    "cargo build --locked -p dakia-cli --bin dakia-provider-smoke",
  );
  const secretIndex = providerJob.indexOf("PROVIDER_SMOKE_CONFIG:");
  assert.ok(buildIndex >= 0 && secretIndex > buildIndex);
  assert.equal(
    [...providerJob.matchAll(/PROVIDER_SMOKE_CONFIG:/g)].length,
    1,
    "the provider secret may be scoped only to the artifact execution step",
  );
  assert.match(
    providerJob,
    /run: \|\n\s+node scripts\/provider-smoke-contract\.mjs\n\s+target\/debug\/dakia-provider-smoke/,
  );
  assert.equal(
    [...workflow.matchAll(/npm run bundle:cli:dev/g)].length,
    2,
    "full-source and coverage must prepare Tauri externalBin",
  );
  assert.doesNotMatch(
    workflow,
    /--localstorage-file/,
    "Node 20 rejects --localstorage-file in NODE_OPTIONS on hosted runners",
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
