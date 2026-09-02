import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import worker, {
  parseUsageReport,
  recordUsage,
  type AnalyticsEnv,
  type UsageReport,
} from "../src/index";

const now = new Date("2026-08-26T12:00:00.000Z");
const payload: UsageReport = {
  schema: 1,
  month: "2026-08",
  rotating_install_token: "0123456789abcdef0123456789abcdef",
  app_version: "0.4.0",
  os: "macos",
  os_version: "15.6",
  arch: "arm64",
  providers: ["fastmail", "gmail"],
};

describe("usage analytics collector schema", () => {
  it("accepts only the documented exact payload shape", () => {
    expect(parseUsageReport(JSON.stringify(payload), now)).toEqual(payload);
    expect(
      parseUsageReport(
        JSON.stringify({ ...payload, email: "private@example.test" }),
        now,
      ),
    ).toBeNull();
    expect(
      parseUsageReport(
        JSON.stringify({ ...payload, providers: ["gmail", "fastmail"] }),
        now,
      ),
    ).toBeNull();
    expect(
      parseUsageReport(JSON.stringify({ ...payload, month: "2026-06" }), now),
    ).toBeNull();
    expect(
      parseUsageReport(
        JSON.stringify({ ...payload, rotating_install_token: "not-a-token" }),
        now,
      ),
    ).toBeNull();
  });

  it("uses one salted receipt insert and an aggregate increment guarded by that insert", async () => {
    const db = new RecordingDatabase();
    await recordUsage(environment(db), payload, 1_000);

    expect(db.batches).toHaveLength(1);
    expect(db.batches[0]).toHaveLength(2);
    expect(db.batches[0][0].sql).toContain(
      "INSERT OR IGNORE INTO analytics_receipts",
    );
    expect(db.batches[0][0].values[0]).toMatch(/^[a-f0-9]{64}$/);
    expect(db.batches[0][0].values[0]).not.toContain(
      payload.rotating_install_token,
    );
    expect(db.batches[0][0].values[1]).toBe(35 * 24 * 60 * 60 * 1_000 + 1_000);
    expect(db.batches[0][1].sql).toContain("WHERE changes() = 1");
    expect(db.batches[0][1].values).toEqual([
      "2026-08",
      "0.4.0",
      "macos",
      "15.6",
      "arm64",
      "fastmail,gmail",
    ]);
  });

  it("counts duplicate tokens once and distinct tokens twice in real SQLite", async () => {
    const db = new SqliteD1Database();
    const env = environment(db as unknown as RecordingDatabase);
    const otherPayload = {
      ...payload,
      rotating_install_token: "fedcba9876543210fedcba9876543210",
    };

    await Promise.all([
      recordUsage(env, payload, 1_000),
      recordUsage(env, payload, 1_001),
    ]);
    await recordUsage(env, otherPayload, 1_002);

    expect(db.scalar("SELECT COUNT(*) FROM analytics_receipts")).toBe(2);
    expect(
      db.scalar("SELECT SUM(active_installs) FROM monthly_usage_aggregates"),
    ).toBe(2);
  });
});

