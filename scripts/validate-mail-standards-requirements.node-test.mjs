import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import {
  REQUIREMENTS_PATH,
  validateMailStandardsRequirements,
} from "./validate-mail-standards-requirements.mjs";

const root = resolve(import.meta.dirname, "..");
const baseline = JSON.parse(
  readFileSync(resolve(root, REQUIREMENTS_PATH), "utf8"),
);
const copy = () => structuredClone(baseline);
const validate = (document, options = {}) =>
  validateMailStandardsRequirements(document, { root, ...options });
const requirement = (document, id) =>
  document.requirements.find((row) => row.id === id);

test("reviewed clause profile resolves every named source assertion", () => {
  assert.deepEqual(validate(baseline), []);
  assert.equal(baseline.requirements.length, 102);
});

test("a deleted reviewed clause cannot silently reduce the assurance profile", () => {
  const document = copy();
  document.requirements = document.requirements.filter(
    (row) => row.id !== "MSR-8997-min-version",
  );
  assert.ok(
    validate(document).some((error) =>
      error.includes("reviewed clause MSR-8997-min-version is missing"),
    ),
  );
});

test("selected LOGOUT response rules cannot be deleted from either IMAP profile", () => {
  for (const id of [
    "MSR-3501-logout-drain",
    "MSR-3501-logout",
    "MSR-3501-logout-ok-only",
    "MSR-9051-logout-drain",
    "MSR-9051-logout",
  ]) {
    const document = copy();
    document.requirements = document.requirements.filter(
      (row) => row.id !== id,
    );
    assert.ok(
      validate(document).some((error) =>
        error.includes(`reviewed clause ${id} is missing`),
      ),
      id,
    );
  }
});

test("tested status requires assertion evidence and every decision needs a reason", () => {
  const document = copy();
  const row = requirement(document, "MSR-8997-min-version");
  row.status = "tested";
  row.reason = "";
  row.evidence = [];
  const errors = validate(document);
  assert.ok(errors.some((error) => error.includes("needs a source assertion")));
  assert.ok(errors.some((error) => error.includes("reason must explain")));
});

test("a token in another test cannot satisfy the named test", () => {
  const document = copy();
  const row = requirement(document, "MSR-3676-delsp");
  row.evidence[0].assertion = 'Some("café.pdf")';
  assert.ok(
    validate(document).some((error) =>
      error.includes("assertion token missing inside named test"),
    ),
  );
});

test("removing a source assertion makes the declaration fail", () => {
  const document = copy();
  const evidence = requirement(document, "MSR-3676-delsp-default").evidence[0];
  const read = (path, encoding) => {
    const source = readFileSync(path, encoding);
    return path.endsWith("/flowed.rs")
      ? source.replaceAll(evidence.assertion, "broken placeholder")
      : source;
  };
  assert.ok(
    validate(document, { read }).some((error) =>
      error.includes("assertion token missing inside named test"),
    ),
  );
});

test("renamed tests, helper-only functions and path traversal are rejected", () => {
  const document = copy();
  const evidence = requirement(document, "MSR-3501-tag").evidence[0];
  evidence.test = "assert_failed_sync_survives_reopen";
  assert.ok(
    validate(document).some((error) =>
      error.includes("test declaration missing"),
    ),
  );
  evidence.path = "../outside.rs";
  assert.ok(
    validate(document).some((error) =>
      error.includes("path must stay inside repository"),
    ),
  );
});

test("delegated assertions must resolve inside a helper called by that test", () => {
  const document = copy();
  requirement(document, "MSR-3501-tag").evidence[0].assertionHelper =
    "unrelated_helper";
  assert.ok(
    validate(document).some((error) =>
      error.includes("delegated assertion helper"),
    ),
  );
});

