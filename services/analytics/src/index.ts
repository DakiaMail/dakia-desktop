const MAX_BODY_BYTES = 2_048;
const RECEIPT_RETENTION_MS = 35 * 24 * 60 * 60 * 1_000;

const PROVIDERS = [
  "fastmail",
  "gmail",
  "icloud",
  "other",
  "outlook",
  "yahoo",
] as const;
const ARCHITECTURES = ["arm64", "other", "x64"] as const;

type Provider = (typeof PROVIDERS)[number];
type Architecture = (typeof ARCHITECTURES)[number];

export type UsageReport = {
  schema: 1;
  month: string;
  rotating_install_token: string;
  app_version: string;
  os: "macos";
  os_version: string;
  arch: Architecture;
  providers: Provider[];
};

type AggregateDimensions = Omit<
  UsageReport,
  "schema" | "rotating_install_token" | "providers"
> & {
  providers: string;
};

// `Env` is generated from wrangler.jsonc. The salt is intentionally absent
// from config because it is a Worker secret, so it is added only at this
// boundary rather than hand-writing the Worker binding interface.
export type AnalyticsEnv = Env & { TOKEN_HASH_SALT: string };

export default {
  async fetch(request: Request, env: AnalyticsEnv): Promise<Response> {
    const cors = corsHeaders(request, env);
    if (request.method === "OPTIONS") {
      return cors
        ? new Response(null, { status: 204, headers: cors })
        : error(403, "forbidden");
    }

    if (!corsAllowed(request, env)) return error(403, "forbidden");
    if (
      request.method !== "POST" ||
      new URL(request.url).pathname !== "/v1/usage"
    ) {
      return error(404, "not_found", cors);
    }
    if (!isJson(request)) return error(415, "unsupported_media_type", cors);
    if (!env.TOKEN_HASH_SALT)
      return error(503, "temporarily_unavailable", cors);

    const networkAddress = request.headers.get("cf-connecting-ip");
    if (!networkAddress) return error(403, "forbidden", cors);
    const rateLimitKey = await hashToken(
      env.TOKEN_HASH_SALT,
      `network:${networkAddress}`,
    );
    const rateLimit = await env.INGEST_RATE_LIMITER.limit({
      key: rateLimitKey,
    });
    if (!rateLimit.success) return error(429, "rate_limited", cors);

    const body = await readSmallBody(request);
    if (body === null) return error(413, "payload_too_large", cors);
    if (body === undefined) return error(400, "invalid_payload", cors);

    const report = parseUsageReport(body);
    if (!report) return error(400, "invalid_payload", cors);
    try {
      await recordUsage(env, report, Date.now());
      return new Response(null, {
        status: 204,
        headers: responseHeaders(cors),
      });
    } catch {
      // Do not log errors here: Worker logs can contain request metadata.
      return error(503, "temporarily_unavailable", cors);
    }
  },

  async scheduled(
    _controller: ScheduledController,
    env: AnalyticsEnv,
    ctx: ExecutionContext,
  ) {
    const now = new Date();
    const deleteReceiptsBefore = now.getTime();
    const retainFromMonth = monthOffset(now, -23);
    ctx.waitUntil(
      env.ANALYTICS_DB.batch([
        env.ANALYTICS_DB.prepare(
          "DELETE FROM analytics_receipts WHERE expires_at <= ?",
        ).bind(deleteReceiptsBefore),
        env.ANALYTICS_DB.prepare(
          "DELETE FROM monthly_usage_aggregates WHERE month < ?",
        ).bind(retainFromMonth),
      ]),
    );
  },
};

export function parseUsageReport(
  body: string,
  now = new Date(),
): UsageReport | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return null;
  }
  if (!isRecord(parsed)) return null;

  const keys = Object.keys(parsed).sort();
  const expected = [
    "app_version",
    "arch",
    "month",
    "os",
    "os_version",
    "providers",
    "rotating_install_token",
    "schema",
  ];
  if (
    keys.length !== expected.length ||
    keys.some((key, index) => key !== expected[index])
  ) {
    return null;
  }

  if (
    parsed.schema !== 1 ||
    !isCurrentOrPreviousMonth(parsed.month, now) ||
    typeof parsed.rotating_install_token !== "string" ||
    !/^[a-f0-9]{32}$/.test(parsed.rotating_install_token) ||
    !isAppVersion(parsed.app_version) ||
    parsed.os !== "macos" ||
    !isOsVersion(parsed.os_version) ||
    !isArchitecture(parsed.arch) ||
    !isProviders(parsed.providers)
  ) {
    return null;
  }

  return {
    schema: 1,
    month: parsed.month,
    rotating_install_token: parsed.rotating_install_token,
    app_version: parsed.app_version,
    os: "macos",
    os_version: parsed.os_version,
    arch: parsed.arch,
    providers: parsed.providers,
  };
}