describe("usage analytics collector HTTP boundary", () => {
  it("allows the bundled macOS Tauri origin and returns no body or cacheable data", async () => {
    const response = await worker.fetch(
      request(
        "https://analytics.dakiamail.com/v1/usage",
        "tauri://localhost",
        payload,
      ),
      environment(new RecordingDatabase()),
    );

    expect(response.status).toBe(204);
    expect(response.headers.get("access-control-allow-origin")).toBe(
      "tauri://localhost",
    );
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(response.headers.get("content-security-policy")).toContain(
      "default-src 'none'",
    );
  });

  it("rejects untrusted browser origins before reading or storing a report", async () => {
    const db = new RecordingDatabase();
    const response = await worker.fetch(
      request(
        "https://analytics.dakiamail.com/v1/usage",
        "https://example.test",
        payload,
      ),
      environment(db),
    );

    expect(response.status).toBe(403);
    expect(db.batches).toHaveLength(0);
    expect(response.headers.get("access-control-allow-origin")).toBeNull();
  });

  it("rejects origin-less requests before reading or storing a report", async () => {
    const db = new RecordingDatabase();
    const response = await worker.fetch(
      new Request("https://analytics.dakiamail.com/v1/usage", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "cf-connecting-ip": "192.0.2.10",
        },
        body: JSON.stringify(payload),
      }),
      environment(db),
    );

    expect(response.status).toBe(403);
    expect(db.batches).toHaveLength(0);
  });

  it("rejects the HTTPS localhost origin used by non-macOS Tauri targets", async () => {
    const db = new RecordingDatabase();
    const response = await worker.fetch(
      request(
        "https://analytics.dakiamail.com/v1/usage",
        "https://tauri.localhost",
        payload,
      ),
      environment(db),
    );

    expect(response.status).toBe(403);
    expect(db.batches).toHaveLength(0);
  });

  it("rejects a wrong content type and an oversized request before parsing", async () => {
    const db = new RecordingDatabase();
    const wrongType = await worker.fetch(
      new Request("https://analytics.dakiamail.com/v1/usage", {
        method: "POST",
        headers: {
          origin: "tauri://localhost",
          "content-type": "text/plain",
          "cf-connecting-ip": "192.0.2.10",
        },
        body: JSON.stringify(payload),
      }),
      environment(db),
    );
    const oversized = await worker.fetch(
      new Request("https://analytics.dakiamail.com/v1/usage", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "content-length": "2049",
          origin: "tauri://localhost",
          "cf-connecting-ip": "192.0.2.10",
        },
        body: "x",
      }),
      environment(db),
    );
    const streamedOversized = await worker.fetch(
      new Request("https://analytics.dakiamail.com/v1/usage", {
        method: "POST",
        headers: {
          origin: "tauri://localhost",
          "content-type": "application/json",
          "cf-connecting-ip": "192.0.2.10",
        },
        body: "x".repeat(2_049),
      }),
      environment(db),
    );

    expect(wrongType.status).toBe(415);
    expect(oversized.status).toBe(413);
    expect(streamedOversized.status).toBe(413);
    expect(db.batches).toHaveLength(0);
  });

  it("rate limits by an opaque salted network key without touching D1", async () => {
    const db = new RecordingDatabase();
    const rateLimiter = new RecordingRateLimiter(false);
    const response = await worker.fetch(
      request(
        "https://analytics.dakiamail.com/v1/usage",
        "tauri://localhost",
        payload,
      ),
      environment(db, rateLimiter),
    );

    expect(response.status).toBe(429);
    expect(rateLimiter.keys[0]).toMatch(/^[a-f0-9]{64}$/);
    expect(rateLimiter.keys[0]).not.toContain("192.0.2.10");
    expect(db.batches).toHaveLength(0);
  });

  it("answers CORS preflight only for the configured origin", async () => {
    const allowed = await worker.fetch(
      new Request("https://analytics.dakiamail.com/v1/usage", {
        method: "OPTIONS",
        headers: { origin: "tauri://localhost" },
      }),
      environment(new RecordingDatabase()),
    );
    const denied = await worker.fetch(
      new Request("https://analytics.dakiamail.com/v1/usage", {
        method: "OPTIONS",
        headers: { origin: "https://example.test" },
      }),
      environment(new RecordingDatabase()),
    );

    expect(allowed.status).toBe(204);
    expect(allowed.headers.get("access-control-allow-methods")).toBe(
      "POST, OPTIONS",
    );
    expect(denied.status).toBe(403);
  });

  it("schedules removal of expired receipts and aggregate data older than 24 months", async () => {
    const db = new RecordingDatabase();
    const waiting: Promise<unknown>[] = [];
    await worker.scheduled({} as ScheduledController, environment(db), {
      waitUntil: (promise: Promise<unknown>): void => {
        waiting.push(promise);
      },
    } as unknown as ExecutionContext);
    await Promise.all(waiting);

    expect(db.batches).toHaveLength(1);
    expect(db.batches[0][0].sql).toContain("DELETE FROM analytics_receipts");
    expect(db.batches[0][0].values[0]).toEqual(expect.any(Number));
    expect(db.batches[0][1].sql).toContain(
      "DELETE FROM monthly_usage_aggregates",
    );
    expect(db.batches[0][1].values).toEqual([monthTwentyThreeMonthsAgo()]);
  });
});

class RecordingDatabase {
  batches: Array<RecordedStatement[]> = [];
  prepared: RecordedStatement[] = [];

  prepare(sql: string) {
    const statement: RecordedStatement = { sql, values: [] };
    this.prepared.push(statement);
    return {
      bind: (...values: unknown[]) => {
        statement.values = values;
        return {
          ...statement,
          run: async () => ({ success: true }),
        };
      },
    };
  }

  async batch(statements: RecordedStatement[]) {
    this.batches.push(statements);
    return [];
  }
}

class SqliteD1Database {
  private readonly sqlite = new DatabaseSync(":memory:");

  constructor() {
    this.sqlite.exec(
      readFileSync(
        new URL("../migrations/0001_usage_analytics.sql", import.meta.url),
        "utf8",
      ),
    );
  }

  prepare(sql: string) {
    return {
      bind: (...values: unknown[]) => ({ sql, values }),
    };
  }

  async batch(statements: RecordedStatement[]) {
    this.sqlite.exec("BEGIN IMMEDIATE");
    try {
      for (const statement of statements) {
        this.sqlite
          .prepare(statement.sql)
          .run(...(statement.values as Parameters<ReturnType<DatabaseSync["prepare"]>["run"]>));
      }
      this.sqlite.exec("COMMIT");
    } catch (error) {
      this.sqlite.exec("ROLLBACK");
      throw error;
    }
    return [];
  }

  scalar(sql: string): unknown {
    return Object.values(this.sqlite.prepare(sql).get() ?? {})[0];
  }
}

class RecordingRateLimiter {
  keys: string[] = [];

  constructor(private readonly success = true) {}

  async limit({ key }: { key: string }) {
    this.keys.push(key);
    return { success: this.success };
  }
}

type RecordedStatement = { sql: string; values: unknown[] };

function environment(
  db: RecordingDatabase,
  rateLimiter = new RecordingRateLimiter(),
): AnalyticsEnv {
  return {
    ANALYTICS_DB: db as unknown as D1Database,
    INGEST_RATE_LIMITER: rateLimiter as unknown as RateLimit,
    ALLOWED_ORIGIN: "tauri://localhost",
    TOKEN_HASH_SALT: "test-only-secret-not-for-deployment",
  };
}

function request(url: string, origin: string, body: unknown): Request {
  return new Request(url, {
    method: "POST",
    headers: {
      origin,
      "content-type": "application/json",
      "cf-connecting-ip": "192.0.2.10",
    },
    body: JSON.stringify(body),
  });
}

function monthTwentyThreeMonthsAgo(): string {
  const now = new Date();
  const date = new Date(
    Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - 23, 1),
  );
  return `${date.getUTCFullYear()}-${String(date.getUTCMonth() + 1).padStart(2, "0")}`;
}
