import { resolve } from "node:path";
import { CATALOGUE_PATH, loadCatalogue, validateCatalogue } from "./mail-safety-catalogue.mjs";

function parseArgs(argv) {
  if (argv.length === 0) return CATALOGUE_PATH;
  if (argv.length === 2 && argv[0] === "--catalogue") return argv[1];
  throw new Error("Usage: node scripts/validate-mail-safety-catalogue.mjs [--catalogue PATH]");
}

try {
  const path = parseArgs(process.argv.slice(2));
  const errors = validateCatalogue(loadCatalogue(path), { root: process.cwd() });
  if (errors.length > 0) throw new Error(errors.join("\n"));
  process.stdout.write(`Mail safety catalogue valid: ${resolve(path)}\n`);
} catch (error) {
  process.stderr.write(`Mail safety catalogue invalid: ${error.message}\n`);
  process.exitCode = 1;
}
