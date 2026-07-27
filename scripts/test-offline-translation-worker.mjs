import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { access, mkdir, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";
import { createServer } from "vite";

const repository = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifest = JSON.parse(
  await readFile(
    join(repository, "apps/desktop/src-tauri/src/translation_models.json"),
    "utf8",
  ),
);
const allCases = [
  { source: "et", text: "Tere maailm" },
  {
    source: "et",
    text: "Tere Outlooki kiri",
    html: [
      "<!doctype html><html><head><style>p{color:#333}</style></head>",
      "<body><table><tr><td><p>Tere esimene dokument.</p></td></tr></table></body></html>",
      '<html xmlns:o="urn:schemas-microsoft-com:office:office"><head>',
      "<style>.outlook{font-family:Arial}</style></head>",
      '<body><table><tr><td class="outlook"><a href="https://example.test/details?x=1&amp;y=2">',
      "Tere teine dokument.</a></td></tr></table>",
      "</body></html>",
    ].join(""),
  },
  { source: "ar", text: "مرحبا بالعالم" },
  { source: "zh", text: "你好，世界" },
  { source: "ja", text: "こんにちは、世界" },
];
const requestedSources = new Set(
  (process.env.DAKIA_TRANSLATION_TEST_LANGUAGES ?? "et,ar,zh,ja").split(","),
);
const cases = allCases.filter(({ source }) => requestedSources.has(source));
assert(cases.length > 0, "No requested translation worker fixtures matched");
const fixtures = new Map();

for (const testCase of cases) {
  const model = manifest.models.find(
    (candidate) =>
      candidate.source === testCase.source && candidate.target === "en",
  );
  assert(model, `The pinned ${testCase.source}-to-English model is missing`);
  const cache = join(
    tmpdir(),
    "dakia-bergamot-worker-test",
    `${testCase.source}-en`,
  );
  await mkdir(cache, { recursive: true });
  const artifacts = [
    ["model.bin", model.files.model],
    ["shortlist.bin", model.files.shortlist],
    ...model.files.vocabs.map((artifact, index) => [
      `vocab-${index}.spm`,
      artifact,
    ]),
  ];
  fixtures.set(testCase.source, { cache, artifacts });

  for (const [name, [url, expectedBytes, expectedHash]] of artifacts) {
    const destination = join(cache, name);
    let valid = false;
    try {
      const bytes = await readFile(destination);
      valid =
        bytes.byteLength === expectedBytes &&
        createHash("sha256").update(bytes).digest("hex") === expectedHash;
    } catch {
      valid = false;
    }
    if (valid) continue;

    const response = await fetch(url);
    assert(response.ok, `Model download failed: ${response.status} ${url}`);
    const bytes = Buffer.from(await response.arrayBuffer());
    assert.equal(
      bytes.byteLength,
      expectedBytes,
      `${testCase.source}/${name} byte length differs from the pinned manifest`,
    );
    assert.equal(
      createHash("sha256").update(bytes).digest("hex"),
      expectedHash,
      `${testCase.source}/${name} SHA-256 differs from the pinned manifest`,
    );
    await writeFile(destination, bytes);
  }
}

const chromePath =
  process.env.DAKIA_TEST_CHROME ??
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
await access(chromePath);

const vite = await createServer({
  configFile: join(repository, "apps/desktop/vite.config.ts"),
  server: { host: "127.0.0.1", port: 0, strictPort: false },
});
await vite.listen();
const address = vite.httpServer?.address();
assert(address && typeof address === "object");
const baseUrl = `http://127.0.0.1:${address.port}`;

const browser = await chromium.launch({
  executablePath: chromePath,
  headless: true,
});
try {
  const page = await browser.newPage();
  const browserErrors = [];
  page.on("pageerror", (error) => browserErrors.push(error.message));
  for (const [source, { cache, artifacts }] of fixtures) {
    for (const [name] of artifacts) {
      await page.route(`${baseUrl}/test-model/${source}/${name}`, (route) =>
        route.fulfill({
          path: join(cache, name),
          contentType: "application/octet-stream",
        }),
      );
    }
    if (!artifacts.some(([name]) => name === "vocab-1.spm")) {
      await page.route(
        `${baseUrl}/test-model/${source}/vocab-1.spm`,
        (route) => route.fulfill({ status: 404 }),
        { times: 1 },
      );
    }
  }
  await page.goto(baseUrl);
  const result = [];
  for (const source of fixtures.keys()) {
    const translations = await page.evaluate(
      async (translationCases) => {
        const harness =
          await import("/src/test/offlineTranslationWorkerHarness.ts");
        return harness.runOfflineTranslationWorkerIntegration(translationCases);
      },
      cases.filter((testCase) => testCase.source === source),
    );
    result.push(...translations);
  }

  for (const translation of result) {
    assert.notEqual(translation.plain.trim(), translation.input);
    assert.match(
      translation.plain,
      /[A-Za-z]{2}/,
      `${translation.source} did not produce English-looking text`,
    );
    assert.match(translation.html, /<p[^>]*>.+<\/p>/s);
  }
  const outlookFixture = result.find(
    ({ input }) => input === "Tere Outlooki kiri",
  );
  assert(outlookFixture, "The malformed Outlook HTML fixture did not run");
  assert.equal((outlookFixture.html.match(/<html(?:\s|>)/g) ?? []).length, 1);
  assert.equal((outlookFixture.html.match(/<body(?:\s|>)/g) ?? []).length, 1);
  assert.match(
    outlookFixture.html,
    /href="https:\/\/example\.test\/details\?x=1&amp;y=2"/,
  );
  assert.match(outlookFixture.html, /class="outlook"/);
  assert.deepEqual(browserErrors, []);
  process.stdout.write(
    `Bergamot worker translated ${result.length} language fixtures locally: ${JSON.stringify(result)}\n`,
  );
} finally {
  await browser.close();
  await vite.close();
}
