# Anonymous usage statistics

Dakia's optional usage statistics are a narrowly scoped, first-party product
measurement service. They are not required for the app to work and are off
until the person using Dakia chooses **Share anonymous statistics**.

## What the desktop app sends

The app posts at most once each calendar week to
`https://analytics.dakiamail.com/v1/usage`. The exact JSON is available,
collapsed by default, in **Settings → Privacy** before consent is given.

The schema contains only:

- schema number;
- the reporting month;
- a random identifier that is regenerated every calendar month;
- Dakia's major/minor version;
- the operating-system family and CPU architecture; and
- the distinct, broad categories of enabled mail providers (`gmail`,
  `outlook`, `fastmail`, `icloud`, `yahoo`, or `other`).

It never contains an email address, display name, account identifier, custom
mail-server host, mailbox, message data, credential, locale, IP address, user
agent, or a permanent device identifier. Disabled accounts are excluded. The sender does not run before the
user opts in. Turning it off cancels future reports immediately and removes
the local rotating identifier and the local last-report marker.

The rotating identifier is deliberately displayed in the preview: it lets the
collector count one opted-in installation once in a reporting period, but it
cannot link that installation to a following month.

## Collector and retention

`analytics.dakiamail.com` is a Dakia-owned Cloudflare Worker. It accepts only
this fixed, small schema and rejects unexpected fields. The Worker neither
logs nor persists the request IP, request headers, or raw JSON body. It hashes
the rotating identifier before storing it, uses that hash only to deduplicate
the current reporting period, and stores aggregate counters separately.

The D1 database uses the `eu` jurisdiction. Deduplication rows are deleted
after 35 days. Aggregate monthly counters are retained for 24 months. The
private reporting query suppresses category/version rows with fewer than 20
reports. There is no public analytics endpoint or dashboard, and the
production service has no third-party analytics or crash-reporting SDK.

Cloudflare is the infrastructure processor for the HTTPS request and database.
Its edge necessarily receives the connection to serve it. The Worker derives
a salted one-way key from Cloudflare's source-address header solely for a
short-lived rate-limit counter. Neither the address nor that key is written to
D1 or application logs. Rate-limit counters are approximate abuse controls,
not usage measurements. The in-app disclosure links to this public document.

The resulting active-installation numbers are estimates: only opted-in apps
report, each monthly token can be counted once, and rate limiting or network
failures can lower the count. Release download counts are a separate server-side
metric and are not inferred from this client telemetry.

## Provisioning and deployment

The implementation lives in `services/analytics`. It is deliberately separate
from the desktop app and release-download bucket.

1. Authenticate Wrangler to Dakia's Cloudflare account.
2. Create the database with `npx wrangler d1 create dakia-usage-analytics --jurisdiction=eu`.
3. Put the resulting database ID in `services/analytics/wrangler.jsonc` (or a
   non-committed deployment-specific config).
4. Apply `services/analytics/migrations/0001_usage_analytics.sql` with
   `npx wrangler d1 migrations apply dakia-usage-analytics --remote`.
5. Deploy only after reviewing the dry run:
   `npx wrangler deploy --config services/analytics/wrangler.jsonc --dry-run`,
   followed by `npx wrangler deploy --config services/analytics/wrangler.jsonc`.
6. Bind the custom domain `analytics.dakiamail.com` to the Worker and verify
   its POST-only route with a test payload. Do not enable Logpush or Worker
   request-body logging for this service.

Private aggregate reporting is available through
`services/analytics/queries/monthly-summary.sql`; it reads only aggregate rows
and applies the minimum-bucket threshold. Run it with the service-local
`npm run report` command after authenticating Wrangler.

Provisioning and deploying are external production changes. They require the
account owner's explicit approval and are intentionally not performed by the
repository build.
