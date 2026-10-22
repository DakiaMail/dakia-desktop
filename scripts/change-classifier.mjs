import { execFileSync } from "node:child_process";
import { appendFileSync } from "node:fs";

const ACTIVE_SCOPES = [
  "frontend",
  "rust-core",
  "mail-boundary",
  "tauri-boundary",
  "release-only",
  "dependency-audit",
];

const DOCS_OR_METADATA = [
  /^docs\//,
  /^README\.md$/,
  /^CONTRIBUTING\.md$/,
  /^SECURITY\.md$/,
  /^CODE_OF_CONDUCT\.md$/,
  /^\.github\/(?:CODEOWNERS|dependabot\.yml|pull_request_template\.md|ISSUE_TEMPLATE\/)/,
];

const MAIL_SAFETY_CATALOGUE = new Set([
  "testdata/mail-safety-catalogue.json",
  "docs/mail-safety-test-catalogue.md",
  "docs/mail-safety-evidence.md",
]);

// The standards matrix is a source-level assurance contract. Its validator
// sensitivity tests also run through the release-script glob, so changes must
// select both the mail boundary and that existing contract-test lane.
const MAIL_STANDARDS_CONTRACT = new Set([
  "testdata/mail-standards-requirements.json",
  "scripts/validate-mail-standards-requirements.mjs",
  "scripts/validate-mail-standards-requirements.node-test.mjs",
]);

const MAIL_STANDARDS_PROFILE = "docs/mail-safety-standards-profile.md";

const MAIL_CORE =
  /^crates\/dakia-core\/(?:src\/(?:mail|storage|provider|mime_budget|classification|flowed|oauth|ai)\.rs|testdata\/|tests\/)/;
const MAIL_FRONTEND =
  /^apps\/desktop\/src\/(?:api(?:MessageContent)?|types|App|components\/(?:HtmlMessage|Reader))(?:\.test)?\.(?:ts|tsx)$/;

// These runners join core/Tauri/frontend stages with nonce-bound artifacts.
// Their inputs must select every existing validation scope their runner uses,
// otherwise a changed fixture runner could skip the coupled Tauri boundary.
const TAURI_ARTIFACT_RUNNER_SCOPES = new Map([
  [
    "scripts/test-mime-reader-artifact.mjs",
    ["rust-core", "frontend", "mail-boundary", "tauri-boundary"],
  ],
  [
    "scripts/test-attachment-command-artifact.mjs",
    ["rust-core", "frontend", "mail-boundary", "tauri-boundary"],
  ],
  [
    "scripts/test-mail-command-tls.mjs",
    ["rust-core", "mail-boundary", "tauri-boundary"],
  ],
  [
    "scripts/test-unsubscribe-command-tls.mjs",
    ["rust-core", "mail-boundary", "tauri-boundary"],
  ],
  [
    "scripts/test-unsubscribe-command-tls.node-test.mjs",
    ["rust-core", "mail-boundary", "tauri-boundary", "release-only"],
  ],
  [
    "scripts/unsubscribe-command-tls-fixture-server.mjs",
    ["rust-core", "mail-boundary", "tauri-boundary"],
  ],
  [
    "scripts/unsubscribe-command-tls-fixture-server.node-test.mjs",
    ["rust-core", "mail-boundary", "tauri-boundary", "release-only"],
  ],
  [
    "scripts/fixture-process-lifecycle.mjs",
    [
      "rust-core",
      "frontend",
      "mail-boundary",
      "tauri-boundary",
      "release-only",
    ],
  ],
  [
    "scripts/attachment-command-fixture-server.mjs",
    ["rust-core", "frontend", "mail-boundary", "tauri-boundary"],
  ],
  [
    "scripts/mail-command-tls-fixture-server.mjs",
    ["rust-core", "mail-boundary", "tauri-boundary"],
  ],
]);

const TAURI_ARTIFACT_COMPONENTS = new Set([
  "apps/desktop/src/components/mimeReaderArtifact.test.tsx",
  "apps/desktop/src/components/mimeCorpusHandoff.test.tsx",
  "apps/desktop/src/components/attachmentCommandArtifact.test.tsx",
  "crates/dakia-core/src/mail/tests/mime_tauri_artifact_export_tests.rs",
  "crates/dakia-core/src/native_mail_safety_unsubscribe_fixture.rs",
  "apps/desktop/src-tauri/src/unsubscribe_command_tls_tests.rs",
  "crates/dakia-core/src/lib.rs",
]);