test("wrong section URLs, unknown catalogue links and duplicate IDs fail", () => {
  const document = copy();
  const row = document.requirements[0];
  row.source = "https://example.test/fake-rfc";
  row.catalogueIds = ["MS-A-999"];
  document.requirements.push(structuredClone(row));
  const errors = validate(document);
  assert.ok(errors.some((error) => error.includes("official clause URL")));
  assert.ok(errors.some((error) => error.includes("catalogueIds")));
  assert.ok(errors.some((error) => error.includes("duplicate stable ID")));
});

test("missing RFC and malformed rows do not become a pass", () => {
  const document = copy();
  document.standards = document.standards.filter((rfc) => rfc !== 3501);
  document.requirements.push(null);
  document.requirements[0].status = "passed";
  const errors = validate(document);
  assert.ok(errors.some((error) => error.includes("inventory")));
  assert.ok(
    errors.some((error) => error.includes("requirement must be an object")),
  );
  assert.ok(errors.some((error) => error.includes("invalid status")));
});

function validateDelegatedFixture(
  body,
  helperBody = 'assert_eq!(1, 1, "known assertion");',
  { prefix = "" } = {},
) {
  const document = copy();
  const path = "virtual-standards.rs";
  requirement(document, "MSR-3501-search").evidence = [
    {
      path,
      test: "delegated_wrapper",
      assertion: "assert_transport(",
      assertionHelper: "assert_transport",
      helperAssertion: "known assertion",
    },
  ];
  const virtualSource = `${prefix}\n#[tokio::test]\nasync fn delegated_wrapper() {\n${body}\n}\nasync fn assert_transport(_value: &str) {\n${helperBody}\n}\n`;
  return validate(document, {
    read: (file, encoding) =>
      file.endsWith(`/${path}`) ? virtualSource : readFileSync(file, encoding),
  });
}

test("straight-line awaited helper calls prove a delegated test without its own assertion", () => {
  assert.deepEqual(
    validateDelegatedFixture(
      'assert_transport("first").await;\nassert_transport("second").await;',
    ),
    [],
  );
  assert.deepEqual(
    validateDelegatedFixture(
      '/* direct call follows */ assert_transport("literal with { braces }");',
    ),
    [],
  );
});

test("comments, strings, raw strings, references and dead branches cannot fake helper delegation", () => {
  for (const body of [
    '// assert_transport("fixture").await;',
    '/* nested /* comment */ assert_transport("fixture").await; */',
    'let _ = "assert_transport(fixture)";',
    'let _ = r###"assert_transport("fixture").await;"###;',
    "let _reference = assert_transport;",
    'if false { assert_transport("fixture").await; }',
    'let _closure = || assert_transport("fixture");',
    'let _future = async { assert_transport("fixture").await; };',
  ]) {
    assert.ok(
      validateDelegatedFixture(body).some(
        (error) =>
          error.includes("delegated assertion helper") ||
          error.includes("no assertion or direct validated helper delegation"),
      ),
      body,
    );
  }
});

test("a helper needs a real direct assertion rather than assertion-looking comments or dead code", () => {
  for (const helper of [
    '// assert_eq!(1, 1, "known assertion");',
    'let _ = r#"assert_eq!(1, 1, "known assertion");"#;',
    'if false { assert_eq!(1, 1, "known assertion"); }',
    'return; assert_eq!(1, 1, "known assertion");',
  ]) {
    assert.ok(
      validateDelegatedFixture(
        'assert_transport("fixture").await;',
        helper,
      ).some((error) => error.includes("delegated assertion helper")),
      helper,
    );
  }
});

test("test declarations inside Rust raw string data are not executable test evidence", () => {
  const document = copy();
  const row = requirement(document, "MSR-3501-search");
  row.evidence = [
    {
      path: "virtual-standards.rs",
      test: "fake_test",
      assertion: "assert_eq!",
    },
  ];
  const source =
    'const DATA: &str = r#"\n#[tokio::test]\nasync fn fake_test() { assert_eq!(1, 1); }\n"#;';
  assert.ok(
    validate(document, {
      read: (path, encoding) =>
        path.endsWith("/virtual-standards.rs")
          ? source
          : readFileSync(path, encoding),
    }).some((error) => error.includes("test declaration missing")),
  );
});

