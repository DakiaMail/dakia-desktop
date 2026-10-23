import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  changedPathsFromMergeBase,
  classifyPaths,
} from "./change-classifier.mjs";

const root = new URL("..", import.meta.url).pathname;
const classifier = join(root, "scripts", "change-classifier.mjs");
const scopeNames = [
  "frontend",
  "rust-core",
  "mail-boundary",
  "tauri-boundary",
  "release-only",
  "dependency-audit",
];

function git(cwd, ...args) {
  return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
}

function commit(cwd, message) {
  git(cwd, "add", ".");
  git(cwd, "commit", "-qm", message);
}

test("classifies representative paths and promotes shared or unknown paths", () => {
  const cases = [
    {
      name: "documentation is docs-only",
      paths: ["README.md", "docs/testing-strategy.md", ".github/CODEOWNERS"],
      expected: { docsOnly: true, activeScopes: [] },
    },
    {
      name: "reader changes cover frontend and mail boundaries",
      paths: ["apps/desktop/src/components/Reader.test.tsx"],
      expected: { activeScopes: ["frontend", "mail-boundary"] },
    },
    {
      name: "core MIME fixtures cover rust and mail boundaries",
      paths: ["crates/dakia-core/testdata/mime/provider-shape.eml"],
      expected: { activeScopes: ["rust-core", "mail-boundary"] },
    },
    {
      name: "fixture validator changes exercise the mail boundary",
      paths: ["scripts/validate-realistic-fixtures.node-test.mjs"],
      expected: { activeScopes: ["mail-boundary"] },
    },
    {
      name: "mail safety catalogue changes exercise its source-level validator",
      paths: ["testdata/mail-safety-catalogue.json"],
      expected: { activeScopes: ["mail-boundary"] },
    },
    {
      name: "mail safety evidence documentation cannot bypass its validator",
      paths: ["docs/mail-safety-evidence.md"],
      expected: { activeScopes: ["mail-boundary"] },
    },
    {
      name: "standards requirement matrix changes run its boundary and contract gates",
      paths: ["testdata/mail-standards-requirements.json"],
      expected: { activeScopes: ["mail-boundary", "release-only"] },
    },
    {
      name: "standards validator changes run its boundary and contract gates",
      paths: ["scripts/validate-mail-standards-requirements.mjs"],
      expected: { activeScopes: ["mail-boundary", "release-only"] },
    },
    {
      name: "standards profile changes cannot be treated as documentation only",
      paths: ["docs/mail-safety-standards-profile.md"],
      expected: { activeScopes: ["mail-boundary"] },
    },
    {
      name: "Tauri inventory changes exercise the Tauri boundary",
      paths: ["scripts/tauri-contract-inventory.mjs"],
      expected: { activeScopes: ["tauri-boundary"] },
    },
    {
      name: "coupled realtime delivery script exercises the Tauri boundary",
      paths: ["scripts/test-realtime-delivery.mjs"],
      expected: { activeScopes: ["tauri-boundary"] },
    },
    {
      name: "MIME artifact runner keeps core, Reader, mail, and Tauri gates coupled",
      paths: ["scripts/test-mime-reader-artifact.mjs"],
      expected: {
        activeScopes: [
          "frontend",
          "rust-core",
          "mail-boundary",
          "tauri-boundary",
        ],
      },
    },
    {
      name: "attachment artifact runner keeps core, Reader, mail, and Tauri gates coupled",
      paths: ["scripts/test-attachment-command-artifact.mjs"],
      expected: {
        activeScopes: [
          "frontend",
          "rust-core",
          "mail-boundary",
          "tauri-boundary",
        ],
      },
    },
    {
      name: "attachment fixture server keeps core, Reader, mail, and Tauri gates coupled",
      paths: ["scripts/attachment-command-fixture-server.mjs"],
      expected: {
        activeScopes: [
          "frontend",
          "rust-core",
          "mail-boundary",
          "tauri-boundary",
        ],
      },
    },
    {
      name: "mail command artifact runner keeps Rust, mail, and Tauri gates coupled",
      paths: ["scripts/test-mail-command-tls.mjs"],
      expected: {
        activeScopes: ["rust-core", "mail-boundary", "tauri-boundary"],
      },
    },
    {
      name: "mail command fixture server keeps Rust, mail, and Tauri gates coupled",
      paths: ["scripts/mail-command-tls-fixture-server.mjs"],
      expected: {
        activeScopes: ["rust-core", "mail-boundary", "tauri-boundary"],
      },
    },
    {
      name: "unsubscribe command runner and owned fixture scripts keep Rust, mail, and Tauri gates coupled",
      paths: [
        "scripts/test-unsubscribe-command-tls.mjs",
        "scripts/test-unsubscribe-command-tls.node-test.mjs",
        "scripts/unsubscribe-command-tls-fixture-server.mjs",
        "scripts/unsubscribe-command-tls-fixture-server.node-test.mjs",
      ],
      expected: {
        activeScopes: [
          "rust-core",
          "mail-boundary",
          "tauri-boundary",
          "release-only",
        ],
      },
    },
    {
      name: "unsubscribe command fixture and dispatcher cannot bypass mail or Tauri gates",
      paths: [
        "crates/dakia-core/src/native_mail_safety_unsubscribe_fixture.rs",
        "apps/desktop/src-tauri/src/unsubscribe_command_tls_tests.rs",
      ],
      expected: {
        activeScopes: ["rust-core", "mail-boundary", "tauri-boundary"],
      },
    },
    {
      name: "artifact Reader fixture tests cannot bypass their coupled Tauri runner",
      paths: ["apps/desktop/src/components/attachmentCommandArtifact.test.tsx"],
      expected: { activeScopes: ["frontend", "tauri-boundary"] },
    },
    {
      name: "core MIME artifact export cannot bypass its coupled Tauri runner",
      paths: [
        "crates/dakia-core/src/mail/tests/mime_tauri_artifact_export_tests.rs",
      ],
      expected: {
        activeScopes: ["rust-core", "mail-boundary", "tauri-boundary"],
      },
    },
    {
      name: "shared Tauri contract fixtures exercise both consumers",
      paths: ["apps/desktop/testdata/tauri-contracts/high-risk.json"],
      expected: { activeScopes: ["frontend", "tauri-boundary"] },
    },
    {
      name: "frontend invoke sources cannot bypass Tauri inventory",
      paths: ["apps/desktop/src/api.ts"],
      expected: {
        activeScopes: ["frontend", "mail-boundary", "tauri-boundary"],
      },
    },
    {
      name: "Tauri origin inventory exercises both consumers",
      paths: ["apps/desktop/testdata/tauri-contract-origins.json"],
      expected: { activeScopes: ["frontend", "tauri-boundary"] },
    },
    {
      name: "tauri configuration includes release validation",
      paths: ["apps/desktop/src-tauri/tauri.conf.json"],
      expected: {
        activeScopes: ["rust-core", "tauri-boundary", "release-only"],
      },
    },
    {
      name: "classifier changes cannot narrow the required lane",
      paths: ["scripts/change-classifier.mjs"],
      expected: { activeScopes: scopeNames },
    },
    {
      name: "vendored source and its integrity manifest cannot bypass dependency audit",
      paths: [
        "vendor/lettre/src/transport/smtp/client/mod.rs",
        "testdata/vendored-crates/lettre.json",
      ],
      expected: { activeScopes: scopeNames },
    },
    {
      name: "the dependency audit gate cannot be changed without selecting itself",
      paths: ["scripts/audit-dependencies.sh"],
      expected: { activeScopes: scopeNames },
    },
    {
      name: "unknown paths fail safe to every automatic scope",
      paths: ["new-top-level-area/input.txt"],
      expected: { activeScopes: scopeNames },
    },
  ];

  for (const { name, paths, expected } of cases) {
    const result = classifyPaths(paths);
    assert.equal(result.docsOnly, expected.docsOnly ?? false, name);
    assert.deepEqual(
      result.scopes,
      Object.fromEntries(
        scopeNames.map((scope) => [
          scope,
          expected.activeScopes.includes(scope),
        ]),
      ),
      name,
    );
  }

  const lfsResult = classifyPaths([
    "apps/desktop/src-tauri/resources/email-classifier-v2/model.onnx",
  ]);
  assert.equal(lfsResult.requiresLfs, false);
  assert.equal(lfsResult.scopes["mail-boundary"], true);
  assert.equal(
    classifyPaths(["crates/dakia-cli/src/main.rs"]).requiresLfs,
    false,
  );
  assert.equal(
    classifyPaths(["Cargo.lock"]).scopes["dependency-audit"],
    true,
    "lockfile changes must run the dependency audit lane",
  );
});

