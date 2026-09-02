#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(new URL("..", import.meta.url).pathname);

export function nextPatch(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!match)
    throw new Error(`Expected a stable semantic version, got '${version}'.`);
  return `${match[1]}.${match[2]}.${Number(match[3]) + 1}`;
}

export function userFacingSubject(subject) {
  if (/^(merge|chore|build|ci|docs|prepare v?|bump )/i.test(subject.trim()))
    return null;
  const cleaned = subject
    .replace(/\s*\(#\d+\)\s*$/, "")
    .replace(/^(feat|fix|perf|refactor|docs|chore)(\([^)]*\))?!?:\s*/i, "")
    .trim();
  if (!cleaned || /^(merge|prepare v?\d|bump )/i.test(cleaned)) return null;
  // Internal or umbrella subjects carry no user-visible claim and must never
  // become release-note bullets on their own.
  if (
    /^(optimize ci|harden release|update (?:dakia )?desktop (?:application|implementation)|update desktop app|prepare v?\d|bump )/i.test(
      cleaned,
    )
  )
    return null;
  const knownChanges = [
    [
      /^interactive email address header actions$/i,
      "Work with email addresses directly from message headers.",
    ],
    [
      /^add (?:a )?feedback button to (?:the )?mailbox sidebar$/i,
      "Send feedback directly from the mailbox sidebar.",
    ],
    [
      /^add privacy-preserving usage analytics$/i,
      "Added privacy-preserving usage analytics.",
    ],
    [
      /^fix people categorization across desktop, CLI, and core$/i,
      "Improve people categorization across desktop, CLI, and core.",
    ],
    [
      /^add send again action for sent messages$/i,
      "Resend sent messages with the new Send again action.",
    ],
    [
      /^make archive and read mutations optimistic$/i,
      "Archive messages and mark them as read with faster optimistic updates.",
    ],
  ];
  const known = knownChanges.find(([pattern]) => pattern.test(cleaned));
  if (known) return known[1];
  return cleaned.endsWith(".") ? cleaned : `${cleaned}.`;
}

export function releaseNotes({ version, subjects }) {
  const changes = [...new Set(subjects.map(userFacingSubject).filter(Boolean))];
  if (!changes.length)
    throw new Error("No reader-facing changes were found for this release.");
  return `# Dakia v${version}\n\nThis update includes the latest improvements to Dakia.\n\n## What changed\n\n${changes
    .map((subject) => `- ${subject}`)
    .join(
      "\n",
    )}\n\n## Download\n\nDakia v${version} is available for macOS, Linux, and Windows.\n`;
}

function git(...args) {
  return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
}

function replaceExactly(path, pattern, replacement) {
  const source = readFileSync(path, "utf8");
  const result = source.replace(pattern, replacement);
  if (result === source)
    throw new Error(`Could not update version in ${path}.`);
  writeFileSync(path, result);
}

export function updateVersion(version) {
  const packagePath = resolve(root, "package.json");
  const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
  packageJson.version = version;
  writeFileSync(packagePath, `${JSON.stringify(packageJson, null, 2)}\n`);

  const lockPath = resolve(root, "package-lock.json");
  const lock = JSON.parse(readFileSync(lockPath, "utf8"));
  lock.version = version;
  lock.packages[""].version = version;
  writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`);

  replaceExactly(
    resolve(root, "Cargo.toml"),
    /(?<=\[workspace\.package\][\s\S]*?version = ")[^"]+/,
    version,
  );
  replaceExactly(
    resolve(root, "apps/desktop/src-tauri/tauri.conf.json"),
    /(?<="version": ")[^"]+/,
    version,
  );
  replaceExactly(
    resolve(root, "Cargo.lock"),
    /(?<=name = "dakia-(?:cli|core|desktop)"\nversion = ")[^"]+/g,
    version,
  );
}

export function parseCliArgs(args) {
  const dryRun = args.includes("--dry-run");
  const positional = args.filter((argument) => argument !== "--dry-run");
  if (
    positional.length !== 1 ||
    args.length !== positional.length + Number(dryRun)
  ) {
    throw new Error(
      "Usage: prepare-nightly-release.mjs <last-release-commit> [--dry-run]",
    );
  }
  return { base: positional[0], dryRun };
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  const { base, dryRun } = parseCliArgs(process.argv.slice(2));
  const currentVersion = JSON.parse(
    readFileSync(resolve(root, "package.json"), "utf8"),
  ).version;
  const version = nextPatch(currentVersion);
  const subjects = git("log", "--format=%s", `${base}..HEAD`).split("\n");
  const notes = releaseNotes({ version, subjects });
  if (dryRun) {
    process.stdout.write(`${JSON.stringify({ tag: `v${version}`, notes })}\n`);
  } else {
    updateVersion(version);
    writeFileSync(resolve(root, "docs/releases", `v${version}.md`), notes);
    process.stdout.write(
      `${JSON.stringify({ tag: `v${version}`, notes: `docs/releases/v${version}.md` })}\n`,
    );
  }
}
