-- This database intentionally has no request/event table. The only per-report
-- value is a salted, one-way hash used for short-lived duplicate suppression.
CREATE TABLE IF NOT EXISTS analytics_receipts (
  token_hash TEXT PRIMARY KEY NOT NULL,
  expires_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS analytics_receipts_expires_at
  ON analytics_receipts(expires_at);

-- Aggregate-only product counters. `providers` is a canonical comma-separated
-- set (or `none`), never an account address, provider host, or account ID.
CREATE TABLE IF NOT EXISTS monthly_usage_aggregates (
  month TEXT NOT NULL,
  app_version TEXT NOT NULL,
  os TEXT NOT NULL,
  os_version TEXT NOT NULL,
  arch TEXT NOT NULL,
  providers TEXT NOT NULL,
  active_installs INTEGER NOT NULL DEFAULT 0 CHECK (active_installs >= 0),
  PRIMARY KEY (month, app_version, os, os_version, arch, providers)
);
