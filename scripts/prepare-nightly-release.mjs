#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(new URL("..", import.meta.url).pathname);

export function nextPatch(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!match) throw new Error(`Expected a stable semantic version, got '${version}'.`);
  return `${match[1]}.${match[2]}.${Number(match[3]) + 1}`;
}

export function userFacingSubject(subject) {
  if (/^(merge|chore|build|ci|docs|prepare v?|bump )/i.test(subject.trim())) return null;
  const cleaned = subject
    .replace(/\s*\(#\d+\)\s*$/, "")
    .replace(/^(feat|fix|perf|refactor|docs|chore)(\([^)]*\))?!?:\s*/i, "")
    .trim();
  if (!cleaned || /^(merge|prepare v?\d|bump )/i.test(cleaned)) return null;
  const knownChanges = [
    [/^interactive email address header actions$/i, "Work with email addresses directly from message headers."],
    [/^add (?:a )?feedback button to (?:the )?mailbox sidebar$/i, "Send feedback directly from the mailbox sidebar."],
  ];
  const known = knownChanges.find(([pattern]) => pattern.test(cleaned));
  if (known) return known[1];
  return cleaned.endsWith(".") ? cleaned : `${cleaned}.`;
}

export function releaseNotes({ version, subjects }) {
  const changes = [...new Set(subjects.map(userFacingSubject).filter(Boolean))];
  if (!changes.length) throw new Error("No reader-facing changes were found for this release.");
  return `# Dakia v${version}\n\nThis update includes the latest improvements to Dakia.\n\n## What changed\n\n${changes
    .map((subject) => `- ${subject}`)
    .join("\n")}\n\n## Download\n\nDakia v${version} supports Apple Silicon Macs.\n`;
}

function git(...args) {
  return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
}

function replaceExactly(path, pattern, replacement) {
  const source = readFileSync(path, "utf8");
  const result = source.replace(pattern, replacement);
  if (result === source) throw new Error(`Could not update version in ${path}.`);
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

  replaceExactly(resolve(root, "Cargo.toml"), /(?<=\[workspace\.package\][\s\S]*?version = ")[^"]+/, version);
  replaceExactly(resolve(root, "apps/desktop/src-tauri/tauri.conf.json"), /(?<="version": ")[^"]+/, version);
  replaceExactly(
    resolve(root, "Cargo.lock"),
    /(?<=name = "dakia-(?:cli|core|desktop)"\nversion = ")[^"]+/g,
    version,
  );
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  const [base] = process.argv.slice(2);
  if (!base) throw new Error("Usage: prepare-nightly-release.mjs <last-release-commit>");
  const currentVersion = JSON.parse(readFileSync(resolve(root, "package.json"), "utf8")).version;
  const version = nextPatch(currentVersion);
  const subjects = git("log", "--format=%s", `${base}..HEAD`).split("\n");
  const notes = releaseNotes({ version, subjects });
  updateVersion(version);
  writeFileSync(resolve(root, "docs/releases", `v${version}.md`), notes);
  process.stdout.write(`${JSON.stringify({ tag: `v${version}`, notes: `docs/releases/v${version}.md` })}\n`);
}
