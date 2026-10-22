import assert from "node:assert/strict";
import {
  chmodSync,
  existsSync,
  mkdirSync,
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
  coverageAvailability,
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

test("zero-observation LCOV metrics are reported as unavailable and cannot hide an observed baseline", () => {
  const observed = {
    lines: { found: 1, hit: 1 },
    branches: { found: 1, hit: 1 },
    functions: { found: 1, hit: 1 },
  };
  const unavailable = {
    lines: { found: 1, hit: 1 },
    branches: { found: 0, hit: 0 },
    functions: { found: 0, hit: 0 },
  };
  assert.deepEqual(coverageAvailability(unavailable), {
    lines: { status: "observed" },
    branches: {
      status: "unavailable",
      reason: "LCOV reported no observable items for this metric",
    },
    functions: {
      status: "unavailable",
      reason: "LCOV reported no observable items for this metric",
    },
  });
  assert.match(
    compareCoverage(
      { components: { rust: unavailable, frontend: observed } },
      { components: { rust: observed, frontend: observed } },
    ).join("\n"),
    /rust branches is unavailable in the candidate/,
  );
});

test("coverage CLI emits a reviewable candidate and never creates a baseline", () => {
  const directory = mkdtempSync(join(tmpdir(), "dakia-coverage-ratchet-"));
  try {
    const lcov = join(directory, "coverage.lcov");
    const output = join(directory, "candidate.json");
    writeFileSync(lcov, "SF:src/a.rs\nDA:1,1\nend_of_record\n");
    const outputText = execFileSync(
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
      { cwd: root, encoding: "utf8" },
    );
    assert.match(outputText, /unavailable evidence, not 100% coverage/);
    assert.deepEqual(
      JSON.parse(readFileSync(output, "utf8")).observations.rust.branches,
      {
        status: "unavailable",
        reason: "LCOV reported no observable items for this metric",
      },
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

test("disposable Gmail smoke runs a private binary copy after the shared target is replaced", () => {
  const directory = mkdtempSync(join(tmpdir(), "dakia-gmail-smoke-runner-"));
  try {
    const scriptDirectory = join(directory, "scripts");
    const targetDirectory = join(directory, "target", "debug");
    const toolDirectory = join(directory, "tools");
    const temporaryDirectory = join(directory, "tmp");
    mkdirSync(scriptDirectory, { recursive: true });
    mkdirSync(targetDirectory, { recursive: true });
    mkdirSync(toolDirectory, { recursive: true });
    mkdirSync(temporaryDirectory, { recursive: true });
    const runner = join(scriptDirectory, "run-disposable-gmail-smoke.sh");
    const target = join(targetDirectory, "dakia-provider-smoke");
    const witness = join(directory, "executed-paths.txt");
    writeFileSync(
      runner,
      readFileSync(join(root, "scripts/run-disposable-gmail-smoke.sh")),
    );
    chmodSync(runner, 0o700);
    writeFileSync(
      target,
      `#!/usr/bin/env bash
set -euo pipefail
printf '%s\\n' "$0" >> "$FAKE_PROVIDER_WITNESS"
if [[ "$1" == "--write-gmail-template" ]]; then
  : > "$2"
  cat > "$FAKE_PROVIDER_TARGET" <<'MUTABLE'
#!/usr/bin/env bash
echo mutable-target-ran >&2
exit 97
MUTABLE
  chmod 700 "$FAKE_PROVIDER_TARGET"
  exit 0
fi
while [[ "$#" -gt 0 ]]; do
  if [[ "$1" == "--evidence-report" ]]; then
    printf '{"outcome":"passed"}\\n' > "$2"
    break
  fi
  shift
done
`,
    );
    chmodSync(target, 0o700);
    writeFileSync(
      join(toolDirectory, "cargo"),
      "#!/usr/bin/env bash\nexit 0\n",
    );
    writeFileSync(
      join(toolDirectory, "git"),
      "#!/usr/bin/env bash\nprintf '%040d\\n' 1\n",
    );
    chmodSync(join(toolDirectory, "cargo"), 0o700);
    chmodSync(join(toolDirectory, "git"), 0o700);
    const result = spawnSync("bash", [runner], {
      cwd: directory,
      encoding: "utf8",
      input: "isolated.runner@gmail.com\n",
      env: {
        ...process.env,
        PATH: `${toolDirectory}:${process.env.PATH}`,
        TMPDIR: temporaryDirectory,
        FAKE_PROVIDER_TARGET: target,
        FAKE_PROVIDER_WITNESS: witness,
      },
    });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.doesNotMatch(
      `${result.stdout}${result.stderr}`,
      /mutable-target-ran/,
    );
    const executions = readFileSync(witness, "utf8").trim().split("\n");
    assert.equal(executions.length, 2);
    assert.ok(
      executions.every((path) => path.includes("dakia-gmail-smoke-config.")),
      executions.join("\n"),
    );
    assert.ok(executions.every((path) => path !== target));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("manual workflow remains dispatch-only and runs bounded infrastructure", () => {
  const workflow = readFileSync(
    join(root, ".github/workflows/manual-validation.yml"),
    "utf8",
  );
  assert.match(workflow, /^\s*workflow_dispatch:/m);
  assert.doesNotMatch(workflow, /^\s*schedule:/m);
  assert.doesNotMatch(
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
  const cacheStartStep = fullSourceJob.slice(
    fullSourceJob.indexOf("- name: Start private R2 compiler cache"),
    fullSourceJob.indexOf("- name: Install Linux dependencies"),
  );
  const fullSuiteStep = fullSourceJob.slice(
    fullSourceJob.indexOf("- name: Run full source suite"),
    fullSourceJob.indexOf("- name: Report Rust compiler cache statistics"),
  );
  assert.match(cacheStartStep, /sccache --start-server/);
  assert.match(cacheStartStep, /AWS_ACCESS_KEY_ID:/);
  assert.match(cacheStartStep, /AWS_SECRET_ACCESS_KEY:/);
  assert.doesNotMatch(
    fullSuiteStep,
    /AWS_ACCESS_KEY_ID|AWS_SECRET_ACCESS_KEY|R2_SCCACHE_/,
    "the source suite must connect to the cache server without inheriting R2 credentials",
  );
  assert.equal(
    [
      ...fullSourceJob.matchAll(
        /R2_SCCACHE_(?:ACCESS_KEY_ID|SECRET_ACCESS_KEY)/g,
      ),
    ].length,
    2,
    "R2 credentials must be scoped to the cache-server startup step",
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

test("coverage lane uses branch-capable Rust coverage and a lock-matched Vitest provider", () => {
  const workflow = readFileSync(
    join(root, ".github/workflows/manual-validation.yml"),
    "utf8",
  );
  const lock = JSON.parse(
    readFileSync(join(root, "package-lock.json"), "utf8"),
  );
  const lockedVitest = lock.packages["node_modules/vitest"]?.version;
  assert.match(
    lockedVitest ?? "",
    /^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/,
    "the workflow must derive a well-formed locked Vitest version",
  );

  const coverageJob = workflow.slice(
    workflow.indexOf("\n  coverage:"),
    workflow.indexOf("\n  fuzz:"),
  );
  const outsideCoverage = workflow.replace(coverageJob, "");
  assert.match(
    coverageJob,
    /rustup toolchain install nightly-2026-10-17 --profile minimal --component llvm-tools-preview/,
  );
  assert.match(coverageJob, /RUSTUP_TOOLCHAIN=nightly-2026-10-17/);
  assert.match(
    coverageJob,
    /cargo \+nightly-2026-10-17 install cargo-llvm-cov --version 0\.9\.1 --locked/,
  );
  assert.match(
    coverageJob,
    /cargo \+nightly-2026-10-17 llvm-cov --workspace --branch --locked --lcov --output-path coverage\/rust\.lcov/,
  );
  assert.doesNotMatch(
    outsideCoverage,
    /nightly-2026-10-17|cargo-llvm-cov --version 0\.9\.1|llvm-cov[^\n]*--branch/,
  );
  assert.equal(
    [
      ...coverageJob.matchAll(
        /cargo \+nightly-2026-10-17 llvm-cov --workspace --branch --locked --lcov/g,
      ),
    ].length,
    1,
    "only the coverage job may generate branch LCOV",
  );

  const ordinaryJobs = [
    [
      "full-source",
      workflow.slice(
        workflow.indexOf("\n  full-source:"),
        workflow.indexOf("\n  coverage:"),
      ),
    ],
    [
      "fuzz",
      workflow.slice(
        workflow.indexOf("\n  fuzz:"),
        workflow.indexOf("\n  provider-smoke:"),
      ),
    ],
    ["provider-smoke", workflow.slice(workflow.indexOf("\n  provider-smoke:"))],
  ];
  for (const [name, job] of ordinaryJobs) {
    assert.match(
      job,
      /rustup toolchain install 1\.89\.0 --profile minimal/,
      `${name} must remain on the supported stable Rust toolchain`,
    );
    assert.match(job, /RUSTUP_TOOLCHAIN=1\.89\.0/);
    assert.doesNotMatch(
      job,
      /nightly-2026-10-17|cargo-llvm-cov|llvm-cov[^\n]*--branch/,
    );
  }

  assert.match(
    coverageJob,
    /locked_vitest_version="\$\(node -p "require\('\.\/package-lock\.json'\)\.packages\['node_modules\/vitest'\]\.version"\)"/,
  );
  assert.match(
    coverageJob,
    /npm install --ignore-scripts --no-save --package-lock=false --no-audit --legacy-peer-deps "vitest@\$locked_vitest_version" "@vitest\/coverage-v8@\$locked_vitest_version"/,
  );
  assert.match(
    coverageJob,
    /installed_vitest_version="\$\(node -p "require\('\.\/node_modules\/vitest\/package\.json'\)\.version"\)"/,
  );
  assert.match(
    coverageJob,
    /installed_provider_version="\$\(node -p "require\('\.\/node_modules\/@vitest\/coverage-v8\/package\.json'\)\.version"\)"/,
  );
  assert.match(
    coverageJob,
    /test "\$installed_vitest_version" = "\$locked_vitest_version"/,
  );
  assert.match(
    coverageJob,
    /test "\$installed_provider_version" = "\$locked_vitest_version"/,
  );
  assert.doesNotMatch(
    coverageJob,
    /@vitest\/coverage-v8@(?!\$locked_vitest_version)/,
    "the coverage provider version must be lock-derived rather than hard-coded",
  );

  const coverageGenerationIndex = coverageJob.indexOf(
    "Generate Rust and frontend LCOV reports",
  );
  const providerInstallIndex = coverageJob.indexOf(
    "npm install --ignore-scripts --no-save --package-lock=false --no-audit",
  );
  const runtimeCheckIndex = coverageJob.indexOf(
    'test "$installed_vitest_version" = "$locked_vitest_version"',
  );
  const providerCheckIndex = coverageJob.indexOf(
    'test "$installed_provider_version" = "$locked_vitest_version"',
  );
  assert.ok(coverageGenerationIndex >= 0);
  for (const [name, index] of [
    ["provider install", providerInstallIndex],
    ["Vitest runtime check", runtimeCheckIndex],
    ["coverage provider check", providerCheckIndex],
  ]) {
    assert.ok(
      index >= 0 && index < coverageGenerationIndex,
      `${name} must run before coverage`,
    );
  }
});

test("coverage workflow rejects Rust LCOV without branch observations", () => {
  const workflow = readFileSync(
    join(root, ".github/workflows/manual-validation.yml"),
    "utf8",
  );
  const coverageJob = workflow.slice(
    workflow.indexOf("\n  coverage:"),
    workflow.indexOf("\n  fuzz:"),
  );
  assert.match(
    coverageJob,
    /uses: actions\/upload-artifact@[0-9a-f]+ # v4\.6\.0\n\s+if: always\(\)/,
  );
  const check = coverageJob.match(
    /node --input-type=module - coverage\/rust\.lcov <<'NODE'\n([\s\S]*?)\n\s+NODE/,
  )?.[1];
  assert.ok(
    check,
    "the coverage job must contain an executable Rust branch check",
  );
  assert.match(
    check,
    /coverageAvailability\(summary\)\.branches\.status !== "observed"/,
  );
  assert.match(
    check,
    /Rust branch LCOV observations are unavailable; coverage candidate cannot be accepted/,
  );

  const directory = mkdtempSync(join(tmpdir(), "dakia-rust-branch-lcov-"));
  try {
    const runCheck = (name, contents) => {
      const lcov = join(directory, name);
      writeFileSync(lcov, contents);
      return spawnSync(process.execPath, ["--input-type=module", "-", lcov], {
        cwd: root,
        encoding: "utf8",
        input: check,
      });
    };
    const observed = runCheck(
      "observed.lcov",
      "SF:src/a.rs\nDA:1,1\nBRDA:1,0,0,1\nend_of_record\n",
    );
    assert.equal(observed.status, 0, observed.stderr);

    const unavailable = runCheck(
      "unavailable.lcov",
      "SF:src/a.rs\nDA:1,1\nend_of_record\n",
    );
    assert.notEqual(unavailable.status, 0);
    assert.match(
      `${unavailable.stdout}${unavailable.stderr}`,
      /Rust branch LCOV observations are unavailable; coverage candidate cannot be accepted/,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
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
