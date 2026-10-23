import { writeFileSync } from "node:fs";
import { CATALOGUE_PATH, loadCatalogue, reportCatalogue, validateCatalogue } from "./mail-safety-catalogue.mjs";

function parseArgs(argv) {
  const options = { lane: "all", output: null };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--lane" || argument === "--output") {
      const value = argv[index + 1];
      if (!value) throw new Error(`${argument} requires a value`);
      options[argument.slice(2)] = value;
      index += 1;
    } else throw new Error(`Unknown argument: ${argument}`);
  }
  return options;
}

try {
  const options = parseArgs(process.argv.slice(2));
  const catalogue = loadCatalogue(CATALOGUE_PATH);
  const errors = validateCatalogue(catalogue, { root: process.cwd() });
  if (errors.length > 0) throw new Error(errors.join("\n"));
  if (options.lane !== "all" && !catalogue.lanes.includes(options.lane)) {
    throw new Error(`Unknown mail safety lane: ${options.lane}`);
  }
  const report = reportCatalogue(catalogue, { root: process.cwd(), lane: options.lane });
  if (options.output) writeFileSync(options.output, report);
  else process.stdout.write(report);
} catch (error) {
  process.stderr.write(`Mail safety catalogue report failed: ${error.message}\n`);
  process.exitCode = 1;
}
