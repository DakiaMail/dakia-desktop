#!/usr/bin/env node

import { createHash } from "node:crypto";
import { appendFileSync, readFileSync, statSync } from "node:fs";
import { spawnSync } from "node:child_process";

function fail(message) {
  throw new Error(message);
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function objectPaths(args) {
  const objects = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] !== "-object") continue;
    const path = args[index + 1];
    if (!path || path.startsWith("-"))
      fail("llvm-cov export has an invalid -object argument");
    objects.push(path);
    index += 1;
  }
  if (objects.length === 0 || new Set(objects).size !== objects.length) {
    fail("llvm-cov export must contain a nonempty, unique object cohort");
  }
  return objects;
}

function state(path) {
  return { path, bytes: statSync(path).size, sha256: sha256(path) };
}

function exportInput(args) {
  if (args[0] !== "export") return undefined;
  const profile = args.find((value) => value.startsWith("-instr-profile="));
  if (!profile) fail("llvm-cov export has no instrumented profile");
  const profilePath = profile.slice("-instr-profile=".length);
  if (!profilePath)
    fail("llvm-cov export has an empty instrumented profile path");
  const objects = objectPaths(args);
  for (let index = 1; index < args.length; index += 1) {
    if (args[index].startsWith("-") || args[index - 1] === "-object") {
      continue;
    }
    try {
      if (statSync(args[index]).isFile()) {
        fail(
          "llvm-cov export passed an object outside a -object argument pair",
        );
      }
    } catch (error) {
      if (
        error instanceof Error &&
        error.message.includes("outside a -object argument pair")
      ) {
        throw error;
      }
      // Non-path option values, such as an ignore regex, are not objects.
    }
  }
  return {
    profilePath,
    profileSha256: sha256(profilePath),
    objectState: objects.map(state),
  };
}

function main() {
  const program = process.env.RUST_COVERAGE_REAL_LLVM_COV;
  const log = process.env.RUST_COVERAGE_EXPORT_LOG;
  if (!program || !log) {
    fail(
      "RUST_COVERAGE_REAL_LLVM_COV and RUST_COVERAGE_EXPORT_LOG are required",
    );
  }
  const args = process.argv.slice(2);
  const before = exportInput(args);
  const result = spawnSync(program, args, { stdio: "inherit" });
  if (result.error) throw result.error;
  const exitCode = result.status ?? 1;
  if (before) {
    const after = exportInput(args);
    appendFileSync(
      log,
      `${JSON.stringify({
        version: 1,
        kind: "rust-coverage-export-invocation",
        program,
        arguments: args,
        exitCode,
        signal: result.signal,
        profilePath: before.profilePath,
        profileSha256Before: before.profileSha256,
        profileSha256After: after.profileSha256,
        objectStateBefore: before.objectState,
        objectStateAfter: after.objectState,
      })}\n`,
      { mode: 0o600 },
    );
  }
  process.exitCode = exitCode;
}

try {
  main();
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