export async function recordUsage(
  env: AnalyticsEnv,
  report: UsageReport,
  now: number,
): Promise<void> {
  const tokenHash = await hashToken(
    env.TOKEN_HASH_SALT,
    report.rotating_install_token,
  );
  const dimensions: AggregateDimensions = {
    month: report.month,
    app_version: report.app_version,
    os: report.os,
    os_version: report.os_version,
    arch: report.arch,
    providers: report.providers.join(",") || "none",
  };

  // D1 batch executes atomically. The second statement only increments the
  // aggregate when the preceding INSERT OR IGNORE created a new receipt.
  await env.ANALYTICS_DB.batch([
    env.ANALYTICS_DB.prepare(
      "INSERT OR IGNORE INTO analytics_receipts (token_hash, expires_at) VALUES (?, ?)",
    ).bind(tokenHash, now + RECEIPT_RETENTION_MS),
    env.ANALYTICS_DB.prepare(
      `INSERT INTO monthly_usage_aggregates
          (month, app_version, os, os_version, arch, providers, active_installs)
         SELECT ?, ?, ?, ?, ?, ?, 1
         WHERE changes() = 1
         ON CONFLICT(month, app_version, os, os_version, arch, providers)
         DO UPDATE SET active_installs = active_installs + 1`,
    ).bind(
      dimensions.month,
      dimensions.app_version,
      dimensions.os,
      dimensions.os_version,
      dimensions.arch,
      dimensions.providers,
    ),
  ]);
}

async function hashToken(salt: string, token: string): Promise<string> {
  const bytes = new TextEncoder().encode(`${salt}:${token}`);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function corsAllowed(request: Request, env: AnalyticsEnv): boolean {
  const origin = request.headers.get("origin");
  return origin === env.ALLOWED_ORIGIN;
}

function corsHeaders(request: Request, env: AnalyticsEnv): Headers | undefined {
  const origin = request.headers.get("origin");
  if (!origin || origin !== env.ALLOWED_ORIGIN) return undefined;
  return new Headers({
    "access-control-allow-origin": origin,
    "access-control-allow-methods": "POST, OPTIONS",
    "access-control-allow-headers": "content-type",
    "access-control-max-age": "0",
    vary: "Origin",
  });
}

function responseHeaders(cors?: Headers): Headers {
  const headers = new Headers(cors);
  headers.set("cache-control", "no-store");
  headers.set(
    "content-security-policy",
    "default-src 'none'; base-uri 'none'; frame-ancestors 'none'",
  );
  headers.set("referrer-policy", "no-referrer");
  headers.set("x-content-type-options", "nosniff");
  return headers;
}

function error(status: number, code: string, cors?: Headers): Response {
  const headers = responseHeaders(cors);
  headers.set("content-type", "application/json; charset=utf-8");
  return new Response(JSON.stringify({ error: code }), { status, headers });
}

function isJson(request: Request): boolean {
  return (
    request.headers
      .get("content-type")
      ?.split(";", 1)[0]
      .trim()
      .toLowerCase() === "application/json"
  );
}

async function readSmallBody(
  request: Request,
): Promise<string | null | undefined> {
  const length = request.headers.get("content-length");
  if (length && (!/^\d+$/.test(length) || Number(length) > MAX_BODY_BYTES))
    return null;
  if (!request.body) return undefined;

  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let received = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      received += value.byteLength;
      if (received > MAX_BODY_BYTES) {
        await reader.cancel();
        return null;
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  if (received === 0) return undefined;
  const body = new Uint8Array(received);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder().decode(body);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isCurrentOrPreviousMonth(value: unknown, now: Date): value is string {
  if (typeof value !== "string" || !/^\d{4}-(0[1-9]|1[0-2])$/.test(value))
    return false;
  const current = monthOffset(now, 0);
  const previous = monthOffset(now, -1);
  return value === current || value === previous;
}

function monthOffset(now: Date, offset: number): string {
  const date = new Date(
    Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + offset, 1),
  );
  return `${date.getUTCFullYear()}-${String(date.getUTCMonth() + 1).padStart(2, "0")}`;
}

function isAppVersion(value: unknown): value is string {
  return (
    typeof value === "string" &&
    (value === "unknown" ||
      /^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(value))
  );
}

function isOsVersion(value: unknown): value is string {
  return (
    typeof value === "string" &&
    (value === "unknown" || /^\d{1,2}(?:\.\d{1,2}){0,2}$/.test(value))
  );
}

function isArchitecture(value: unknown): value is Architecture {
  return (
    typeof value === "string" &&
    (ARCHITECTURES as readonly string[]).includes(value)
  );
}

function isProviders(value: unknown): value is Provider[] {
  return (
    Array.isArray(value) &&
    value.length <= PROVIDERS.length &&
    value.every(
      (provider) =>
        typeof provider === "string" &&
        (PROVIDERS as readonly string[]).includes(provider),
    ) &&
    value.every((provider, index) => index === 0 || value[index - 1] < provider)
  );
}
