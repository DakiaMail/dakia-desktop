import { spawn } from "node:child_process";
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { pathToFileURL } from "node:url";

export function commandsFor(lane) {
  if (lane === "source")
    return [
      ["node", ["scripts/validate-mail-safety-catalogue.mjs"]],
      ["node", ["scripts/validate-mail-standards-requirements.mjs"]],
      ["node", ["scripts/validate-realistic-fixtures.mjs"]],
      ["node", ["scripts/tauri-contract-inventory.mjs"]],
      ["npm", ["run", "typecheck"]],
      ["npm", ["run", "format:check"]],
      ["cargo", ["fmt", "--all", "--", "--check"]],
      [
        "cargo",
        [
          "clippy",
          "--workspace",
          "--all-targets",
          "--locked",
          "--",
          "-D",
          "warnings",
        ],
      ],
      ["npm", ["test", "--", "--maxWorkers=1"]],
      ["cargo", ["test", "--workspace", "--locked"]],
      [
        "cargo",
        [
          "test",
          "--locked",
          "-p",
          "dakia-desktop",
          "--features",
          "native-mail-safety-fixture",
          "--lib",
        ],
      ],
      ["node", ["scripts/test-realtime-delivery.mjs"]],
      ["node", ["scripts/test-mime-reader-artifact.mjs"]],
      ["node", ["scripts/test-attachment-command-artifact.mjs"]],
      ["node", ["scripts/test-mail-command-tls.mjs"]],
      ["node", ["scripts/test-unsubscribe-command-tls.mjs"]],
      ["npm", ["run", "test:release-scripts"]],
      ["npm", ["run", "build:web"]],
    ];
  if (lane === "deep")
    return [
      ["bash", ["scripts/mail-safety-servers.sh", "all"]],
      ["bash", ["scripts/run-mail-safety-fuzz.sh", "30"]],
    ];
  throw new Error("Use --lane source or --lane deep");
}

function execute(command, args) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { stdio: "inherit", shell: false });
    child.once("error", () =>
      resolve({ exitCode: null, signal: null, status: "unavailable" }),
    );
    child.once("exit", (exitCode, signal) =>
      resolve({
        exitCode,
        signal,
        status: exitCode === 0 ? "passed" : "failed",
      }),
    );
  });
}

export async function runMailSafety({
  lane = "source",
  repeat = 1,
  revision = "unavailable",
  workingTree = "unknown",
  sourceTree = null,
  run = execute,
  now = () => new Date().toISOString(),
} = {}) {
  if (!Number.isInteger(repeat) || repeat < 1 || repeat > 3)
    throw new Error("Repeat must be 1, 2, or 3");
  const plan = commandsFor(lane);
  const report = {
    schemaVersion: 1,
    lane,
    revision,
    workingTree,
    sourceTree,
    environment: {
      platform: process.platform,
      architecture: process.arch,
      node: process.version,
    },
    startedAt: now(),
    finishedAt: null,
    status: "running",
    runs: [],
    limitations:
      lane === "source"
        ? [
            "Ignored credentialed/independent-server tests are separate evidence.",
            "Native WebKit, packaged upgrades, live providers, audits, performance, and fuzzing are not established by this source run.",
          ]
        : [
            "This deep lane does not establish native, live-provider, packaged-release, or performance acceptance.",
          ],
  };
  for (let iteration = 1; iteration <= repeat; iteration += 1) {
    const result = { iteration, status: "passed", commands: [] };
    report.runs.push(result);
    for (const [command, args] of plan) {
      const entry = {
        command,
        args,
        startedAt: now(),
        finishedAt: null,
        status: "running",
      };
      result.commands.push(entry);
      let outcome;
      try {
        outcome = await run(command, args);
      } catch {
        outcome = { status: "unavailable", exitCode: null, signal: null };
      }
      Object.assign(entry, outcome, { finishedAt: now() });
      if (entry.status !== "passed" || entry.exitCode !== 0 || entry.signal) {
        entry.status =
          entry.status === "unavailable" ? "unavailable" : "failed";
        result.status = entry.status;
        report.status = entry.status;
        report.finishedAt = now();
        return report;
      }
    }
  }
  report.status = "passed";
  report.finishedAt = now();
  return report;
}