// These Tauri artifacts carry mail protocol data through the command boundary.
// The generic Tauri path branch supplies rust-core and tauri-boundary; it does
// not otherwise know that these two files also require mail-boundary checks.
const MAIL_TAURI_ARTIFACT_COMPONENTS = new Set([
  "crates/dakia-core/src/native_mail_safety_unsubscribe_fixture.rs",
  "apps/desktop/src-tauri/src/unsubscribe_command_tls_tests.rs",
  "crates/dakia-core/src/lib.rs",
]);

function matches(path, patterns) {
  return patterns.some((pattern) => pattern.test(path));
}

function allActiveScopes() {
  return Object.fromEntries(ACTIVE_SCOPES.map((scope) => [scope, true]));
}

/**
 * Classify repository-relative paths into validation scopes. This deliberately
 * promotes unknown or shared changes instead of risking an under-tested PR.
 */
export function classifyPaths(paths) {
  const normalizedPaths = [
    ...new Set(paths.map((path) => path.replaceAll("\\", "/"))),
  ].sort();
  const docsOnly = normalizedPaths.every((path) =>
    matches(path, DOCS_OR_METADATA),
  );
  const scopes = Object.fromEntries(
    ACTIVE_SCOPES.map((scope) => [scope, false]),
  );
  let requiresLfs = false;

  // The catalogue is documentation-shaped, but changing it changes a
  // source-level assurance declaration. Do not let it bypass its validator.
  if (
    docsOnly &&
    !normalizedPaths.some(
      (path) =>
        MAIL_SAFETY_CATALOGUE.has(path) || path === MAIL_STANDARDS_PROFILE,
    )
  ) {
    return {
      changedPaths: normalizedPaths,
      docsOnly: true,
      scopes,
      requiresLfs,
    };
  }

  for (const path of normalizedPaths) {
    const artifactScopes = TAURI_ARTIFACT_RUNNER_SCOPES.get(path);
    if (artifactScopes) {
      for (const scope of artifactScopes) scopes[scope] = true;
      continue;
    }

    if (TAURI_ARTIFACT_COMPONENTS.has(path)) {
      scopes["tauri-boundary"] = true;
      if (
        path.startsWith("crates/dakia-core/") ||
        MAIL_TAURI_ARTIFACT_COMPONENTS.has(path)
      ) {
        scopes["mail-boundary"] = true;
      }
      // The generic path branch below selects the source language boundary.
    }

    if (
      path === "Cargo.toml" ||
      path === "Cargo.lock" ||
      path === "package.json" ||
      path === "package-lock.json" ||
      path === "vitest.config.ts" ||
      path === "apps/desktop/tsconfig.json" ||
      path === "apps/desktop/vite.config.ts" ||
      path.startsWith(".github/workflows/") ||
      path === "scripts/change-classifier.mjs" ||
      path === "scripts/change-classifier.node-test.mjs"
    ) {
      Object.assign(scopes, allActiveScopes());
      continue;
    }

    // A crates.io path patch has no registry source checksum in Cargo.lock.
    // Its provenance manifest and verifier are therefore part of the
    // dependency-security boundary and must not be able to skip that gate.
    if (
      path === ".cargo/audit.toml" ||
      path === "scripts/audit-dependencies.sh" ||
      path.startsWith("scripts/verify-vendored-crates.") ||
      path.startsWith("testdata/vendored-crates/") ||
      path.startsWith("vendor/")
    ) {
      Object.assign(scopes, allActiveScopes());
      continue;
    }

    if (
      MAIL_SAFETY_CATALOGUE.has(path) ||
      path.startsWith("scripts/mail-safety-catalogue.") ||
      path.startsWith("scripts/validate-mail-safety-catalogue.") ||
      path.startsWith("scripts/report-mail-safety-catalogue.")
    ) {
      scopes["mail-boundary"] = true;
      continue;
    }

    if (MAIL_STANDARDS_CONTRACT.has(path)) {
      scopes["mail-boundary"] = true;
      scopes["release-only"] = true;
      continue;
    }

    if (path === MAIL_STANDARDS_PROFILE) {
      scopes["mail-boundary"] = true;
      continue;
    }

    if (path.startsWith("apps/desktop/src/")) {
      scopes.frontend = true;
      if (
        path === "apps/desktop/src/api.ts" ||
        path === "apps/desktop/src/nativeWindows.ts" ||
        path === "apps/desktop/src/composeWindow.ts" ||
        path === "apps/desktop/src/types.ts"
      ) {
        scopes["tauri-boundary"] = true;
      }
      if (
        MAIL_FRONTEND.test(path) ||
        path.startsWith("apps/desktop/src/test/fixtures/")
      ) {
        scopes["mail-boundary"] = true;
      }
      continue;
    }

    if (path.startsWith("crates/dakia-core/")) {
      scopes["rust-core"] = true;
      if (MAIL_CORE.test(path)) {
        scopes["mail-boundary"] = true;
      }
      continue;
    }

    if (path.startsWith("crates/dakia-cli/")) {
      scopes["rust-core"] = true;
      continue;
    }

    if (path.startsWith("apps/desktop/testdata/tauri-contract")) {
      // These fixtures are consumed on both sides of the invoke/event boundary:
      // TypeScript asserts frontend decoding while Rust asserts serialized
      // command and event payloads.
      scopes.frontend = true;
      scopes["tauri-boundary"] = true;
      continue;
    }

    if (path.startsWith("apps/desktop/src-tauri/")) {
      scopes["rust-core"] = true;
      scopes["tauri-boundary"] = true;
      if (
        path.startsWith("apps/desktop/src-tauri/resources/email-classifier-v2/")
      ) {
        scopes["mail-boundary"] = true;
        scopes["release-only"] = true;
      }
      if (/\/tauri(?:\.install)?\.conf\.json$/.test(path)) {
        scopes["release-only"] = true;
      }
      continue;
    }

    if (path.startsWith("apps/desktop/")) {
      scopes.frontend = true;
      continue;
    }

    if (path.startsWith("scripts/validate-realistic-fixtures.")) {
      scopes["mail-boundary"] = true;
      continue;
    }

    if (path.startsWith("scripts/tauri-contract-inventory.")) {
      scopes["tauri-boundary"] = true;
      continue;
    }

    if (path === "scripts/test-realtime-delivery.mjs") {
      scopes["tauri-boundary"] = true;
      continue;
    }

    if (path.startsWith("scripts/") || path.startsWith("docs/releases/")) {
      scopes["release-only"] = true;
      continue;
    }

    // A new top-level area has no established cheap validation contract yet.
    // Exercise every automatic scope until one is deliberately added above.
    Object.assign(scopes, allActiveScopes());
  }

  // Ordinary PR validation must never prepare the release-only ONNX model.
  // Model-runtime tests remain in the authoritative local gate.
  requiresLfs = false;

  return {
    changedPaths: normalizedPaths,
    docsOnly: false,
    scopes,
    requiresLfs,
  };
}

