#!/usr/bin/env node

import { createHash } from "node:crypto";
import { existsSync, lstatSync, readFileSync, readdirSync } from "node:fs";
import { dirname, extname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = "testdata/realistic-fixtures.manifest.json";
const fixtureRoots = [
  { path: "crates/dakia-core/testdata/mime", extension: ".eml" },
  { path: "crates/dakia-core/tests/fixtures", extension: ".eml" },
  { path: "apps/desktop/src/test/fixtures", extension: ".html" },
];
const expectedSemantics = [
  "body",
  "html",
  "snippet",
  "attachments",
  "error",
  "resourceLimit",
];
const applicablePaths = new Set([
  "complete-rfc822",
  "catalogue",
  "header",
  "partial-preview",
  "storage-round-trip",
  "selective-bodystructure",
  "inline-image-resolution",
  "reader-render",
  "html-document",
  "reader-disclosure",
  "tauri-message-content",
  "typescript-decoding",
]);
const exerciseDomainPaths = new Map([
  ["rust.mail.parse-paths", ["complete-rfc822", "catalogue", "header"]],
  [
    "rust.mail.selective-bodystructure",
    ["selective-bodystructure", "storage-round-trip"],
  ],
  ["rust.mail.inline-images", ["complete-rfc822", "inline-image-resolution"]],
  ["rust.mail.partial-preview", ["partial-preview"]],
  ["rust.tauri.message-content", ["tauri-message-content"]],
  ["typescript.tauri-decoding", ["typescript-decoding"]],
  ["frontend.html-message", ["html-document", "reader-disclosure"]],
  ["frontend.reader", ["reader-render", "reader-disclosure"]],
]);
const prohibitedDomains = [
  "gmail.com",
  "googlemail.com",
  "outlook.com",
  "hotmail.com",
  "live.com",
  "yahoo.com",
  "icloud.com",
  "me.com",
  "mac.com",
  "linkedin.com",
  "swedbank.ee",
  "freshdesk.com",
];
const credentialPatterns = [
  /-----BEGIN(?: [A-Z]+)? PRIVATE KEY-----/i,
  /\b(?:api[-_ ]?key|access[-_ ]?token|client[-_ ]?secret|password)\b\s*[:=]\s*[^\s<]{8,}/i,
  /\bauthorization\s*:\s*bearer\s+[A-Za-z0-9._~-]{12,}/i,
  /\bAKIA[0-9A-Z]{16}\b/,
];

function addError(errors, message) {
  errors.push(message);
}

function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isSafeRelativePath(value) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    !value.startsWith("/") &&
    !value.includes("\\") &&
    value
      .split("/")
      .every((part) => part !== "" && part !== "." && part !== "..")
  );
}

function domainIsWithin(domain, parent) {
  return domain === parent || domain.endsWith(`.${parent}`);
}

function isValidDomain(value) {
  return (
    typeof value === "string" &&
    /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+$/i.test(
      value,
    )
  );
}

function listedFixtureFiles(root) {
  const files = [];
  for (const fixtureRoot of fixtureRoots) {
    const directory = resolve(root, fixtureRoot.path);
    if (!existsSync(directory)) {
      continue;
    }
    const visit = (current) => {
      for (const entry of readdirSync(current, { withFileTypes: true })) {
        const path = join(current, entry.name);
        if (entry.isDirectory()) {
          visit(path);
        } else if (
          entry.isFile() &&
          extname(entry.name) === fixtureRoot.extension
        ) {
          files.push(relative(root, path).replaceAll("\\", "/"));
        }
      }
    };
    visit(directory);
  }
  return files.sort();
}

function fixtureRootForPath(path) {
  return fixtureRoots.find(
    (fixtureRoot) =>
      path.startsWith(`${fixtureRoot.path}/`) &&
      extname(path) === fixtureRoot.extension,
  );
}

function extractFixtureDomains(contents) {
  const domains = new Set();
  for (const match of contents.matchAll(
    /[A-Za-z0-9._%+-]+@([A-Za-z0-9.-]+\.[A-Za-z]{2,})/g,
  )) {
    domains.add(match[1].toLowerCase());
  }
  // Scan every absolute endpoint, including RFC headers such as
  // List-Unsubscribe and plain-text links, not only HTML attributes.
  for (const match of contents.matchAll(
    /(?:https?:)?\/\/([A-Za-z0-9.-]+\.[A-Za-z]{2,})(?::\d+)?/gi,
  )) {
    domains.add(match[1].replace(/:\d+$/, "").toLowerCase());
  }
  return domains;
}

