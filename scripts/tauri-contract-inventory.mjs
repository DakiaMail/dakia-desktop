#!/usr/bin/env node

import { readFile, readdir } from "node:fs/promises";
import { dirname, extname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SCRIPT_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CONTRACT_PATH = "apps/desktop/testdata/tauri-contract-origins.json";
const FRONTEND_ROOT = "apps/desktop/src";
const RUST_ROOT = "apps/desktop/src-tauri/src";

function isIdentifierStart(character) {
  return /[A-Za-z_$]/.test(character);
}

function isIdentifierPart(character) {
  return /[A-Za-z0-9_$]/.test(character);
}

function decodeQuoted(value, quote) {
  let result = "";
  for (let index = 1; index < value.length - 1; index += 1) {
    const character = value[index];
    if (character !== "\\") {
      result += character;
      continue;
    }
    index += 1;
    const escaped = value[index];
    if (escaped === undefined) break;
    const simple = {
      b: "\b",
      f: "\f",
      n: "\n",
      r: "\r",
      t: "\t",
      v: "\v",
      0: "\0",
    };
    if (simple[escaped] !== undefined) {
      result += simple[escaped];
    } else if (escaped === "x") {
      const hex = value.slice(index + 1, index + 3);
      result += String.fromCharCode(Number.parseInt(hex, 16));
      index += 2;
    } else if (escaped === "u" && value[index + 1] === "{") {
      const end = value.indexOf("}", index + 2);
      if (end === -1)
        throw new Error(`Unterminated Unicode escape in ${value}`);
      result += String.fromCodePoint(
        Number.parseInt(value.slice(index + 2, end), 16),
      );
      index = end;
    } else if (escaped === "u") {
      const hex = value.slice(index + 1, index + 5);
      result += String.fromCharCode(Number.parseInt(hex, 16));
      index += 4;
    } else {
      // Command and event names are ASCII, but retaining an escaped quote or
      // slash makes the lexer usable for adjacent parser tests too.
      result += escaped;
    }
  }
  return result;
}

/**
 * A deliberately small lexer for the call shapes used at the IPC boundary.
 * It skips comments and string bodies so a commented-out or quoted invoke can
 * never become a contract entry. This is not a TypeScript or Rust parser.
 */
export function tokenize(source, language = "typescript") {
  const tokens = [];
  let index = 0;
  while (index < source.length) {
    const character = source[index];
    const next = source[index + 1];
    if (/\s/.test(character)) {
      index += 1;
      continue;
    }
    if (character === "/" && next === "/") {
      index = source.indexOf("\n", index + 2);
      if (index === -1) break;
      continue;
    }
    if (character === "/" && next === "*") {
      const end = source.indexOf("*/", index + 2);
      if (end === -1) throw new Error("Unterminated block comment");
      index = end + 2;
      continue;
    }
    if (
      language === "rust" &&
      character === "'" &&
      /[A-Za-z_]/.test(next ?? "")
    ) {
      let lifetimeEnd = index + 1;
      while (isIdentifierPart(source[lifetimeEnd] ?? "")) lifetimeEnd += 1;
      if (source[lifetimeEnd] !== "'") {
        tokens.push({ type: "symbol", value: "'" });
        index += 1;
        continue;
      }
    }
    if (character === "'" || character === '"' || character === "`") {
      const quote = character;
      const start = index;
      let hasInterpolation = false;
      index += 1;
      while (index < source.length) {
        if (source[index] === "\\") {
          index += 2;
          continue;
        }
        if (
          quote === "`" &&
          source[index] === "$" &&
          source[index + 1] === "{"
        ) {
          hasInterpolation = true;
        }
        if (source[index] === quote) {
          index += 1;
          break;
        }
        index += 1;
      }
      if (source[index - 1] !== quote) {
        throw new Error(`Unterminated ${quote} string at offset ${start}`);
      }
      const raw = source.slice(start, index);
      tokens.push({
        type: quote === "`" && hasInterpolation ? "template" : "string",
        value: quote === "`" ? raw.slice(1, -1) : decodeQuoted(raw, quote),
      });
      continue;
    }
    // Rust raw strings, including r#"event"# and br"event".
    const rawString = source.slice(index).match(/^(?:br|rb|r)(#+)?"/);
    if (rawString) {
      const hashes = rawString[1] ?? "";
      const delimiter = `"${hashes}`;
      const start = index + rawString[0].length;
      const end = source.indexOf(delimiter, start);
      if (end === -1) throw new Error("Unterminated Rust raw string");
      tokens.push({ type: "string", value: source.slice(start, end) });
      index = end + delimiter.length;
      continue;
    }
    if (isIdentifierStart(character)) {
      const start = index;
      index += 1;
      while (index < source.length && isIdentifierPart(source[index]))
        index += 1;
      tokens.push({ type: "identifier", value: source.slice(start, index) });
      continue;
    }
    tokens.push({ type: "symbol", value: character });
    index += 1;
  }
  return tokens;
}

function skipTypeArguments(tokens, index) {
  if (tokens[index]?.value !== "<") return index;
  let depth = 0;
  for (; index < tokens.length; index += 1) {
    if (tokens[index].value === "<") depth += 1;
    if (tokens[index].value === ">") {
      depth -= 1;
      if (depth === 0) return index + 1;
    }
  }
  throw new Error("Unterminated generic type arguments");
}

function literalCalls(source, names, argumentIndex) {
  const tokens = tokenize(source);
  const values = [];
  for (let index = 0; index < tokens.length; index += 1) {
    if (tokens[index].type !== "identifier" || !names.has(tokens[index].value))
      continue;
    let cursor = skipTypeArguments(tokens, index + 1);
    if (tokens[cursor]?.value !== "(") continue;
    for (let argument = 0; argument < argumentIndex; argument += 1) {
      cursor += 1;
      let depth = 0;
      while (cursor < tokens.length) {
        if (tokens[cursor].value === "(") depth += 1;
        if (tokens[cursor].value === ")") depth -= 1;
        if (tokens[cursor].value === "," && depth === 0) break;
        cursor += 1;
      }
      if (tokens[cursor]?.value !== ",") {
        throw new Error(
          `${tokens[index].value} is missing argument ${argumentIndex + 1}`,
        );
      }
    }
    const event = tokens[cursor + 1];
    if (event?.type !== "string") {
      throw new Error(
        `${tokens[index].value} must use a literal contract name`,
      );
    }
    values.push(event.value);
  }
  return values;
}

function skipBalanced(tokens, index, opening, closing) {
  if (tokens[index]?.value !== opening) {
    throw new Error(`Expected ${opening}`);
  }
  let depth = 0;
  for (; index < tokens.length; index += 1) {
    if (tokens[index].value === opening) depth += 1;
    if (tokens[index].value === closing) {
      depth -= 1;
      if (depth === 0) return index + 1;
    }
  }
  throw new Error(`Unterminated ${opening}${closing} expression`);
}

function skipObjectValue(tokens, index) {
  const closings = new Map([
    ["(", ")"],
    ["[", "]"],
    ["{", "}"],
  ]);
  const stack = [];
  for (; index < tokens.length; index += 1) {
    const value = tokens[index].value;
    if (closings.has(value)) {
      stack.push(closings.get(value));
      continue;
    }
    if (stack.length && value === stack.at(-1)) {
      stack.pop();
      continue;
    }
    if (!stack.length && [",", "}"].includes(value)) return index;
  }
  throw new Error("Unterminated frontend invoke request object");
}

function isSpread(tokens, index) {
  return (
    tokens[index]?.value === "." &&
    tokens[index + 1]?.value === "." &&
    tokens[index + 2]?.value === "."
  );
}

function frontendRequestObjectKeys(tokens, index, command) {
  if (tokens[index]?.value !== "{") throw new Error("Expected request object");
  const keys = [];
  let cursor = index + 1;
  while (tokens[cursor]?.value !== "}") {
    if (!tokens[cursor]) {
      throw new Error(
        `invoke(${JSON.stringify(command)}) has an unterminated request object`,
      );
    }
    if (isSpread(tokens, cursor)) {
      throw new Error(
        `invoke(${JSON.stringify(command)}) request object must not use spread properties`,
      );
    }
    const property = tokens[cursor];
    if (!["identifier", "string"].includes(property.type)) {
      throw new Error(
        `invoke(${JSON.stringify(command)}) request object must use static top-level keys`,
      );
    }
    const key = property.value;
    cursor += 1;
    if (tokens[cursor]?.value === ":") {
      cursor = skipObjectValue(tokens, cursor + 1);
    } else if (property.type !== "identifier") {
      throw new Error(
        `invoke(${JSON.stringify(command)}) request object has an invalid property`,
      );
    }
    keys.push(key);
    if (tokens[cursor]?.value === ",") {
      cursor += 1;
      continue;
    }
    if (tokens[cursor]?.value !== "}") {
      throw new Error(
        `invoke(${JSON.stringify(command)}) request object must use static top-level keys`,
      );
    }
  }
  return { keys: sortedSet(keys), end: cursor + 1 };
}

/**
 * Returns every frontend invocation with its statically knowable top-level
 * request keys. Tauri deserializes command parameters directly from this
 * object, so an opaque argument or spread would make the inventory unsound.
 */
export function extractFrontendInvokeRequests(source) {
  const tokens = tokenize(source);
  const requests = [];
  for (let index = 0; index < tokens.length; index += 1) {
    if (tokens[index].type !== "identifier" || tokens[index].value !== "invoke")
      continue;
    let cursor = skipTypeArguments(tokens, index + 1);
    if (tokens[cursor]?.value !== "(") continue;
    const command = tokens[cursor + 1];
    if (command?.type !== "string") {
      throw new Error("invoke must use a literal contract name");
    }
    cursor += 2;
    if (tokens[cursor]?.value === ")") {
      requests.push({ command: command.value, keys: [] });
      continue;
    }
    if (tokens[cursor]?.value !== ",") {
      throw new Error(`invoke(${JSON.stringify(command.value)}) is malformed`);
    }
    cursor += 1;
    if (tokens[cursor]?.value !== "{") {
      throw new Error(
        `invoke(${JSON.stringify(command.value)}) request arguments must be a static object literal`,
      );
    }
    const request = frontendRequestObjectKeys(tokens, cursor, command.value);
    if (![",", ")"].includes(tokens[request.end]?.value)) {
      throw new Error(`invoke(${JSON.stringify(command.value)}) is malformed`);
    }
    requests.push({ command: command.value, keys: request.keys });
  }
  return requests;
}

export function extractFrontendInvokes(source) {
  return extractFrontendInvokeRequests(source).map(({ command }) => command);
}

export function extractFrontendListeners(source) {
  return literalCalls(source, new Set(["listen"]), 0);
}

export function extractFrontendEmitters(source) {
  return literalCalls(source, new Set(["emitTo"]), 1);
}

export function extractGenerateHandlerCommands(source) {
  const tokens = tokenize(source, "rust");
  const commands = [];
  for (let index = 0; index < tokens.length; index += 1) {
    if (tokens[index].value !== "generate_handler") continue;
    if (tokens[index + 1]?.value !== "!" || tokens[index + 2]?.value !== "[")
      continue;
    let cursor = index + 3;
    while (tokens[cursor]?.value !== "]") {
      const command = tokens[cursor];
      if (command?.type !== "identifier") {
        throw new Error("generate_handler! must contain bare command names");
      }
      commands.push(command.value);
      cursor += 1;
      if (tokens[cursor]?.value === ",") cursor += 1;
      else if (tokens[cursor]?.value !== "]") {
        throw new Error("generate_handler! commands must be comma separated");
      }
    }
  }
  if (commands.length === 0)
    throw new Error("No generate_handler! commands found");
  return commands;
}

const TAURI_INJECTED_PARAMETER_TYPES = new Set([
  "AppHandle",
  "State",
  "Window",
  "WebviewWindow",
  "Webview",
  "Request",
]);

function commandAttributeEnd(tokens, index) {
  if (
    tokens[index]?.value !== "#" ||
    tokens[index + 1]?.value !== "[" ||
    tokens[index + 2]?.value !== "tauri" ||
    tokens[index + 3]?.value !== ":" ||
    tokens[index + 4]?.value !== ":" ||
    tokens[index + 5]?.value !== "command"
  ) {
    return null;
  }
  return skipBalanced(tokens, index + 1, "[", "]");
}

function rustParameterIsInjected(typeTokens) {
  const typeNames = typeTokens
    .filter((token) => token.type === "identifier")
    .map((token) => token.value);
  // Channel is a Tauri IPC type, but unlike AppHandle/State/etc. it is sent by
  // the frontend and therefore remains part of the request contract.
  if (typeNames.includes("Channel")) return false;
  return typeNames.some((name) => TAURI_INJECTED_PARAMETER_TYPES.has(name));
}

function rustCommandParameters(tokens, start, end, command) {
  const parameters = [];
  let cursor = start + 1;
  while (cursor < end) {
    if (tokens[cursor]?.value === ",") {
      cursor += 1;
      continue;
    }
    if (tokens[cursor]?.value === "mut") cursor += 1;
    const name = tokens[cursor];
    if (name?.type !== "identifier" || tokens[cursor + 1]?.value !== ":") {
      throw new Error(
        `#[tauri::command] ${command} must use named function parameters`,
      );
    }
    cursor += 2;
    const typeStart = cursor;
    let genericDepth = 0;
    let delimiterDepth = 0;
    while (cursor < end) {
      const value = tokens[cursor].value;
      if (value === "<") genericDepth += 1;
      else if (value === ">") genericDepth -= 1;
      else if (["(", "[", "{"].includes(value)) delimiterDepth += 1;
      else if ([")", "]", "}"].includes(value)) delimiterDepth -= 1;
      if (value === "," && genericDepth === 0 && delimiterDepth === 0) break;
      cursor += 1;
    }
    if (!rustParameterIsInjected(tokens.slice(typeStart, cursor))) {
      parameters.push(snakeToCamel(name.value));
    }
  }
  return parameters;
}

function snakeToCamel(name) {
  return name.replace(/_([a-zA-Z0-9])/g, (_, character) =>
    character.toUpperCase(),
  );
}

/** Extract Tauri command request parameter names, expressed as JS keys. */
export function extractRustCommandParameters(source) {
  const tokens = tokenize(source, "rust");
  const commands = {};
  for (let index = 0; index < tokens.length; index += 1) {
    let cursor = commandAttributeEnd(tokens, index);
    if (cursor === null) continue;
    // Attributes such as #[allow(...)] may sit between command and fn.
    while (tokens[cursor]?.value === "#") {
      if (tokens[cursor + 1]?.value !== "[") break;
      cursor = skipBalanced(tokens, cursor + 1, "[", "]");
    }
    while (["pub", "async", "unsafe", "extern"].includes(tokens[cursor]?.value)) {
      cursor += 1;
      if (tokens[cursor - 1]?.value === "extern" && tokens[cursor]?.type === "string")
        cursor += 1;
    }
    if (tokens[cursor]?.value !== "fn" || tokens[cursor + 1]?.type !== "identifier") {
      throw new Error("#[tauri::command] must be followed by a function");
    }
    const command = tokens[cursor + 1].value;
    cursor += 2;
    while (tokens[cursor]?.value !== "(" && cursor < tokens.length) cursor += 1;
    const end = skipBalanced(tokens, cursor, "(", ")") - 1;
    if (Object.hasOwn(commands, command)) {
      throw new Error(`Duplicate #[tauri::command] function ${command}`);
    }
    commands[command] = rustCommandParameters(tokens, cursor, end, command);
  }
  return commands;
}

function rustStringConstants(tokens) {
  const constants = new Map();
  for (let index = 0; index < tokens.length; index += 1) {
    if (
      tokens[index].value !== "const" ||
      tokens[index + 1]?.type !== "identifier"
    )
      continue;
    const name = tokens[index + 1].value;
    let cursor = index + 2;
    while (cursor < tokens.length && tokens[cursor].value !== ";") {
      if (
        tokens[cursor].value === "=" &&
        tokens[cursor + 1]?.type === "string"
      ) {
        constants.set(name, tokens[cursor + 1].value);
        break;
      }
      cursor += 1;
    }
  }
  return constants;
}

export function extractRustNativeEmits(source) {
  const tokens = tokenize(source, "rust");
  const constants = rustStringConstants(tokens);
  const events = [];
  for (let index = 0; index < tokens.length; index += 1) {
    if (!new Set(["emit", "emit_all", "emit_to"]).has(tokens[index].value))
      continue;
    if (tokens[index + 1]?.value !== "(") continue;
    const event = tokens[index + 2];
    if (event?.type === "string") {
      events.push(event.value);
    } else if (event?.type === "identifier" && constants.has(event.value)) {
      events.push(constants.get(event.value));
    } else {
      throw new Error(
        `${tokens[index].value} must use a literal or const event name`,
      );
    }
  }
  return events;
}

async function filesBelow(root, predicate) {
  const files = [];
  async function visit(directory) {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const path = resolve(directory, entry.name);
      if (entry.isDirectory()) await visit(path);
      else if (predicate(path)) files.push(path);
    }
  }
  await visit(root);
  return files.sort();
}

function sortedSet(values) {
  return [...new Set(values)].sort();
}

async function namesFromFiles(files, extractor, marker) {
  const names = [];
  for (const file of files) {
    const source = await readFile(file, "utf8");
    // Avoid lexing unrelated TypeScript/TSX modules. Besides making the check
    // cheap, this limits the mechanical lexer to the API syntax it owns.
    if (!source.includes(marker)) continue;
    names.push(...extractor(source));
  }
  return sortedSet(names);
}

async function frontendInvokeRequestsFromFiles(files) {
  const requests = [];
  for (const file of files) {
    const source = await readFile(file, "utf8");
    if (!source.includes("invoke")) continue;
    requests.push(...extractFrontendInvokeRequests(source));
  }
  return requests;
}

async function rustCommandParametersFromFiles(files) {
  const commands = {};
  for (const file of files) {
    const source = await readFile(file, "utf8");
    if (!source.includes("tauri::command")) continue;
    for (const [name, parameters] of Object.entries(
      extractRustCommandParameters(source),
    )) {
      if (Object.hasOwn(commands, name)) {
        throw new Error(`Duplicate #[tauri::command] function ${name}`);
      }
      commands[name] = parameters;
    }
  }
  return commands;
}

export async function inventoryRepository(repositoryRoot = SCRIPT_ROOT) {
  const frontendDirectory = resolve(repositoryRoot, FRONTEND_ROOT);
  const rustDirectory = resolve(repositoryRoot, RUST_ROOT);
  const frontendFiles = await filesBelow(
    frontendDirectory,
    (path) =>
      [".ts", ".tsx"].includes(extname(path)) &&
      !/\.test\.tsx?$/.test(path) &&
      !relative(frontendDirectory, path).split("/").includes("test"),
  );
  const rustFiles = await filesBelow(
    rustDirectory,
    (path) => extname(path) === ".rs",
  );
  const handlerSource = await readFile(
    resolve(rustDirectory, "lib.rs"),
    "utf8",
  );
  const frontendInvokeRequests = await frontendInvokeRequestsFromFiles(
    frontendFiles,
  );
  return {
    frontendInvokes: sortedSet(
      frontendInvokeRequests.map(({ command }) => command),
    ),
    frontendInvokeRequests,
    frontendListeners: await namesFromFiles(
      frontendFiles,
      extractFrontendListeners,
      "listen",
    ),
    frontendEmitters: await namesFromFiles(
      frontendFiles,
      extractFrontendEmitters,
      "emitTo",
    ),
    rustHandlers: sortedSet(extractGenerateHandlerCommands(handlerSource)),
    rustCommandParameters: await rustCommandParametersFromFiles(rustFiles),
    rustNativeEmits: await namesFromFiles(
      rustFiles,
      extractRustNativeEmits,
      "emit",
    ),
  };
}

function difference(left, right) {
  const rightNames = new Set(right);
  return left.filter((name) => !rightNames.has(name));
}

function duplicates(values) {
  return values.filter((value, index) => values.indexOf(value) !== index);
}

function expectExact(errors, label, actual, expected) {
  const missing = difference(expected, actual);
  const unexpected = difference(actual, expected);
  if (missing.length) errors.push(`${label} missing: ${missing.join(", ")}`);
  if (unexpected.length)
    errors.push(`${label} unexpected: ${unexpected.join(", ")}`);
}

export function validateInventory(inventory, contract) {
  const errors = [];
  if (contract?.schemaVersion !== 1)
    errors.push("contract schemaVersion must be 1");
  const demoOnlyInvokes = contract?.demoOnlyInvokes;
  const eventOrigins = contract?.eventOrigins;
  if (!Array.isArray(demoOnlyInvokes))
    errors.push("demoOnlyInvokes must be an array");
  if (
    !eventOrigins ||
    typeof eventOrigins !== "object" ||
    Array.isArray(eventOrigins)
  ) {
    errors.push("eventOrigins must be an object");
  }
  if (!Array.isArray(inventory.frontendInvokeRequests)) {
    errors.push("frontendInvokeRequests must be an array");
  }
  if (
    !inventory.rustCommandParameters ||
    typeof inventory.rustCommandParameters !== "object" ||
    Array.isArray(inventory.rustCommandParameters)
  ) {
    errors.push("rustCommandParameters must be an object");
  }
  if (errors.length) return errors;

  if (duplicates(demoOnlyInvokes).length)
    errors.push("demoOnlyInvokes contains duplicates");
  for (const [event, origin] of Object.entries(eventOrigins)) {
    if (!["native", "frontend", "demo-only"].includes(origin)) {
      errors.push(`event ${event} has invalid origin ${String(origin)}`);
    }
  }
  const frontendCommands = inventory.frontendInvokes.filter(
    (name) => !demoOnlyInvokes.includes(name),
  );
  expectExact(
    errors,
    "frontend invokes vs Rust handlers",
    frontendCommands,
    inventory.rustHandlers,
  );
  for (const command of inventory.rustHandlers) {
    if (!Object.hasOwn(inventory.rustCommandParameters, command)) {
      errors.push(`registered Rust command ${command} has no parsed signature`);
    }
  }
  for (const request of inventory.frontendInvokeRequests) {
    if (demoOnlyInvokes.includes(request.command)) continue;
    const parameters = inventory.rustCommandParameters[request.command];
    if (!parameters) continue;
    if (!Array.isArray(request.keys)) {
      errors.push(
        `frontend invoke request for ${request.command} must contain a keys array`,
      );
      continue;
    }
    expectExact(
      errors,
      `frontend invoke request keys for ${request.command}`,
      request.keys,
      parameters,
    );
  }
  expectExact(
    errors,
    "declared demo-only invokes",
    demoOnlyInvokes,
    inventory.frontendInvokes.filter((name) => demoOnlyInvokes.includes(name)),
  );

  const knownEvents = Object.keys(eventOrigins).sort();
  expectExact(
    errors,
    "frontend event classifications",
    inventory.frontendListeners,
    knownEvents,
  );
  const nativeEvents = knownEvents.filter(
    (event) => eventOrigins[event] === "native",
  );
  const frontendEvents = knownEvents.filter(
    (event) => eventOrigins[event] === "frontend",
  );
  const demoEvents = knownEvents.filter(
    (event) => eventOrigins[event] === "demo-only",
  );
  expectExact(
    errors,
    "native emitted events vs native listeners",
    inventory.rustNativeEmits,
    nativeEvents,
  );
  expectExact(
    errors,
    "frontend emitted events vs frontend listeners",
    inventory.frontendEmitters,
    frontendEvents,
  );
  for (const event of demoEvents) {
    if (
      !inventory.frontendListeners.includes(event) &&
      !inventory.frontendEmitters.includes(event)
    ) {
      errors.push(
        `demo-only event ${event} is not present in frontend production source`,
      );
    }
  }
  return errors;
}

export function assertValidInventory(inventory, contract) {
  const errors = validateInventory(inventory, contract);
  if (errors.length)
    throw new Error(
      `Tauri contract drift:\n${errors.map((error) => `- ${error}`).join("\n")}`,
    );
}

export async function readContract(repositoryRoot = SCRIPT_ROOT) {
  return JSON.parse(
    await readFile(resolve(repositoryRoot, CONTRACT_PATH), "utf8"),
  );
}

async function main() {
  const repositoryRoot = resolve(process.argv[2] ?? SCRIPT_ROOT);
  const [inventory, contract] = await Promise.all([
    inventoryRepository(repositoryRoot),
    readContract(repositoryRoot),
  ]);
  assertValidInventory(inventory, contract);
  process.stdout.write(
    `Verified Tauri contracts: ${inventory.rustHandlers.length} commands, ${inventory.rustNativeEmits.length} native events, ${inventory.frontendEmitters.length} frontend events.\n`,
  );
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`);
    process.exitCode = 1;
  });
}
