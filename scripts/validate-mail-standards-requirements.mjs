import { readFileSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const REQUIREMENTS_PATH = "testdata/mail-standards-requirements.json";
const REQUIRED_RFCS = [
  3501, 9051, 5321, 5322, 6531, 2045, 2047, 2231, 3676, 8058, 6068, 8997, 2046,
  1870,
];
// Reviewed profile inventory. Removing a requirement needs an explicit profile change,
// not just deletion of an uncomfortable gap from the JSON document.
const REQUIRED_CLAUSES = {
  3501: [
    "tag",
    "epoch",
    "sequence",
    "seen",
    "literals",
    "nil",
    "posttls",
    "failure",
    "logindisabled",
    "sasl",
    "utf7",
    "list",
    "peek",
    "section",
    "store",
    "bye",
    "logout-drain",
    "logout",
    "logout-ok-only",
    "expunge",
    "search",
  ],
  9051: [
    "idle-done",
    "idle-renew",
    "move-policy",
    "uid-expunge",
    "no-uidplus",
    "rev2-enable",
    "logout-drain",
    "logout",
  ],
  5321: [
    "multiline",
    "recipients",
    "data",
    "transparency",
    "uncertain",
    "path-length",
    "command-length",
    "reply-length",
    "text-line",
    "timeouts",
    "ehlo-fallback",
  ],
  5322: [
    "header-injection",
    "bcc",
    "unfold",
    "duplicate-singletons",
    "date",
    "reply-headers",
    "line-limits",
    "required-fields",
    "obsolete-input",
  ],
  6531: ["capability", "parameter", "idna", "utf8-replies"],
  2045: [
    "quoted-printable",
    "base64",
    "unknown-transfer",
    "unknown-as-binary",
    "default-type",
    "default-transfer",
    "parameter-case",
    "mime-version",
    "qp-boundaries",
    "base64-boundaries",
  ],
  2047: [
    "q-header",
    "b-filename",
    "placement",
    "adjacent-words",
    "encoded-word-length",
  ],
  2231: ["continuations", "extended", "language", "invalid-continuations"],
  3676: ["delsp", "delsp-default", "signature", "unstuff", "quotes"],
  8058: ["post", "consent", "dkim", "redirect", "no-cookies"],
  6068: [
    "percent",
    "fields",
    "unsafe-headers",
    "frontend-injection",
    "body-crlf",
    "no-send",
  ],
  8997: ["min-version", "confidentiality-ui"],
  2046: [
    "nested",
    "alternative",
    "mixed",
    "forwarded",
    "truncated",
    "boundary-limit",
    "digest",
    "unknown-multipart",
    "default-charset",
  ],
  1870: ["octets", "maximum", "zero"],
};

const STATUSES = new Set([
  "tested",
  "unsupported",
  "intentionally_tolerated",
  "gap",
]);

// Keep offsets and newlines while hiding Rust comments and literal contents.
// This is a reference validator, not a Rust parser or an execution proof.
function rustCode(source) {
  const result = source.split("");
  const blank = (start, end) => {
    for (let index = start; index < end; index += 1) {
      if (source[index] !== "\n" && source[index] !== "\r") result[index] = " ";
    }
  };
  for (let cursor = 0; cursor < source.length;) {
    const start = cursor;
    if (source.startsWith("//", cursor)) {
      cursor = source.indexOf("\n", cursor);
      if (cursor < 0) cursor = source.length;
    } else if (source.startsWith("/*", cursor)) {
      let depth = 1;
      cursor += 2;
      while (cursor < source.length && depth > 0) {
        if (source.startsWith("/*", cursor)) {
          depth += 1;
          cursor += 2;
        } else if (source.startsWith("*/", cursor)) {
          depth -= 1;
          cursor += 2;
        } else cursor += 1;
      }
    } else {
      const raw = /^(?:br|cr|r)(#*)"/.exec(source.slice(cursor));
      if (raw && !/[a-zA-Z0-9_]/.test(source[cursor - 1] ?? "")) {
        const end = source.indexOf(`"${raw[1]}`, cursor + raw[0].length);
        cursor = end < 0 ? source.length : end + raw[1].length + 1;
      } else if (source[cursor] === '"') {
        cursor += 1;
        while (cursor < source.length) {
          if (source[cursor] === "\\") cursor += 2;
          else if (source[cursor++] === '"') break;
        }
      } else {
        const character =
          /^'(?:\\(?:u\{[a-fA-F0-9_]+\}|x[a-fA-F0-9]{2}|.)|[^'\\\r\n])'/u.exec(
            source.slice(cursor),
          );
        if (character) cursor += character[0].length;
        else {
          cursor += 1;
          continue;
        }
      }
    }
    blank(start, cursor);
  }
  return result.join("");
}