function git(cwd, args) {
  return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
}

/**
 * Resolve changed files against the true merge base rather than the current
 * base branch tip, which can include unrelated changes made after branching.
 */
export function changedPathsFromMergeBase({
  base,
  head = "HEAD",
  cwd = process.cwd(),
}) {
  if (!base) {
    throw new Error("A base revision is required (pass --base <revision>)");
  }
  const mergeBase = git(cwd, ["merge-base", base, head]);
  const output = execFileSync(
    "git",
    ["diff", "--name-only", "-z", mergeBase, head],
    {
      cwd,
      encoding: "utf8",
    },
  );
  const changedPaths = output.split("\0").filter(Boolean);
  return { mergeBase, changedPaths };
}

function parseArguments(argv) {
  const options = { head: "HEAD", githubOutput: false, json: false };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--base" || argument === "--head") {
      const value = argv[index + 1];
      if (!value) throw new Error(`${argument} requires a revision`);
      options[argument.slice(2)] = value;
      index += 1;
    } else if (argument === "--github-output") {
      options.githubOutput = true;
    } else if (argument === "--json") {
      options.json = true;
    } else {
      throw new Error(`Unknown argument: ${argument}`);
    }
  }
  return options;
}

function writeGithubOutput(result) {
  const outputPath = process.env.GITHUB_OUTPUT;
  if (!outputPath) throw new Error("--github-output requires GITHUB_OUTPUT");
  const entries = [
    ["docs_only", result.docsOnly],
    ["requires_lfs", result.requiresLfs],
    ...ACTIVE_SCOPES.map((scope) => [
      scope.replaceAll("-", "_"),
      result.scopes[scope],
    ]),
  ];
  appendFileSync(
    outputPath,
    `${entries.map(([key, value]) => `${key}=${value}`).join("\n")}\n`,
  );
}

function main() {
  const options = parseArguments(process.argv.slice(2));
  const { mergeBase, changedPaths } = changedPathsFromMergeBase(options);
  const result = {
    ...classifyPaths(changedPaths),
    base: options.base,
    head: options.head,
    mergeBase,
  };
  if (options.githubOutput) writeGithubOutput(result);
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (import.meta.url === new URL(process.argv[1], "file:").href) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`change classifier failed: ${error.message}\n`);
    process.exitCode = 1;
  }
}