function validateJavascriptFixture(source, evidence = {}) {
  const document = copy();
  const path = "virtual-mailto.test.ts";
  requirement(document, "MSR-6068-frontend-injection").evidence = [
    {
      path,
      test: "rejects $lineBreak in $field",
      assertion: "expect(send).not.toHaveBeenCalled()",
      ...evidence,
    },
  ];
  return validate(document, {
    read: (file, encoding) =>
      file.endsWith(`/${path}`) ? source : readFileSync(file, encoding),
  });
}

test("each declarations resolve the exact later title after nested tables and isolate its callback", () => {
  const source = `
    it.each(["earlier"])("unrelated $field", () => { expect(other).toBe(1); });
    it.each([{ lineBreak: "CR" }].flatMap((entry) => [
      { field: "To", value: nested(entry, { braces: "(})" }) },
      { field: "Cc", value: /[{}()]/ },
    ]))(
      "rejects $lineBreak in $field",
      async ({ field }) => { expect(send).not.toHaveBeenCalled(); },
    );
  `;
  assert.deepEqual(validateJavascriptFixture(source), []);
  assert.ok(
    validateJavascriptFixture(source, {
      test: "rejects CR in To",
    }).some((error) => error.includes("test declaration missing")),
  );
  assert.ok(
    validateJavascriptFixture(source, {
      assertion: "expect(other).toBe(1)",
    }).some((error) =>
      error.includes("assertion token missing inside named test"),
    ),
  );
});

test("each tables, following tests, comments and string declarations cannot supply callback assertions", () => {
  for (const source of [
    'it.each([() => expect(send).not.toHaveBeenCalled()])("rejects $lineBreak in $field", () => {});',
    'it.each([1])("rejects $lineBreak in $field", () => {}); it("later", () => { expect(send).not.toHaveBeenCalled(); });',
    '// it.each([1])("rejects $lineBreak in $field", () => { expect(send).not.toHaveBeenCalled(); });',
    '/* it.each([1])("rejects $lineBreak in $field", () => { expect(send).not.toHaveBeenCalled(); }); */',
    'const prose = `it.each([1])("rejects $lineBreak in $field", () => { expect(send).not.toHaveBeenCalled(); });`;',
    'object.it.each([1])("rejects $lineBreak in $field", () => { expect(send).not.toHaveBeenCalled(); });',
    'it.each([1])("rejects $lineBreak in $field", () => { /* expect(send).not.toHaveBeenCalled(); */ });',
    'it.each([1])("rejects $lineBreak in $field", () => { const prose = "expect(send).not.toHaveBeenCalled()"; });',
    'it.each([1])("rejects $lineBreak in $field", () => { /* expect(send).not.toHaveBeenCalled(); */ expect(other).toBe(1); });',
  ]) {
    assert.notDeepEqual(validateJavascriptFixture(source), [], source);
  }
});

test("JavaScript helper evidence requires an executed direct call and its actual named assertion", () => {
  const evidence = {
    assertion: "openCompose(",
    assertionHelper: "openCompose",
    helperAssertion: "expect(send).not.toHaveBeenCalled()",
  };
  const source = (call, assertion) => `
    async function openCompose() { ${assertion} return {}; }
    it.each([{ field: "To" }])("rejects $lineBreak in $field", async ({ field }) => {
      ${call}
    });
  `;
  assert.deepEqual(
    validateJavascriptFixture(
      source(
        "const { seed } = await openCompose(field);",
        "expect(send).not.toHaveBeenCalled();",
      ),
      evidence,
    ),
    [],
  );
  for (const call of [
    "// openCompose(field);",
    'const prose = "openCompose(field)";',
    "const unused = openCompose;",
    "if (false) { await openCompose(field); }",
    "const unused = () => openCompose(field);",
    "const future = openCompose(field);",
    "expect(other).toBe(1); if (false) { await openCompose(field); }",
  ]) {
    assert.notDeepEqual(
      validateJavascriptFixture(
        source(call, "expect(send).not.toHaveBeenCalled();"),
        evidence,
      ),
      [],
      call,
    );
  }
  for (const assertion of [
    "",
    "/* expect(send).not.toHaveBeenCalled(); */ expect(other).toBe(1);",
    'const prose = "expect(send).not.toHaveBeenCalled()"; expect(other).toBe(1);',
    "if (false) { expect(send).not.toHaveBeenCalled(); }",
    "return {}; expect(send).not.toHaveBeenCalled();",
  ]) {
    assert.notDeepEqual(
      validateJavascriptFixture(
        source("await openCompose(field);", assertion),
        evidence,
      ),
      [],
      assertion,
    );
  }
});