function decodedInspectionSurfaces(contents) {
  const surfaces = [contents];
  const transferEncoding =
    /^content-transfer-encoding:\s*(base64|quoted-printable)[^\r\n]*$/gim;
  for (const match of contents.matchAll(transferEncoding)) {
    const bodyStartMatch = /\r?\n\r?\n/g;
    bodyStartMatch.lastIndex = match.index + match[0].length;
    const separator = bodyStartMatch.exec(contents);
    if (!separator) continue;
    const bodyStart = separator.index + separator[0].length;
    const nextBoundary = contents.slice(bodyStart).search(/\r?\n--[^\r\n]+/);
    const body = contents.slice(
      bodyStart,
      nextBoundary < 0 ? contents.length : bodyStart + nextBoundary,
    );
    if (Buffer.byteLength(body) > 2 * 1024 * 1024) continue;
    if (match[1].toLowerCase() === "base64") {
      const compact = body.replace(/\s/g, "");
      if (compact.length >= 8 && /^[A-Za-z0-9+/]*={0,2}$/.test(compact)) {
        surfaces.push(Buffer.from(compact, "base64").toString("utf8"));
      }
    } else {
      surfaces.push(
        body
          .replace(/=\r?\n/g, "")
          .replace(/=([A-Fa-f0-9]{2})/g, (_, hex) =>
            String.fromCharCode(Number.parseInt(hex, 16)),
          ),
      );
    }
  }
  return surfaces;
}

function declaredTestBody(source, exerciseId) {
  const escaped = exerciseId.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const declarations = [
    new RegExp(
      `#\\[(?:tokio::)?test\\]\\s*(?:async\\s+)?fn\\s+${escaped}\\s*\\(`,
    ),
    new RegExp(`(?:it|test)\\s*\\(\\s*["'\`]${escaped}["'\`]`),
  ];
  const start = declarations
    .map((pattern) => source.search(pattern))
    .find((index) => index >= 0);
  if (start === undefined) return null;
  const remainder = source.slice(start + 1);
  const nextOffsets = [
    remainder.search(/\n\s*#\[(?:tokio::)?test\]/),
    remainder.search(/\n\s*(?:it|test)\s*\(/),
  ].filter((index) => index >= 0);
  const end = nextOffsets.length
    ? start + 1 + Math.min(...nextOffsets)
    : source.length;
  return source.slice(start, end);
}

function withoutComments(source) {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:])\/\/.*$/gm, "$1");
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// A receipt cannot be an unused binding or `void fixture`: either form can
// satisfy a name-based audit while never reaching the code under test.
function identifierHasNonTrivialUse(source, identifier) {
  const escaped = escapeRegExp(identifier);
  const withoutNoops = source
    .replace(new RegExp(`\\bvoid\\s+${escaped}\\s*;?`, "g"), "")
    .replace(
      new RegExp(
        `\\b(?:const|let|var)\\s+[A-Za-z_$][A-Za-z0-9_$]*\\s*=\\s*${escaped}\\s*;`,
        "g",
      ),
      "",
    );
  return new RegExp(`\\b${escaped}\\b`).test(withoutNoops);
}