export const SOURCE_SNAPSHOT_PATHS = [
  "apps/desktop/src",
  "apps/desktop/src-tauri/src",
  "apps/desktop/src-tauri/testdata",
  "apps/desktop/src-tauri/icons",
  "apps/desktop/src-tauri/resources",
  "apps/desktop/src-tauri/capabilities",
  "apps/desktop/src-tauri/permissions",
  "apps/desktop/src-tauri/Cargo.toml",
  "apps/desktop/src-tauri/build.rs",
  "apps/desktop/src-tauri/tauri.conf.json",
  "apps/desktop/src-tauri/tauri.install.conf.json",
  "apps/desktop/index.html",
  "apps/desktop/fatal-recovery.html",
  "apps/desktop/vite.config.ts",
  "apps/desktop/tsconfig.json",
  "rust-toolchain.toml",
  ".cargo/config.toml",
  ".cargo/audit.toml",
  "vitest.config.ts",
  "apps/desktop/testdata",
  "crates",
  "scripts",
  "testdata",
  "fuzz",
  "vendor",
  "Cargo.toml",
  "Cargo.lock",
  "package.json",
  "package-lock.json",
  ".github/workflows/pull-request-validation.yml",
];

export function sourceSnapshot() {
  try {
    const paths = [
      ...new Set(
        execFileSync(
          "git",
          [
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ...SOURCE_SNAPSHOT_PATHS,
          ],
          { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 },
        )
          .split("\0")
          .filter(Boolean),
      ),
    ].sort();
    const hash = createHash("sha256");
    for (const path of paths) {
      hash.update(path).update("\0");
      try {
        hash.update(createHash("sha256").update(readFileSync(path)).digest());
      } catch (failure) {
        if (failure.code !== "ENOENT") throw failure;
        hash.update("deleted");
      }
      hash.update("\0");
    }
    return {
      algorithm: "sha256-path-and-content-manifest",
      digest: hash.digest("hex"),
      fileCount: paths.length,
    };
  } catch {
    return null;
  }
}

async function main() {
  const options = { lane: "source", repeat: 1, output: null };
  const args = process.argv.slice(2);
  for (let index = 0; index < args.length; index += 2) {
    const key = args[index];
    const value = args[index + 1];
    if (!["--lane", "--repeat", "--output"].includes(key) || !value)
      throw new Error(
        "Use --lane source|deep [--repeat 1|2|3] [--output PATH]",
      );
    options[key.slice(2)] = key === "--repeat" ? Number(value) : value;
  }
  let revision = "unavailable";
  let workingTree = "unknown";
  try {
    revision = execFileSync("git", ["rev-parse", "HEAD"], {
      encoding: "utf8",
    }).trim();
  } catch {
    /* Report missing provenance honestly. */
  }
  try {
    execFileSync(
      "git",
      [
        "diff",
        "--quiet",
        "HEAD",
        "--",
        "apps/desktop/src",
        "apps/desktop/src-tauri/src",
        "crates",
        "scripts",
        "testdata",
        "fuzz",
        "package.json",
        "package-lock.json",
      ],
      { stdio: "ignore" },
    );
    workingTree = "no-tracked-differences; untracked files not assessed";
  } catch {
    workingTree = "modified or unavailable";
  }
  const sourceTree = sourceSnapshot();
  const report = await runMailSafety({
    ...options,
    revision,
    workingTree,
    sourceTree,
  });
  report.sourceTreeAfter = sourceSnapshot();
  if (
    !sourceTree ||
    !report.sourceTreeAfter ||
    sourceTree.digest !== report.sourceTreeAfter.digest
  ) {
    report.status = "failed";
    report.limitations.push(
      "Source changed during execution; this is not a clean baseline.",
    );
  }
  const json = `${JSON.stringify(report, null, 2)}\n`;
  if (options.output) writeFileSync(options.output, json);
  else process.stdout.write(json);
  if (report.status !== "passed") process.exitCode = 1;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main().catch((error) => {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  });
}
