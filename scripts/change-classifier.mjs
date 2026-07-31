import { execFileSync } from "node:child_process";
import { appendFileSync } from "node:fs";

const ACTIVE_SCOPES = [
  "frontend",
  "rust-core",
  "mail-boundary",
  "tauri-boundary",
  "release-only",
];

const DOCS_OR_METADATA = [
  /^docs\//,
  /^README\.md$/,
  /^CONTRIBUTING\.md$/,
  /^SECURITY\.md$/,
  /^CODE_OF_CONDUCT\.md$/,
  /^\.github\/(?:CODEOWNERS|dependabot\.yml|pull_request_template\.md|ISSUE_TEMPLATE\/)/,
];

const MAIL_CORE =
  /^crates\/dakia-core\/(?:src\/(?:mail|storage|provider|mime_budget|classification|flowed|oauth|ai)\.rs|testdata\/|tests\/)/;
const MAIL_FRONTEND =
  /^apps\/desktop\/src\/(?:api(?:MessageContent)?|types|App|components\/(?:HtmlMessage|Reader))(?:\.test)?\.(?:ts|tsx)$/;

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

  if (docsOnly) {
    return {
      changedPaths: normalizedPaths,
      docsOnly: true,
      scopes,
      requiresLfs,
    };
  }

  for (const path of normalizedPaths) {
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