function bindingBefore(source, index) {
  const start = Math.max(
    source.lastIndexOf(";", index),
    source.lastIndexOf("{", index),
    source.lastIndexOf("}", index),
  );
  return source
    .slice(start + 1, index)
    .match(
      /(?:^|\n)\s*(?:let(?:\s+mut)?|const|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\b[^=]*=\s*[\s\S]*$/,
    )?.[1];
}

function loaderResultIsConsumed(body, match) {
  const before = body.slice(0, match.index);
  const after = body.slice(match.index + match[0].length);
  if (/\bvoid\s*$/.test(before) || /^\s*;/.test(after)) return false;
  const binding = bindingBefore(body, match.index);
  if (!binding) return true;
  const escaped = escapeRegExp(binding);
  const meaningfulAfter = after.replace(
    new RegExp(`\\bvoid\\s+${escaped}\\s*;?`, "g"),
    "",
  );
  return new RegExp(`\\b${escaped}\\b`).test(meaningfulAfter);
}

function directLoaderExercisesFixture(body, fixtureName) {
  const escapedName = escapeRegExp(fixtureName);
  const loader = new RegExp(
    `(?:mime_corpus\\s*|include_(?:str|bytes)!\\s*)\\([^)]*["'\\x60][^"'\\x60]*${escapedName}(?:\\.(?:eml|html))?["'\\x60]`,
    "g",
  );
  return [...body.matchAll(loader)].some((match) =>
    loaderResultIsConsumed(body, match),
  );
}

function enumeratedLoaderExercisesFixture(body, fixtureName) {
  const casesContainFixture = body.includes(`"${fixtureName}"`);
  if (!casesContainFixture) return false;
  const loader = /mime_corpus\s*\(\s*name\s*\)/g;
  return [...body.matchAll(loader)].some((match) => {
    const before = body.slice(0, match.index);
    if (/\bvoid\s*$/.test(before)) return false;
    const binding = before.match(
      /\b(?:let|const)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*$/,
    )?.[1];
    return (
      binding === undefined ||
      identifierHasNonTrivialUse(
        body.slice(match.index + match[0].length),
        binding,
      )
    );
  });
}

function importedFixtureIdentifier(source, fixtureName) {
  const escapedName = escapeRegExp(fixtureName);
  const match = source.match(
    new RegExp(
      `\\bimport\\s+([A-Za-z_$][A-Za-z0-9_$]*)\\s+from\\s+["'\\x60][^"'\\x60]*${escapedName}\\.(?:eml|html)(?:\\?[^"'\\x60]*)?["'\\x60]`,
    ),
  );
  return match?.[1] ?? null;
}

function hasSharedContractReceipt(source, body, fixtureName) {
  const contractImport = source.match(
    /\bimport\s+([A-Za-z_$][A-Za-z0-9_$]*)\s+from\s+["'`][^"'`]*testdata\/tauri-contracts\/high-risk\.json["'`]/,
  )?.[1];
  const contractBinding = body.match(
    /\blet\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*fixture\s*\(\s*\)/,
  )?.[1];
  return (
    ((contractImport !== undefined &&
      identifierHasNonTrivialUse(body, contractImport)) ||
      (source.includes("tauri-contracts/high-risk.json") &&
        contractBinding !== undefined &&
        identifierHasNonTrivialUse(
          body.slice(body.indexOf(contractBinding) + contractBinding.length),
          contractBinding,
        ))) &&
    body.includes("realisticFixtureIds") &&
    body.includes("providerSignature") &&
    new RegExp(`["'\\x60]${escapeRegExp(fixtureName)}["'\\x60]`).test(body)
  );
}

function testBodyExercisesFixture(source, testBody, fixtureName) {
  const body = withoutComments(testBody);
  const importedIdentifier = importedFixtureIdentifier(source, fixtureName);
  // Kept local while the legacy recognition expressions below are removed in
  // the next change; their results are deliberately no longer accepted.
  const fixtureIdentifier = fixtureName;
  const escapedName = fixtureName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const escapedIdentifier = fixtureIdentifier.replace(
    /[.*+?^${}()|[\]\\]/g,
    "\\$&",
  );
  const directLoader = new RegExp(
    `(?:mime_corpus|include_(?:str|bytes)!)\\s*\\([^)]*["'\`][^"'\`]*${escapedName}(?:\\.(?:eml|html))?["'\`]`,
  );
  const enumeratedLoader = new RegExp(
    `["'\`]${escapedName}["'\`][\\s\\S]*mime_corpus\\s*\\(\\s*name\\s*\\)`,
  );
  const importedFixture = new RegExp(`\\b${escapedIdentifier}\\b`);
  const sharedContractReceipt =
    body.includes("realisticFixtureIds") &&
    body.includes("providerSignature") &&
    new RegExp(`["'\`]${escapedName}["'\`]`).test(body);
  return (
    directLoaderExercisesFixture(body, fixtureName) ||
    enumeratedLoaderExercisesFixture(body, fixtureName) ||
    (importedIdentifier !== null &&
      identifierHasNonTrivialUse(body, importedIdentifier)) ||
    hasSharedContractReceipt(source, body, fixtureName)
  );
}

function assertString(errors, value, label) {
  if (typeof value !== "string" || value.trim() === "") {
    addError(errors, `${label} must be a non-empty string.`);
    return false;
  }
  return true;
}

function validateExercise(errors, root, fixture, exercise) {
  const label = `Fixture ${fixture.id} exercise`;
  if (!isPlainObject(exercise)) {
    addError(errors, `${label} must be an object.`);
    return;
  }
  if (!assertString(errors, exercise.domain, `${label}.domain`)) return;
  const coveredPaths = exerciseDomainPaths.get(exercise.domain);
  if (!coveredPaths) {
    addError(errors, `${label}.domain is not a governed publication path.`);
    return;
  }
  if (!assertString(errors, exercise.source, `${label}.source`)) return;
  if (!assertString(errors, exercise.id, `${label}.id`)) return;
  if (!isSafeRelativePath(exercise.source)) {
    addError(
      errors,
      `${label}.source must be a safe repository-relative path.`,
    );
    return;
  }
  const sourcePath = resolve(root, exercise.source);
  if (!sourcePath.startsWith(`${root}/`) || !existsSync(sourcePath)) {
    addError(errors, `${label} source is missing: ${exercise.source}`);
    return;
  }
  const source = readFileSync(sourcePath, "utf8");
  const fixtureName = fixture.path
    .slice(0, -extname(fixture.path).length)
    .split("/")
    .at(-1);
  const testBody = declaredTestBody(source, exercise.id);
  if (
    !testBody ||
    !testBodyExercisesFixture(source, testBody, fixtureName)
  ) {
    addError(
      errors,
      `Fixture ${fixture.id} is not exercised by declared ${exercise.domain}/${exercise.id}.`,
    );
    return;
  }
  return coveredPaths;
}

export function validateFixtureCorpus(root = repositoryRoot) {
  const errors = [];
  const absoluteRoot = resolve(root);
  const absoluteManifest = resolve(absoluteRoot, manifestPath);
  let manifest;
  try {
    manifest = JSON.parse(readFileSync(absoluteManifest, "utf8"));
  } catch (error) {
    throw new Error(
      `Could not read realistic fixture manifest: ${error.message}`,
    );
  }
  if (!isPlainObject(manifest)) {
    throw new Error("Realistic fixture manifest must be a JSON object.");
  }
  if (manifest.schemaVersion !== 1) {
    addError(errors, "Realistic fixture manifest schemaVersion must be 1.");
  }
  if (JSON.stringify(manifest.fixtureRoots) !== JSON.stringify(fixtureRoots)) {
    addError(
      errors,
      "Realistic fixture manifest fixtureRoots must match the governed corpus roots.",
    );
  }
  if (!Array.isArray(manifest.fixtures)) {
    addError(errors, "Realistic fixture manifest fixtures must be an array.");
  } else {
    const ids = new Set();
    const paths = new Set();
    for (const fixture of manifest.fixtures) {
      if (!isPlainObject(fixture)) {
        addError(errors, "Fixture entries must be objects.");
        continue;
      }
      if (!assertString(errors, fixture.id, "Fixture id")) continue;
      if (ids.has(fixture.id))
        addError(errors, `Duplicate fixture id: ${fixture.id}`);
      ids.add(fixture.id);
      if (!assertString(errors, fixture.path, `Fixture ${fixture.id} path`))
        continue;
      if (paths.has(fixture.path))
        addError(errors, `Duplicate fixture path: ${fixture.path}`);
      paths.add(fixture.path);
      if (
        !isSafeRelativePath(fixture.path) ||
        !fixtureRootForPath(fixture.path)
      ) {
        addError(
          errors,
          `Fixture ${fixture.id} path is outside the governed corpus roots: ${fixture.path}`,
        );
        continue;
      }
      const absoluteFixture = resolve(absoluteRoot, fixture.path);
      if (
        !absoluteFixture.startsWith(`${absoluteRoot}/`) ||
        !existsSync(absoluteFixture) ||
        !lstatSync(absoluteFixture).isFile()
      ) {
        addError(
          errors,
          `Fixture ${fixture.id} file is missing: ${fixture.path}`,
        );
        continue;
      }
      if (
        typeof fixture.sha256 !== "string" ||
        !/^[a-f0-9]{64}$/.test(fixture.sha256)
      ) {
        addError(
          errors,
          `Fixture ${fixture.id} sha256 must be lowercase SHA-256 hex.`,
        );
      } else {
        const actual = createHash("sha256")
          .update(readFileSync(absoluteFixture))
          .digest("hex");
        if (actual !== fixture.sha256)
          addError(
            errors,
            `Fixture ${fixture.id} checksum drift: expected ${fixture.sha256}, got ${actual}.`,
          );
      }
      if (
        !isPlainObject(fixture.provenance) ||
        !["synthetic", "faithfully-redacted"].includes(
          fixture.provenance.kind,
        ) ||
        !assertString(
          errors,
          fixture.provenance.detail,
          `Fixture ${fixture.id} provenance.detail`,
        )
      ) {
        addError(
          errors,
          `Fixture ${fixture.id} requires synthetic or faithfully-redacted provenance.`,
        );
      }
      assertString(errors, fixture.issue, `Fixture ${fixture.id} issue`);
      if (
        !isPlainObject(fixture.provider) ||
        !assertString(
          errors,
          fixture.provider.shape,
          `Fixture ${fixture.id} provider.shape`,
        ) ||
        fixture.provider.liveProviderCompatible !== false
      ) {
        addError(
          errors,
          `Fixture ${fixture.id} provider must explicitly set liveProviderCompatible to false.`,
        );
      }
      if (!isPlainObject(fixture.expected)) {
        addError(errors, `Fixture ${fixture.id} requires expected semantics.`);
      } else {
        for (const field of expectedSemantics)
          assertString(
            errors,
            fixture.expected[field],
            `Fixture ${fixture.id} expected.${field}`,
          );
      }
      if (
        !Array.isArray(fixture.applicablePaths) ||
        fixture.applicablePaths.length === 0 ||
        fixture.applicablePaths.some((path) => !applicablePaths.has(path))
      ) {
        addError(errors, `Fixture ${fixture.id} has invalid applicablePaths.`);
      }
      const redaction = fixture.redaction;
      if (
        !isPlainObject(redaction) ||
        !assertString(
          errors,
          redaction.reviewer,
          `Fixture ${fixture.id} redaction.reviewer`,
        ) ||
        !/^\d{4}-\d{2}-\d{2}$/.test(redaction.reviewedOn ?? "") ||
        !Array.isArray(redaction.permittedDomains) ||
        redaction.permittedDomains.length === 0
      ) {
        addError(
          errors,
          `Fixture ${fixture.id} requires reviewer, ISO review date, and permitted domains.`,
        );
      } else {
        for (const domain of redaction.permittedDomains) {
          if (
            !isValidDomain(domain) ||
            prohibitedDomains.some((prohibited) =>
              domainIsWithin(domain, prohibited),
            )
          ) {
            addError(
              errors,
              `Fixture ${fixture.id} has prohibited permitted domain: ${domain}`,
            );
          }
        }
      }
      if (
        !Array.isArray(fixture.exercisedBy) ||
        fixture.exercisedBy.length === 0
      ) {
        addError(
          errors,
          `Fixture ${fixture.id} must declare at least one exercising test.`,
        );
      } else {
        const exercisedPaths = new Set();
        for (const exercise of fixture.exercisedBy) {
          for (const path of validateExercise(
            errors,
            absoluteRoot,
            fixture,
            exercise,
          ) ?? []) {
            exercisedPaths.add(path);
          }
        }
        for (const path of fixture.applicablePaths ?? []) {
          if (!exercisedPaths.has(path)) {
            addError(
              errors,
              `Fixture ${fixture.id} applicable path is not exercised: ${path}.`,
            );
          }
        }
      }
      const contents = readFileSync(absoluteFixture, "utf8");
      const inspectionSurfaces = decodedInspectionSurfaces(contents);
      const permittedDomains = redaction?.permittedDomains ?? [];
      for (const surface of inspectionSurfaces) {
        for (const domain of extractFixtureDomains(surface)) {
          if (
            prohibitedDomains.some((prohibited) =>
              domainIsWithin(domain, prohibited),
            )
          ) {
            addError(
              errors,
              `Fixture ${fixture.id} contains prohibited live domain: ${domain}`,
            );
          } else if (
            !permittedDomains.some(
              (permitted) =>
                isValidDomain(permitted) && domainIsWithin(domain, permitted),
            )
          ) {
            addError(
              errors,
              `Fixture ${fixture.id} contains undeclared address or endpoint domain: ${domain}`,
            );
          }
        }
        for (const pattern of credentialPatterns) {
          if (pattern.test(surface))
            addError(
              errors,
              `Fixture ${fixture.id} contains credential-like material matching ${pattern}.`,
            );
        }
      }
    }
    const manifested = [...paths].sort();
    const actual = listedFixtureFiles(absoluteRoot);
    for (const path of actual)
      if (!paths.has(path))
        addError(errors, `Unmanifested fixture file: ${path}`);
    for (const path of manifested)
      if (!actual.includes(path))
        addError(errors, `Manifested fixture file is not enumerated: ${path}`);
  }
  if (errors.length > 0)
    throw new Error(
      `Realistic fixture manifest validation failed:\n- ${errors.join("\n- ")}`,
    );
  return {
    fixtureCount: manifest.fixtures.length,
    files: listedFixtureFiles(absoluteRoot),
  };
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const result = validateFixtureCorpus();
    process.stdout.write(
      `Validated ${result.fixtureCount} realistic fixtures.\n`,
    );
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : error}\n`);
    process.exitCode = 1;
  }
}