test("each unsubscribe TLS command boundary input selects the executable coupled scopes", () => {
  const coupled = Object.fromEntries(
    scopeNames.map((scope) => [
      scope,
      ["rust-core", "mail-boundary", "tauri-boundary"].includes(scope),
    ]),
  );
  for (const path of [
    "scripts/test-unsubscribe-command-tls.mjs",
    "scripts/unsubscribe-command-tls-fixture-server.mjs",
    "crates/dakia-core/src/native_mail_safety_unsubscribe_fixture.rs",
    "apps/desktop/src-tauri/src/unsubscribe_command_tls_tests.rs",
    "crates/dakia-core/src/lib.rs",
  ]) {
    assert.deepEqual(classifyPaths([path]).scopes, coupled, path);
  }

  const sharedLifecycle = Object.fromEntries(
    scopeNames.map((scope) => [
      scope,
      [
        "rust-core",
        "frontend",
        "mail-boundary",
        "tauri-boundary",
        "release-only",
      ].includes(scope),
    ]),
  );
  assert.deepEqual(
    classifyPaths(["scripts/fixture-process-lifecycle.mjs"]).scopes,
    sharedLifecycle,
  );

  const nodeTests = Object.fromEntries(
    scopeNames.map((scope) => [
      scope,
      ["rust-core", "mail-boundary", "tauri-boundary", "release-only"].includes(
        scope,
      ),
    ]),
  );
  for (const path of [
    "scripts/test-unsubscribe-command-tls.node-test.mjs",
    "scripts/unsubscribe-command-tls-fixture-server.node-test.mjs",
  ]) {
    assert.deepEqual(classifyPaths([path]).scopes, nodeTests, path);
  }
});

