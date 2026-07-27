import { writeFile } from "node:fs/promises";
import { resolve } from "node:path";

const registryUrl =
  "https://firefox.settings.services.mozilla.com/v1/buckets/main/collections/translations-models/records";
const attachmentBaseUrl =
  "https://firefox-settings-attachments.cdn.mozilla.net/";
const outputPath = resolve(
  "apps/desktop/src-tauri/src/translation_models.json",
);

// Whatlang 0.18's complete language set, expressed as the BCP-47 language
// codes used by Mozilla's model registry.
const whatlangLanguages = new Map(
  [
    ["af", "Afrikaans"],
    ["ak", "Akan"],
    ["am", "Amharic"],
    ["ar", "Arabic"],
    ["az", "Azerbaijani"],
    ["be", "Belarusian"],
    ["bg", "Bulgarian"],
    ["bn", "Bengali"],
    ["ca", "Catalan"],
    ["cs", "Czech"],
    ["cy", "Welsh"],
    ["da", "Danish"],
    ["de", "German"],
    ["el", "Greek"],
    ["en", "English"],
    ["eo", "Esperanto"],
    ["es", "Spanish"],
    ["et", "Estonian"],
    ["fa", "Persian"],
    ["fi", "Finnish"],
    ["fr", "French"],
    ["gu", "Gujarati"],
    ["he", "Hebrew"],
    ["hi", "Hindi"],
    ["hr", "Croatian"],
    ["hu", "Hungarian"],
    ["hy", "Armenian"],
    ["id", "Indonesian"],
    ["it", "Italian"],
    ["ja", "Japanese"],
    ["jv", "Javanese"],
    ["ka", "Georgian"],
    ["km", "Khmer"],
    ["kn", "Kannada"],
    ["ko", "Korean"],
    ["la", "Latin"],
    ["lt", "Lithuanian"],
    ["lv", "Latvian"],
    ["mk", "Macedonian"],
    ["ml", "Malayalam"],
    ["mr", "Marathi"],
    ["my", "Burmese"],
    ["nb", "Norwegian Bokmål"],
    ["ne", "Nepali"],
    ["nl", "Dutch"],
    ["or", "Odia"],
    ["pa", "Punjabi"],
    ["pl", "Polish"],
    ["pt", "Portuguese"],
    ["ro", "Romanian"],
    ["ru", "Russian"],
    ["si", "Sinhala"],
    ["sk", "Slovak"],
    ["sl", "Slovenian"],
    ["sn", "Shona"],
    ["sr", "Serbian"],
    ["sv", "Swedish"],
    ["ta", "Tamil"],
    ["te", "Telugu"],
    ["th", "Thai"],
    ["tk", "Turkmen"],
    ["tl", "Tagalog"],
    ["tr", "Turkish"],
    ["uk", "Ukrainian"],
    ["ur", "Urdu"],
    ["uz", "Uzbek"],
    ["vi", "Vietnamese"],
    ["yi", "Yiddish"],
    ["zh", "Chinese"],
    ["zu", "Zulu"],
  ].sort(([left], [right]) => left.localeCompare(right)),
);
const englishLanguageNames = new Intl.DisplayNames(["en"], {
  type: "language",
});

