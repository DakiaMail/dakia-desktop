import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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
      name: "Tauri inventory changes exercise the Tauri boundary",
      paths: ["scripts/tauri-contract-inventory.mjs"],
      expected: { activeScopes: ["tauri-boundary"] },
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
  assert.deepEqual(
    [...workflow.matchAll(/^  ([a-z][a-z0-9-]+):$/gm)].map((match) => match[1]),
    ["classify", "validate"],
  );
  assert.match(
    workflow,
    /validate:\n\s+name: Validate applicable scopes[\s\S]*?timeout-minutes: 20/,
    "the cold-cache activation run must have time to populate the shared Cargo cache",
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
  const tauriStep = workflow.match(
    /id: tauri_boundary[\s\S]*?(?=\n      - id:|\n      - name: Report selected scopes)/,
  )?.[0];
  assert.ok(tauriStep, "expected a Tauri boundary step");
  assert.doesNotMatch(tauriStep, /tauri-contract-inventory/);
  assert.doesNotMatch(
    workflow,
    /cargo test -p dakia-desktop tauri_contracts_tests/,
  );
});
