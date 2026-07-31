import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  cpSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { validateFixtureCorpus } from "./validate-realistic-fixtures.mjs";

const repositoryRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const copiedDirectories = [
  "crates/dakia-core/testdata/mime",
  "crates/dakia-core/tests/fixtures",
  "apps/desktop/src/test/fixtures",
];
const copiedSources = [
  "crates/dakia-core/src/mail.rs",
  "apps/desktop/src-tauri/src/tauri_contracts_tests.rs",
  "apps/desktop/src/components/HtmlMessage.test.tsx",
  "apps/desktop/src/components/Reader.test.tsx",
  "apps/desktop/src/tauriContracts.test.ts",
];

function copyCorpus() {
  const root = mkdtempSync("/tmp/dakia-realistic-fixtures-");
  for (const directory of copiedDirectories)
    cpSync(join(repositoryRoot, directory), join(root, directory), {
      recursive: true,
    });
  for (const source of copiedSources)
    cpSync(join(repositoryRoot, source), join(root, source));
  cpSync(
    join(repositoryRoot, "testdata/realistic-fixtures.manifest.json"),
    join(root, "testdata/realistic-fixtures.manifest.json"),
  );
  return root;
}

function withCorpus(run) {
  const root = copyCorpus();
  try {
    return run(root);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

function manifestAt(root) {
  const path = join(root, "testdata/realistic-fixtures.manifest.json");
  return [path, JSON.parse(readFileSync(path, "utf8"))];
}

test("validates the checked-in realistic mail fixture corpus", () => {
  const result = validateFixtureCorpus(repositoryRoot);
  assert.equal(result.fixtureCount, 17);
  assert.equal(result.files.filter((path) => path.endsWith(".eml")).length, 13);
  assert.equal(result.files.filter((path) => path.endsWith(".html")).length, 4);
});

test("rejects checksum drift and unmanifested fixtures", () =>
  withCorpus((root) => {
    const drifted = join(
      root,
      "crates/dakia-core/testdata/mime/charset-matrix.eml",
    );
    writeFileSync(drifted, `${readFileSync(drifted, "utf8")}\nDrift.`);
    assert.throws(() => validateFixtureCorpus(root), /checksum drift/);

    const extra = join(
      root,
      "crates/dakia-core/testdata/mime/unmanifested.eml",
    );
    writeFileSync(extra, "From: fixture@example.test\n\nfixture\n");
    assert.throws(
      () => validateFixtureCorpus(root),
      /Unmanifested fixture file/,
    );
  }));

test("rejects live domains, credential-like values, and non-exercising tests", () =>
  withCorpus((root) => {
    const [manifestPath, manifest] = manifestAt(root);
    const fixture = manifest.fixtures.find(
      (entry) => entry.id === "mime.charset-matrix",
    );
    const fixturePath = join(root, fixture.path);
    const changed = `${readFileSync(fixturePath, "utf8")}\nAuthorization: Bearer abcdefghijklmnop\nFrom: person@gmail.com\n`;
    writeFileSync(fixturePath, changed);
    fixture.sha256 = createHash("sha256").update(changed).digest("hex");
    fixture.exercisedBy[0].id = "missing fixture test";
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

    assert.throws(
      () => validateFixtureCorpus(root),
      (error) => {
        const message = String(error);
        return (
          /prohibited live domain: gmail\.com/.test(message) &&
          /credential-like/.test(message) &&
          /not exercised/.test(message)
        );
      },
    );
  }));

test("scans header URLs and bounded decoded MIME bodies and requires a real test declaration", () =>
  withCorpus((root) => {
    const [manifestPath, manifest] = manifestAt(root);
    const fixture = manifest.fixtures.find(
      (entry) => entry.id === "mime.charset-matrix",
    );
    const fixturePath = join(root, fixture.path);
    const secret = Buffer.from("password=production-secret", "utf8").toString(
      "base64",
    );
    const changed = `${readFileSync(fixturePath, "utf8")}\nList-Unsubscribe: <https://gmail.com/private-user-token>\nContent-Transfer-Encoding: base64\n\n${secret}\n`;
    writeFileSync(fixturePath, changed);
    fixture.sha256 = createHash("sha256").update(changed).digest("hex");
    fixture.exercisedBy[0].id = "parse_message";
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

    assert.throws(
      () => validateFixtureCorpus(root),
      (error) => {
        const message = String(error);
        return (
          /prohibited live domain: gmail\.com/.test(message) &&
          /credential-like/.test(message) &&
          /not exercised/.test(message)
        );
      },
    );
  }));

test("rejects an applicable publication path without a path-specific exercise", () =>
  withCorpus((root) => {
    const [manifestPath, manifest] = manifestAt(root);
    const fixture = manifest.fixtures.find(
      (entry) => entry.id === "mime.truncated-multipart-lf",
    );
    fixture.exercisedBy = fixture.exercisedBy.filter(
      (exercise) => exercise.domain !== "rust.mail.partial-preview",
    );
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

    assert.throws(
      () => validateFixtureCorpus(root),
      /applicable path is not exercised: partial-preview/,
    );
  }));

test("does not accept a fixture basename that appears only in a test comment", () =>
  withCorpus((root) => {
    const sourcePath = join(root, "crates/dakia-core/src/mail.rs");
    const source = readFileSync(sourcePath, "utf8").replace(
      '("charset-matrix", true),',
      '("charset-matrix-removed", true), // charset-matrix',
    );
    writeFileSync(sourcePath, source);

    assert.throws(
      () => validateFixtureCorpus(root),
      /Fixture mime\.charset-matrix is not exercised/,
    );
  }));

test("rejects a corpus case whose loader result is discarded or left unused", () =>
  withCorpus((root) => {
    const sourcePath = join(root, "crates/dakia-core/src/mail.rs");
    const source = readFileSync(sourcePath, "utf8")
      .replace(
        "let raw = mime_corpus(name);",
        "void mime_corpus(name); // this exercise is intentionally discarded",
      )
      .replace(
        'let raw = include_str!("../tests/fixtures/provider-signature-inline.eml");',
        'let unused = include_str!("../tests/fixtures/provider-signature-inline.eml");',
      );
    writeFileSync(sourcePath, source);

    assert.throws(
      () => validateFixtureCorpus(root),
      /Fixture mime\.charset-matrix is not exercised[\s\S]*Fixture mime\.provider-signature-inline is not exercised/,
    );
  }));

test("rejects a raw fixture import referenced only through void", () =>
  withCorpus((root) => {
    const sourcePath = join(
      root,
      "apps/desktop/src/components/HtmlMessage.test.tsx",
    );
    const source = readFileSync(sourcePath, "utf8").replace(
      "html={freshdeskReplySection}",
      "html={\"<p>replacement</p>\"}\n        {void freshdeskReplySection}",
    );
    writeFileSync(sourcePath, source);

    assert.throws(
      () => validateFixtureCorpus(root),
      /Fixture html\.freshdesk-reply-section is not exercised/,
    );
  }));