function appliesToDesktopRelease(expression = "") {
  if (!expression.trim()) return true;
  const evaluable = expression
    .replaceAll("env.appinfo.OS", JSON.stringify("Darwin"))
    .replaceAll("env.channel", JSON.stringify("release"));
  if (!/^[\s"'()=!&|A-Za-z]+$/.test(evaluable)) {
    throw new Error(`Unsupported Remote Settings filter: ${expression}`);
  }
  return Boolean(Function(`"use strict"; return (${evaluable});`)());
}

function compareVersions(left, right) {
  const parts = (version) =>
    version
      .split(/[.-]/)
      .map((part) => (/^\d+$/.test(part) ? Number(part) : part));
  const leftParts = parts(left);
  const rightParts = parts(right);
  for (
    let index = 0;
    index < Math.max(leftParts.length, rightParts.length);
    index += 1
  ) {
    const a = leftParts[index] ?? 0;
    const b = rightParts[index] ?? 0;
    if (a === b) continue;
    if (typeof a === "number" && typeof b === "number") return a - b;
    if (typeof a === "number") return 1;
    if (typeof b === "number") return -1;
    return a.localeCompare(b);
  }
  return 0;
}

function artifact(record) {
  return [
    new URL(record.attachment.location, attachmentBaseUrl).href,
    Number(record.attachment.size),
    record.attachment.hash,
  ];
}

function completeVersion(records) {
  const byType = new Map(records.map((record) => [record.fileType, record]));
  const vocabs = byType.has("vocab")
    ? [artifact(byType.get("vocab"))]
    : byType.has("srcvocab") && byType.has("trgvocab")
      ? [artifact(byType.get("srcvocab")), artifact(byType.get("trgvocab"))]
      : null;
  if (!byType.has("model") || !byType.has("lex") || !vocabs) return null;
  const modelRecord = byType.get("model");
  const config = modelRecord.name.endsWith("intgemm8.bin")
    ? { "gemm-precision": "int8shiftAll" }
    : {};
  return {
    files: {
      model: artifact(modelRecord),
      shortlist: artifact(byType.get("lex")),
      vocabs,
    },
    config,
  };
}

const response = await fetch(registryUrl);
if (!response.ok) {
  throw new Error(`Mozilla model registry returned HTTP ${response.status}`);
}
const { data: records } = await response.json();
const sourceToEnglishRecords = records.filter(
  (record) => record.toLang === "en",
);
const desktopRecords = sourceToEnglishRecords.filter((record) =>
  appliesToDesktopRelease(record.filter_expression),
);

function collectCandidates(candidateRecords) {
  const grouped = new Map();
  for (const record of candidateRecords) {
    const key = `${record.fromLang}\0${record.version}`;
    const group = grouped.get(key) ?? [];
    group.push(record);
    grouped.set(key, group);
  }
  const result = new Map();
  for (const [key, versionRecords] of grouped) {
    const [registrySource, version] = key.split("\0");
    const complete = completeVersion(versionRecords);
    if (!complete) continue;
    const versions = result.get(registrySource) ?? [];
    versions.push({ version, ...complete });
    result.set(registrySource, versions);
  }
  return result;
}
const candidates = collectCandidates(desktopRecords);
const allCandidates = collectCandidates(sourceToEnglishRecords);

const models = [];
const excludedModels = [];
for (const registrySource of [...allCandidates.keys()].sort()) {
  const versions = candidates.get(registrySource);
  const source = registrySource === "zh-Hans" ? "zh" : registrySource;
  const sourceName = whatlangLanguages.get(source);
  if (!versions || !sourceName || registrySource === "zh-Hant") {
    excludedModels.push({
      source: registrySource,
      sourceName:
        registrySource === "zh-Hant"
          ? "Chinese (Traditional)"
          : (sourceName ?? englishLanguageNames.of(registrySource)),
      reason: !versions
        ? "Mozilla has a model record for this language, but it is not enabled for Desktop Release."
        : registrySource === "zh-Hant"
          ? "Whatlang reports Chinese as Mandarin and does not reliably distinguish Traditional from Simplified script."
          : `Whatlang 0.18 does not detect ${registrySource} as a distinct language.`,
    });
    continue;
  }
  const selected = versions.sort((left, right) =>
    compareVersions(right.version, left.version),
  )[0];
  models.push({
    source,
    sourceName,
    target: "en",
    registrySource,
    version: selected.version,
    files: selected.files,
    config: selected.config,
  });
}

models.sort((left, right) => left.source.localeCompare(right.source));
excludedModels.sort((left, right) => left.source.localeCompare(right.source));

const manifest = {
  registryUrl,
  registryLastModified: Math.max(
    ...desktopRecords.map((record) => Number(record.last_modified)),
  ),
  models,
  excludedModels,
};
await writeFile(outputPath, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(
  `Pinned ${models.length} released source-to-English models; documented ${excludedModels.length} exclusions.`,
);