test("each evidence requires a statically nonempty literal table", () => {
  for (const table of [
    "[]",
    "[ /* no cases */ ]",
    "[,]",
    "[...unknownCases]",
    "[].flatMap((value) => [value])",
    "[1].flatMap((value) => [])",
    "[1].filter(() => false)",
    "unknownCases",
  ]) {
    assert.notDeepEqual(
      validateJavascriptFixture(
        `it.each(${table})("rejects $lineBreak in $field", () => { expect(send).not.toHaveBeenCalled(); });`,
      ),
      [],
      table,
    );
  }
  assert.deepEqual(
    validateJavascriptFixture(
      'it.each(["a", "b"],)("rejects $lineBreak in $field", () => { expect(send).not.toHaveBeenCalled(); });',
    ),
    [],
  );
});

test("dead or unused callback assertions cannot satisfy parameterized evidence", () => {
  for (const body of [
    "if (false) { expect(send).not.toHaveBeenCalled(); }",
    "const unused = () => { expect(send).not.toHaveBeenCalled(); };",
    "function unused() { expect(send).not.toHaveBeenCalled(); }",
    "return; expect(send).not.toHaveBeenCalled();",
    'throw new Error("stop"); expect(send).not.toHaveBeenCalled();',
    "expect(other).toBe(1); if (false) { expect(send).not.toHaveBeenCalled(); }",
    "expect(other).toBe(1); const unused = () => { expect(send).not.toHaveBeenCalled(); };",
    "expect(other).toBe(1); return; expect(send).not.toHaveBeenCalled();",
  ]) {
    assert.notDeepEqual(
      validateJavascriptFixture(
        `it.each([1])("rejects $lineBreak in $field", () => { ${body} });`,
      ),
      [],
      body,
    );
  }
});

test("ordinary and each declarations accept function callbacks with executable assertions", () => {
  for (const factory of ["it", "it.each([1])", "test.each([1])"]) {
    for (const callback of ["function (value)", "async function (value)"]) {
      assert.deepEqual(
        validateJavascriptFixture(
          `${factory}("rejects $lineBreak in $field", ${callback} { expect(send).not.toHaveBeenCalled(); });`,
        ),
        [],
        `${factory} ${callback}`,
      );
    }
  }
});

test("an argument arrow or unselected branch cannot masquerade as the test callback", () => {
  for (const source of [
    'function wrapper(ignored) { return () => {}; } it.each([1])("rejects $lineBreak in $field", wrapper(() => { expect(send).not.toHaveBeenCalled(); }));',
    'it.each([1])("rejects $lineBreak in $field", false ? (() => { expect(send).not.toHaveBeenCalled(); }) : (() => {}));',
  ])
    assert.notDeepEqual(validateJavascriptFixture(source), [], source);
});

test("a discarded function body cannot supply assertions for a different callback", () => {
  for (const suffix of [
    "&& function(){}",
    "|| function(){}",
    "? function(){} : function(){}",
  ])
    assert.notDeepEqual(
      validateJavascriptFixture(
        `it.each([1])("rejects $lineBreak in $field", function(){ expect(send).not.toHaveBeenCalled(); } ${suffix});`,
      ),
      [],
      suffix,
    );
});