function hasDirectHelperAssertion(code) {
  let depth = 0;
  for (let cursor = 0; cursor < code.length; cursor += 1) {
    if (code[cursor] === "{") depth += 1;
    else if (code[cursor] === "}") depth -= 1;
    else if (depth === 1 && /^(?:return|throw)\b/.test(code.slice(cursor)))
      return false;
    else if (
      depth === 1 &&
      /^assert(?:_eq|_ne)?!\s*\(/.test(code.slice(cursor))
    )
      return true;
  }
  return false;
}

function directlyDelegatesToHelper(code, name) {
  const opening = code.indexOf("{");
  const closing = code.lastIndexOf("}");
  if (opening < 0 || closing <= opening) return false;
  let remaining = code.slice(opening + 1, closing).trim();
  let calls = 0;
  // Only straight-line statement calls qualify. References, closures,
  // conditional/dead branches, assignments and arbitrary expressions do not.
  while (remaining) {
    if (!remaining.startsWith(`${name}(`)) return false;
    let cursor = name.length;
    let depth = 0;
    do {
      const token = remaining[cursor++];
      if (token === "(") depth += 1;
      else if (token === ")") depth -= 1;
    } while (cursor < remaining.length && depth > 0);
    if (depth !== 0) return false;
    const suffix = /^\s*(?:\.await\s*)?;/.exec(remaining.slice(cursor));
    if (!suffix) return false;
    remaining = remaining.slice(cursor + suffix[0].length).trim();
    calls += 1;
  }
  return calls > 0;
}

// As in the catalogue's each-table scanner, match balanced delimiters rather
// than the first ')' in a nested table. Mask non-executable text first so a
// comment or string cannot declare a test, helper call, or assertion.
function javascriptCode(source, markLiterals = false) {
  const result = source.split("");
  const blank = (start, end) => {
    for (let index = start; index < end; index += 1)
      if (!/[\r\n]/.test(source[index])) result[index] = " ";
  };
  for (let cursor = 0; cursor < source.length;) {
    const start = cursor;
    let literal = false;
    if (source.startsWith("//", cursor)) {
      cursor = source.indexOf("\n", cursor);
      if (cursor < 0) cursor = source.length;
    } else if (source.startsWith("/*", cursor)) {
      const end = source.indexOf("*/", cursor + 2);
      cursor = end < 0 ? source.length : end + 2;
    } else if (["'", '"', "`"].includes(source[cursor])) {
      literal = true;
      const quote = source[cursor++];
      while (cursor < source.length) {
        if (source[cursor] === "\\") cursor += 2;
        else if (source[cursor++] === quote) break;
      }
    } else if (
      source[cursor] === "/" &&
      /(?:[=(:,[!&|?{;]|=>|\breturn)\s*$/.test(result.slice(0, cursor).join(""))
    ) {
      cursor += 1;
      let characterClass = false;
      while (cursor < source.length) {
        const character = source[cursor++];
        if (character === "\\") cursor += 1;
        else if (character === "[") characterClass = true;
        else if (character === "]") characterClass = false;
        else if (character === "/" && !characterClass) break;
        else if (character === "\n") break;
      }
      while (/[a-z]/i.test(source[cursor] ?? "")) cursor += 1;
    } else {
      cursor += 1;
      continue;
    }
    blank(start, cursor);
    if (markLiterals && literal) result[start] = "0";
  }
  return result.join("");
}

function matchingDelimiter(code, opening, left, right) {
  let depth = 0;
  for (let cursor = opening; cursor < code.length; cursor += 1) {
    if (code[cursor] === left) depth += 1;
    else if (code[cursor] === right && --depth === 0) return cursor;
  }
  return -1;
}

// Deliberately recognize only literal nonempty tables, optionally followed by
// one flatMap whose callback returns another literal nonempty array. Unknown
// expressions, holes, spreads and filters are not execution evidence. This is
// a conservative declaration check, not evaluation of JavaScript table code.
function staticallyNonemptyEachTable(source) {
  const code = javascriptCode(source, true).trim().replace(/,\s*$/, "");
  function nonemptyArray(start) {
    if (code[start] !== "[") return -1;
    const end = matchingDelimiter(code, start, "[", "]");
    if (end < 0) return -1;
    const entries = [];
    let itemStart = start + 1;
    let depth = 0;
    for (let at = itemStart; at < end; at += 1) {
      if ("([{ ".trim().includes(code[at])) depth += 1;
      else if (")]}".includes(code[at])) depth -= 1;
      else if (code[at] === "," && depth === 0) {
        entries.push(code.slice(itemStart, at).trim());
        itemStart = at + 1;
      }
    }
    const last = code.slice(itemStart, end).trim();
    if (last) entries.push(last);
    return entries.length &&
      entries.every((entry) => entry && !entry.startsWith("..."))
      ? end
      : -1;
  }
  const baseEnd = nonemptyArray(0);
  if (baseEnd < 0) return false;
  const suffix = code.slice(baseEnd + 1).trim();
  if (!suffix) return true;
  const flatMap = /^\.flatMap\s*\(/.exec(suffix);
  if (!flatMap) return false;
  const callOpen = code.indexOf("(", baseEnd + 1);
  const callEnd = matchingDelimiter(code, callOpen, "(", ")");
  if (callEnd < 0 || code.slice(callEnd + 1).trim()) return false;
  const callback = code.slice(callOpen + 1, callEnd);
  const signature = /^\s*(?:\([^()]*\)|[A-Za-z_$][\w$]*)\s*=>\s*/.exec(
    callback,
  );
  if (!signature) return false;
  const resultStart = callOpen + 1 + signature[0].length;
  const resultEnd = nonemptyArray(resultStart);
  return resultEnd >= 0 && !code.slice(resultEnd + 1, callEnd).trim();
}

function javascriptBody(source, code, name, requireTest) {
  if (!requireTest) {
    const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const declaration = new RegExp(
      `\\b(?:async\\s+)?function\\s+${escaped}\\s*\\(`,
    ).exec(code);
    if (!declaration) return undefined;
    const parameters = declaration.index + declaration[0].lastIndexOf("(");
    const parameterEnd = matchingDelimiter(code, parameters, "(", ")");
    const opening = code.indexOf("{", parameterEnd + 1);
    const closing = matchingDelimiter(code, opening, "{", "}");
    return parameterEnd >= 0 && opening >= 0 && closing >= 0
      ? source.slice(opening, closing + 1)
      : undefined;
  }
  const starts = /(?<![\w$.])(?:it|test)(\.each)?\s*\(/g;
  for (const declaration of code.matchAll(starts)) {
    let opening = declaration.index + declaration[0].lastIndexOf("(");
    if (declaration[1]) {
      const tableEnd = matchingDelimiter(code, opening, "(", ")");
      if (
        tableEnd < 0 ||
        !staticallyNonemptyEachTable(source.slice(opening + 1, tableEnd))
      )
        continue;
      opening = tableEnd + 1;
      while (/\s/.test(code[opening] ?? "")) opening += 1;
      if (code[opening] !== "(") continue;
    }
    let titleStart = opening + 1;
    while (/\s/.test(source[titleStart] ?? "")) titleStart += 1;
    const quote = source[titleStart];
    if (!["'", '"', "`"].includes(quote)) continue;
    let titleEnd = titleStart + 1;
    while (titleEnd < source.length) {
      if (source[titleEnd] === "\\") titleEnd += 2;
      else if (source[titleEnd] === quote) break;
      else titleEnd += 1;
    }
    if (source.slice(titleStart + 1, titleEnd) !== name) continue;
    let separator = titleEnd + 1;
    while (/\s/.test(code[separator] ?? "")) separator += 1;
    if (code[separator] !== ",") continue;
    const callEnd = matchingDelimiter(code, opening, "(", ")");
    let callbackStart = separator + 1;
    while (/\s/.test(code[callbackStart] ?? "")) callbackStart += 1;
    const functionCallback =
      /^(?:async\s+)?function(?:\s+[A-Za-z_$][\w$]*)?\s*\(/.exec(
        code.slice(callbackStart),
      );
    let bodyStart;
    if (functionCallback) {
      const parameterOpen =
        callbackStart + functionCallback[0].lastIndexOf("(");
      const parameterEnd = matchingDelimiter(code, parameterOpen, "(", ")");
      if (parameterEnd < 0 || parameterEnd >= callEnd) continue;
      bodyStart = parameterEnd + 1;
    } else {
      let parameterStart = callbackStart;
      const asyncPrefix = /^async(?=\s|\()/.exec(code.slice(parameterStart));
      if (asyncPrefix) parameterStart += asyncPrefix[0].length;
      while (/\s/.test(code[parameterStart] ?? "")) parameterStart += 1;
      let parameterEnd;
      if (code[parameterStart] === "(") {
        parameterEnd = matchingDelimiter(code, parameterStart, "(", ")");
        if (parameterEnd < 0) continue;
        parameterEnd += 1;
      } else {
        const parameter = /^[A-Za-z_$][\w$]*/.exec(code.slice(parameterStart));
        if (!parameter) continue;
        parameterEnd = parameterStart + parameter[0].length;
      }
      while (/\s/.test(code[parameterEnd] ?? "")) parameterEnd += 1;
      if (
        code.slice(parameterEnd, parameterEnd + 2) !== "=>" ||
        parameterEnd >= callEnd
      )
        continue;
      bodyStart = parameterEnd + 2;
    }
    while (/\s/.test(code[bodyStart] ?? "")) bodyStart += 1;
    if (code[bodyStart] !== "{") continue;
    const bodyEnd = matchingDelimiter(code, bodyStart, "{", "}");
    if (bodyEnd < 0 || bodyEnd > callEnd) continue;
    // The callback must be the second argument itself, not a function value
    // discarded by an &&/||/conditional suffix or another expression.
    if (!/^\s*,?\s*$/.test(code.slice(bodyEnd + 1, callEnd))) continue;
    return source.slice(bodyStart, bodyEnd + 1);
  }
  return undefined;
}

function javascriptTopLevelToken(
  code,
  token,
  requireAwait = false,
  targetOffset = null,
) {
  let depth = 0;
  let statementStart = code.indexOf("{") + 1;
  for (let cursor = 0; cursor < code.length; cursor += 1) {
    if (code[cursor] === "{") depth += 1;
    else if (code[cursor] === "}") depth -= 1;
    else if (depth === 1 && code[cursor] === ";") statementStart = cursor + 1;
    else if (depth === 1 && /^(?:return|throw)\b/.test(code.slice(cursor)))
      return false;
    if (depth !== 1 || !code.startsWith(`${token}(`, cursor)) continue;
    const prefix = code.slice(statementStart, cursor);
    if (targetOffset !== null && cursor !== targetOffset) continue;
    if (requireAwait && !/\bawait\s*$/.test(prefix)) continue;
    if (
      /^\s*(?:(?:const|let)\s+(?:[\w$]+|\{[\w\s,:]+\})\s*=\s*)?(?:await\s+)?$/.test(
        prefix,
      )
    )
      return true;
  }
  return false;
}

function executableJavascriptAssertion(source, token) {
  const code = javascriptCode(source);
  for (
    let cursor = source.indexOf(token);
    cursor >= 0;
    cursor = source.indexOf(token, cursor + 1)
  ) {
    if (
      code.startsWith("expect(", cursor) &&
      javascriptTopLevelToken(code, "expect", false, cursor)
    )
      return true;
  }
  return false;
}

function testBody(
  source,
  name,
  path,
  { requireTest = true, code: providedCode } = {},
) {
  if (typeof name !== "string" || !name) return undefined;
  if (path.endsWith(".rs")) {
    const code = providedCode ?? rustCode(source);
    const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const match = new RegExp(
      `^([ \\t]*)(?:async )?fn ${escaped}\\s*\\(`,
      "m",
    ).exec(code);
    if (!match) return undefined;
    const start = match.index;
    const rest = code.slice(start + match[0].length);
    const next = new RegExp(`^${match[1]}(?:#\\[|(?:async )?fn )`, "m").exec(
      rest,
    );
    const body = source.slice(
      start,
      next ? start + match[0].length + next.index : undefined,
    );
    const prefix = code.slice(Math.max(0, start - 120), start);
    return !requireTest || /#\[(?:tokio::)?test\]/.test(prefix)
      ? body
      : undefined;
  }
  return javascriptBody(
    source,
    providedCode ?? javascriptCode(source),
    name,
    requireTest,
  );
}

export function validateMailStandardsRequirements(
  document,
  { root = process.cwd(), read = readFileSync } = {},
) {
  const errors = [];
  const fail = (id, detail) => errors.push(`${id}: ${detail}`);
  if (document?.schemaVersion !== 1)
    fail("document", "schemaVersion must be 1");
  if (
    !Array.isArray(document?.requirements) ||
    document.requirements.length === 0
  )
    return [...errors, "document: requirements must be a nonempty array"];
  const catalogueIds = new Set(
    JSON.parse(
      read(resolve(root, "testdata/mail-safety-catalogue.json"), "utf8"),
    ).scenarios.map((scenario) => scenario.id),
  );
  const sourceCache = new Map();
  const ids = new Set();
  const rfcs = new Set();
  for (const row of document.requirements) {
    if (!row || typeof row !== "object") {
      fail("row", "requirement must be an object");
      continue;
    }
    const id = row?.id ?? "missing ID";
    if (!/^MSR-\d{4}-[a-z][a-z0-9-]*$/.test(id) || ids.has(id))
      fail(id, "invalid or duplicate stable ID");
    ids.add(id);
    if (!REQUIRED_RFCS.includes(row.rfc))
      fail(id, "RFC is outside the declared profile");
    rfcs.add(row.rfc);
    if (!/^\d+(?:\.\d+)*$/.test(row.section ?? ""))
      fail(id, "numeric RFC section required");
    if (
      row.source !==
      `https://www.rfc-editor.org/rfc/rfc${row.rfc}.html#section-${row.section}`
    )
      fail(id, "official clause URL must match RFC and section");
    for (const field of ["requirement", "applicability", "profile", "reason"]) {
      if (
        typeof row[field] !== "string" ||
        row[field].trim().length < (field === "profile" ? 3 : 10)
      )
        fail(id, `${field} must explain the scoped claim`);
    }
    if (!STATUSES.has(row.status)) fail(id, "invalid status");
    if (
      !Array.isArray(row.catalogueIds) ||
      !row.catalogueIds.every(
        (value) => /^MS-[A-L]-\d{3}$/.test(value) && catalogueIds.has(value),
      )
    )
      fail(id, "catalogueIds must contain stable catalogue IDs");
    if (!Array.isArray(row.evidence)) {
      fail(id, "evidence must be an array");
      continue;
    }
    if (
      ["tested", "intentionally_tolerated"].includes(row.status) &&
      row.evidence.length === 0
    )
      fail(id, "tested/tolerated claim needs a source assertion");
    for (const evidence of row.evidence) {
      if (
        typeof evidence.path !== "string" ||
        isAbsolute(evidence.path) ||
        relative(resolve(root), resolve(root, evidence.path)).startsWith("..")
      ) {
        fail(id, "evidence path must stay inside repository");
        continue;
      }
      try {
        let cached = sourceCache.get(evidence.path);
        if (!cached) {
          const source = read(resolve(root, evidence.path), "utf8");
          cached = {
            source,
            code: evidence.path.endsWith(".rs")
              ? rustCode(source)
              : javascriptCode(source),
          };
          sourceCache.set(evidence.path, cached);
        }
        const { source, code } = cached;
        const body = testBody(source, evidence.test, evidence.path, { code });
        if (!body) {
          fail(
            id,
            `test declaration missing: ${evidence.path}#${evidence.test}`,
          );
          continue;
        }
        if (
          typeof evidence.assertion !== "string" ||
          evidence.assertion.length < 4 ||
          !body.includes(evidence.assertion)
        )
          fail(
            id,
            `assertion token missing inside named test: ${evidence.assertion}`,
          );
        const isRust = evidence.path.endsWith(".rs");
        const bodyCode = isRust ? rustCode(body) : javascriptCode(body);
        if (
          !isRust &&
          evidence.assertion?.startsWith("expect(") &&
          !executableJavascriptAssertion(body, evidence.assertion)
        )
          fail(
            id,
            "assertion token is not an executable top-level callback assertion",
          );
        let directDelegation = false;
        if (evidence.assertionHelper) {
          const helperName = evidence.assertionHelper;
          const validName = /^[a-zA-Z_][a-zA-Z0-9_]*$/.test(helperName);
          const helper =
            validName &&
            testBody(source, helperName, evidence.path, {
              requireTest: false,
              code,
            });
          const helperValid =
            validName &&
            (isRust || javascriptTopLevelToken(bodyCode, helperName, true)) &&
            new RegExp(`\\b${helperName}\\s*\\(`).test(bodyCode) &&
            helper &&
            typeof evidence.helperAssertion === "string" &&
            helper.includes(evidence.helperAssertion) &&
            (isRust
              ? hasDirectHelperAssertion(rustCode(helper))
              : executableJavascriptAssertion(
                  helper,
                  evidence.helperAssertion,
                ));
          if (!helperValid) {
            fail(
              id,
              "delegated assertion helper is not linked or has no matching assertion",
            );
          } else {
            directDelegation = isRust
              ? directlyDelegatesToHelper(bodyCode, helperName)
              : javascriptTopLevelToken(bodyCode, helperName, true);
          }
        }
        const ownAssertion = isRust
          ? /\bassert(?:_eq|_ne)?!\s*\(/.test(bodyCode)
          : javascriptTopLevelToken(bodyCode, "expect");
        if (!ownAssertion && !directDelegation)
          fail(
            id,
            "named test has no assertion or direct validated helper delegation",
          );
      } catch (error) {
        fail(id, `evidence cannot be read: ${error.message}`);
      }
    }
  }
  for (const rfc of REQUIRED_RFCS)
    if (!rfcs.has(rfc)) fail("document", `required RFC ${rfc} is missing`);
  for (const [rfc, clauses] of Object.entries(REQUIRED_CLAUSES)) {
    for (const clause of clauses)
      if (!ids.has(`MSR-${rfc}-${clause}`))
        fail("document", `reviewed clause MSR-${rfc}-${clause} is missing`);
  }
  if (
    JSON.stringify([...(document.standards ?? [])].sort()) !==
    JSON.stringify([...REQUIRED_RFCS].sort())
  )
    fail("document", "standards inventory must match the selected profile");
  return errors;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  try {
    const args = process.argv.slice(2);
    if (
      args.length !== 0 &&
      !(args.length === 2 && args[0] === "--requirements")
    )
      throw new Error(
        "Usage: node scripts/validate-mail-standards-requirements.mjs [--requirements PATH]",
      );
    const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
    const document = JSON.parse(
      readFileSync(resolve(root, args[1] ?? REQUIREMENTS_PATH), "utf8"),
    );
    const errors = validateMailStandardsRequirements(document, { root });
    if (errors.length) throw new Error(errors.join("\n"));
    const counts = Object.fromEntries(
      [...STATUSES].map((status) => [
        status,
        document.requirements.filter((row) => row.status === status).length,
      ]),
    );
    process.stdout.write(
      `Mail standards profile valid: ${document.requirements.length} clauses ${JSON.stringify(counts)}\n`,
    );
  } catch (error) {
    process.stderr.write(`Mail standards profile invalid: ${error.message}\n`);
    process.exitCode = 1;
  }
}
