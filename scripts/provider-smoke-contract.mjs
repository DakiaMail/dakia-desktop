#!/usr/bin/env node

const SECRET_KEYS = new Set([
  "password",
  "appPassword",
]);

function fail(message) {
  throw new Error(message);
}

function nonEmptyString(value, name) {
  if (typeof value !== "string" || value.trim().length === 0) {
    fail(`${name} must be a non-empty string`);
  }
  return value;
}

function endpoint(value, name) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    fail(`${name} must be an object`);
  }
  nonEmptyString(value.host, `${name}.host`);
  if (!Number.isInteger(value.port) || value.port < 1 || value.port > 65535) {
    fail(`${name}.port must be an integer from 1 to 65535`);
  }
  if (!["tls", "starttls"].includes(value.security)) {
    fail(`${name}.security must be tls or starttls`);
  }
}

/**
 * Validate the secret-backed provider smoke contract before the Rust harness
 * opens its single read-neutral IMAP session and SMTP auth/QUIT probe. OAuth
 * values are deliberately rejected: this lane must not guess a refresh or
 * token acquisition flow.
 */
export function validateProviderSmokeContract(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    fail("provider smoke configuration must be a JSON object");
  }
  if (value.version !== 1)
    fail("provider smoke configuration version must be 1");
  nonEmptyString(value.provider, "provider");
  nonEmptyString(value.accountEmail, "accountEmail");
  endpoint(value.imap, "imap");
  endpoint(value.smtp, "smtp");
  if (
    !value.credentials ||
    typeof value.credentials !== "object" ||
    Array.isArray(value.credentials)
  ) {
    fail("credentials must be an object");
  }
  const credentialKeys = Object.keys(value.credentials);
  if (credentialKeys.length !== 1 || !SECRET_KEYS.has(credentialKeys[0])) {
    fail("credentials must contain exactly one password or appPassword field");
  }
  nonEmptyString(
    value.credentials[credentialKeys[0]],
    `credentials.${credentialKeys[0]}`,
  );
  return {
    version: value.version,
    provider: value.provider,
    accountEmail: value.accountEmail,
    imap: {
      host: value.imap.host,
      port: value.imap.port,
      security: value.imap.security,
    },
    smtp: {
      host: value.smtp.host,
      port: value.smtp.port,
      security: value.smtp.security,
    },
    credentialKind: credentialKeys[0],
  };
}

function main() {
  const raw = process.env.PROVIDER_SMOKE_CONFIG;
  if (!raw) fail("PROVIDER_SMOKE_CONFIG is required");
  let value;
  try {
    value = JSON.parse(raw);
  } catch {
    fail("PROVIDER_SMOKE_CONFIG must be valid JSON");
  }
  const contract = validateProviderSmokeContract(value);
  // Do not log account, endpoint, provider, or the secret-backed JSON. The
  // workflow runs the separately compiled Rust harness immediately after this
  // validation, so this is not presented as a live-provider pass by itself.
  void contract;
  console.log("Provider smoke configuration validated for the live harness.");
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    main();
  } catch (error) {
    // Deliberately never print the secret-backed JSON input.
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