test("uses the merge base rather than unrelated commits on the base branch", () => {
  const directory = mkdtempSync(join(tmpdir(), "dakia-change-classifier-"));
  try {
    git(directory, "init", "-q");
    git(directory, "config", "user.email", "tests@example.invalid");
    git(directory, "config", "user.name", "Dakia test");
    writeFileSync(join(directory, "README.md"), "initial\n");
    commit(directory, "initial");
    git(directory, "branch", "-M", "main");
    git(directory, "checkout", "-qb", "topic");
    writeFileSync(join(directory, "README.md"), "topic documentation\n");
    commit(directory, "topic docs");
    git(directory, "checkout", "-q", "main");
    writeFileSync(join(directory, "Cargo.toml"), "[workspace]\n");
    commit(directory, "base rust change");

    const diff = changedPathsFromMergeBase({
      base: "main",
      head: "topic",
      cwd: directory,
    });
    assert.deepEqual(diff.changedPaths, ["README.md"]);
    assert.equal(classifyPaths(diff.changedPaths).docsOnly, true);

    const result = spawnSync(
      process.execPath,
      [classifier, "--base", "main", "--head", "topic", "--json"],
      {
        cwd: directory,
        encoding: "utf8",
      },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout).changedPaths, ["README.md"]);

    const githubOutput = join(directory, "github-output.txt");
    const outputResult = spawnSync(
      process.execPath,
      [classifier, "--base", "main", "--head", "topic", "--github-output"],
      {
        cwd: directory,
        encoding: "utf8",
        env: { ...process.env, GITHUB_OUTPUT: githubOutput },
      },
    );
    assert.equal(outputResult.status, 0, outputResult.stderr);
    assert.match(readFileSync(githubOutput, "utf8"), /^docs_only=true$/m);
    assert.match(readFileSync(githubOutput, "utf8"), /^frontend=false$/m);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("requires a GitHub output file when asked to publish workflow outputs", () => {
  const result = spawnSync(
    process.execPath,
    [classifier, "--base", "HEAD", "--github-output"],
    {
      cwd: root,
      encoding: "utf8",
      env: { ...process.env, GITHUB_OUTPUT: "" },
    },
  );
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /requires GITHUB_OUTPUT/);
});

test("ordinary pull-request workflow preserves the cost and release boundaries", () => {
  const workflow = readFileSync(
    join(root, ".github/workflows/pull-request-validation.yml"),
    "utf8",
  );
  assert.match(workflow, /cancel-in-progress: true/);
  const jobs = workflow.slice(
    workflow.indexOf("\njobs:\n") + "\njobs:\n".length,
  );
  assert.deepEqual(
    [...jobs.matchAll(/^  ([a-z][a-z0-9_-]+):$/gm)].map((match) => match[1]),
    ["analytics", "classify", "validate_non_tauri", "tauri", "validate"],
  );
  assert.match(
    workflow,
    /analytics:\n\s+name: Validate analytics collector[\s\S]*?node-version: 22\.19\.0[\s\S]*?npm run deploy:dry-run/,
    "the privacy-sensitive collector must keep its separate Node 22 validation gate",
  );
  assert.match(
    workflow,
    /validate_non_tauri:\n\s+name: Validate non-Tauri scopes[\s\S]*?timeout-minutes: 20/,
    "the cold-cache activation run must have time to populate the shared Cargo cache",
  );
  const requiredGate = workflow.match(/\n  validate:\n([\s\S]*)$/)?.[1];
  assert.ok(requiredGate, "expected the required validation gate");
  assert.match(requiredGate, /name: Validate applicable scopes/);
  assert.match(
    requiredGate,
    /needs: \[analytics, classify, validate_non_tauri, tauri\]/,
  );
  assert.match(requiredGate, /if: always\(\)/);
  assert.match(requiredGate, /timeout-minutes: 2/);
  assert.match(
    requiredGate,
    /ANALYTICS_RESULT: \$\{\{ needs\.analytics\.result \}\}/,
  );
  assert.match(
    requiredGate,
    /CLASSIFY_RESULT: \$\{\{ needs\.classify\.result \}\}/,
  );
  assert.match(
    requiredGate,
    /NON_TAURI_RESULT: \$\{\{ needs\.validate_non_tauri\.result \}\}/,
  );
  assert.match(
    requiredGate,
    /TAURI_SELECTED: \$\{\{ needs\.classify\.outputs\.tauri_boundary \}\}/,
  );
  assert.match(requiredGate, /TAURI_RESULT: \$\{\{ needs\.tauri\.result \}\}/);
  assert.match(
    requiredGate,
    /test "\$ANALYTICS_RESULT" = success[\s\S]*test "\$CLASSIFY_RESULT" = success[\s\S]*test "\$NON_TAURI_RESULT" = success[\s\S]*if \[ "\$TAURI_SELECTED" = true \]; then[\s\S]*test "\$TAURI_RESULT" = success[\s\S]*else[\s\S]*test "\$TAURI_RESULT" = skipped/,
    "the required context must reject any selected lane that did not pass",
  );
  assert.equal(
    [...workflow.matchAll(/^    name: Validate applicable scopes$/gm)].length,
    1,
    "the branch-protection context must be emitted by exactly one job",
  );
  const requiredGateRunBlock = workflowRunBlock(workflow, "enforce_results");
  const runRequiredGate = (overrides = {}) =>
    spawnSync("bash", ["-euc", requiredGateRunBlock], {
      encoding: "utf8",
      env: {
        ...process.env,
        ANALYTICS_RESULT: "success",
        CLASSIFY_RESULT: "success",
        NON_TAURI_RESULT: "success",
        TAURI_SELECTED: "true",
        TAURI_RESULT: "success",
        ...overrides,
      },
    });
  assert.equal(runRequiredGate().status, 0);
  for (const failure of [
    { ANALYTICS_RESULT: "failure" },
    { CLASSIFY_RESULT: "failure" },
    { NON_TAURI_RESULT: "failure" },
    { TAURI_RESULT: "failure" },
    { TAURI_RESULT: "skipped" },
  ]) {
    assert.notEqual(runRequiredGate(failure).status, 0);
  }
  assert.equal(
    runRequiredGate({ TAURI_SELECTED: "false", TAURI_RESULT: "skipped" })
      .status,
    0,
  );
  assert.notEqual(
    runRequiredGate({ TAURI_SELECTED: "false", TAURI_RESULT: "success" })
      .status,
    0,
  );
  assert.doesNotMatch(workflow, /runs-on:\s*macos/i);
  assert.doesNotMatch(workflow, /setup:worktree/);
  assert.doesNotMatch(workflow, /git lfs (?:pull|fetch)/);
  assert.doesNotMatch(
    workflow,
    /release:(?:build|publish)|notari[sz]|codesign|live-provider/i,
  );
  assert.doesNotMatch(
    workflow,
    /--localstorage-file/,
    "Node 20 rejects --localstorage-file in NODE_OPTIONS on hosted runners",
  );
  assert.match(
    workflow,
    /rustup toolchain install 1\.89\.0 --profile minimal --component rustfmt --component clippy/,
    "rustup requires one --component flag per requested component",
  );
  assert.match(
    workflow,
    /restore-keys: \|\n\s+npm-\$\{\{ runner\.os \}\}-\$\{\{ runner\.arch \}\}-node-20\.19\.0-/,
  );
  assert.match(
    workflow,
    /cargo-deps-\$\{\{ runner\.os \}\}-\$\{\{ runner\.arch \}\}-rust-1\.89\.0-/,
  );
  assert.doesNotMatch(workflow, /sccache/);
  assert.doesNotMatch(
    workflow,
    /path: \|\n\s+~\/\.cargo\/registry\n\s+~\/\.cargo\/git\n\s+target/,
    "the immutable Actions cache must not store the entire target directory",
  );
  for (const testName of [
    "local_model_and_policy_classify_the_production_regression_corpus",
    "model_and_policy_preserve_representative_categories",
  ]) {
    assert.match(
      workflow,
      new RegExp(`--skip classification::model_integration_tests::${testName}`),
      `the LFS-free PR lane must skip ${testName}`,
    );
  }
  for (const action of workflow.matchAll(/uses:\s*[^@\s]+@([^\s#]+)/g)) {
    assert.match(action[1], /^[a-f0-9]{40}$/, `unpinned action: ${action[0]}`);
  }
  assert.match(
    workflow,
    /npm run bundle:cli:dev\s+cargo clippy -p dakia-desktop[\s\S]*cargo test -p dakia-desktop\s/,
  );
  assert.match(
    workflow,
    /id: frontend[\s\S]*npm run test[\s\S]*node --test scripts\/tauri-contract-inventory\.node-test\.mjs[\s\S]*node scripts\/tauri-contract-inventory\.mjs/,
  );
  assert.match(
    workflow,
    /id: mail_boundary[\s\S]*node --test scripts\/mail-safety-catalogue\.node-test\.mjs[\s\S]*node scripts\/validate-mail-safety-catalogue\.mjs[\s\S]*node --test scripts\/validate-mail-standards-requirements\.node-test\.mjs[\s\S]*node scripts\/validate-mail-standards-requirements\.mjs[\s\S]*node scripts\/report-mail-safety-catalogue\.mjs --lane pull-request/,
  );
  assert.match(
    workflow,
    /name: Audit changed dependency lockfiles[\s\S]*cargo \+1\.89\.0 install cargo-audit --version 0\.22\.2 --locked[\s\S]*npm run audit:dependencies:npm/,
  );
  const tauriJob = workflow.match(/\n  tauri:\n([\s\S]*)$/)?.[1];
  assert.ok(tauriJob, "expected a separate Tauri boundary job");
  assert.match(tauriJob, /name: Validate Linux Tauri boundary/);
  assert.match(tauriJob, /needs: classify/);
  assert.match(
    tauriJob,
    /if: needs\.classify\.outputs\.tauri_boundary == 'true'/,
  );
  assert.match(tauriJob, /runs-on: ubuntu-24\.04/);
  assert.match(tauriJob, /timeout-minutes: 20/);
  assert.match(
    tauriJob,
    /printf '%s\\n' 'http:\/\/archive\.ubuntu\.com\/ubuntu' \| sudo tee \/etc\/apt\/apt-mirrors\.txt >\/dev\/null/,
    "the Tauri job must avoid the runner's intermittently slow Azure mirror",
  );
  assert.equal(
    [...tauriJob.matchAll(/Acquire::Retries=0/g)].length,
    2,
    "both apt update and install must disable adaptive retries",
  );
  assert.match(
    tauriJob,
    /sudo timeout --signal=TERM --kill-after=15s 360s/,
    "dependency acquisition must fail within a deterministic bound",
  );
  assert.match(tauriJob, /--yes --no-install-recommends/);
  const tauriSteps = tauriJob.slice(
    tauriJob.indexOf("\n    steps:\n") + "\n    steps:\n".length,
  );
  assert.doesNotMatch(
    tauriSteps,
    /\n        (?:continue-on-error|if):/,
    "selected Tauri steps must not be skipped or softened",
  );
  for (const pkgConfigName of [
    "gtk+-3.0",
    "webkit2gtk-4.1",
    "ayatana-appindicator3-0.1",
    "librsvg-2.0",
  ]) {
    assert.match(
      tauriJob,
      new RegExp(pkgConfigName.replaceAll("+", "\\+")),
      `the Tauri job must verify ${pkgConfigName}`,
    );
  }
  const tauriRunBlock = workflowRunBlock(workflow, "tauri_boundary");
  assert.deepEqual(tauriRunBlock.split("\n"), [
    "npm run bundle:cli:dev",
    "cargo clippy -p dakia-desktop --all-targets -- -D warnings",
    "cargo test -p dakia-desktop",
    "node scripts/test-realtime-delivery.mjs",
    "node scripts/test-mime-reader-artifact.mjs",
    "node scripts/test-attachment-command-artifact.mjs",
    "node scripts/test-mail-command-tls.mjs",
    "node scripts/test-unsubscribe-command-tls.mjs",
    "cargo test --locked -p tauri-plugin-updater --lib",
  ]);
  assert.doesNotMatch(tauriRunBlock, /tauri-contract-inventory/);
  assert.doesNotMatch(
    workflow,
    /cargo test -p dakia-desktop tauri_contracts_tests/,
  );
});

function workflowRunBlock(workflow, stepId) {
  const stepStart = workflow.indexOf(`      - id: ${stepId}\n`);
  assert.ok(stepStart >= 0, `expected workflow step ${stepId}`);
  const runStart = workflow.indexOf("        run: |\n", stepStart);
  assert.ok(runStart >= 0, `expected run block for ${stepId}`);
  const blockStart = runStart + "        run: |\n".length;
  const nextStep = workflow.indexOf("\n      - ", blockStart);
  const nextJobMatch = workflow
    .slice(blockStart)
    .match(/\n  [a-z][a-z0-9_-]*:\n/);
  const nextJob =
    nextJobMatch?.index === undefined ? -1 : blockStart + nextJobMatch.index;
  const blockEnd = [nextStep, nextJob]
    .filter((offset) => offset >= 0)
    .sort((left, right) => left - right)[0];
  const block = workflow.slice(blockStart, blockEnd);
  return block
    .split("\n")
    .filter((line) => line.length > 0)
    .map((line) => line.replace(/^ {10}/, ""))
    .join("\n");
}

function runDependencyFetchBlock(block, { failFirstFetch = false } = {}) {
  const directory = mkdtempSync(join(tmpdir(), "dakia-audit-fetch-test-"));
  const bin = join(directory, "bin");
  const log = join(directory, "cargo.log");
  mkdirSync(bin);
  const cargo = join(bin, "cargo");
  writeFileSync(
    cargo,
    `#!/bin/sh
printf '%s\\n' "$*" >> "$DAKIA_TEST_CARGO_LOG"
if [ "${failFirstFetch ? "true" : "false"}" = true ] && [ "$1" = fetch ] && [ "$2" = --locked ]; then
  exit 91
fi
`,
  );
  chmodSync(cargo, 0o700);
  try {
    const result = spawnSync("bash", ["-euc", block], {
      encoding: "utf8",
      env: {
        ...process.env,
        PATH: `${bin}:${process.env.PATH}`,
        DAKIA_TEST_CARGO_LOG: log,
      },
    });
    return {
      result,
      calls: readFileSync(log, "utf8").trimEnd().split("\n"),
    };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test("dependency-audit workflow fetches both locked graphs before offline audit and stops on failure", () => {
  const workflow = readFileSync(
    join(root, ".github/workflows/pull-request-validation.yml"),
    "utf8",
  );
  const fetchBlock = workflowRunBlock(workflow, "dependency_audit_fetch");
  const fetchStep = workflow.indexOf("      - id: dependency_audit_fetch\n");
  const auditStep = workflow.indexOf("      - id: dependency_audit\n");
  assert.ok(
    fetchStep < auditStep,
    "locked fetches must precede the offline audit step",
  );
  assert.match(
    workflow.slice(fetchStep, auditStep),
    /if: needs\.classify\.outputs\.dependency_audit == 'true'/,
    "the warm-cache preparation must remain dependency-audit scoped",
  );
  assert.match(
    workflow,
    /hashFiles\('Cargo\.lock', 'fuzz\/Cargo\.lock'\)/,
    "the registry cache must invalidate when either locked graph changes",
  );
  assert.deepEqual(fetchBlock.split("\n"), [
    "cargo fetch --locked",
    "cargo fetch --manifest-path fuzz/Cargo.toml --locked",
  ]);

  const success = runDependencyFetchBlock(fetchBlock);
  assert.equal(success.result.status, 0, success.result.stderr);
  assert.deepEqual(success.calls, [
    "fetch --locked",
    "fetch --manifest-path fuzz/Cargo.toml --locked",
  ]);

  const failure = runDependencyFetchBlock(fetchBlock, { failFirstFetch: true });
  assert.equal(failure.result.status, 91);
  assert.deepEqual(
    failure.calls,
    ["fetch --locked"],
    "bash -e must stop before the fuzz graph or audit can be treated as prepared",
  );
});
