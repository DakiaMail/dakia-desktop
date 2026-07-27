# Provider setup

Dakia auto-detects common personal domains and also accepts custom IMAP/SMTP hosts.

| Provider | IMAP | SMTP | Authentication |
| --- | --- | --- | --- |
| Gmail / Google Workspace | `imap.gmail.com:993` TLS | `smtp.gmail.com:465` TLS | OAuth 2.0 (or an app password while Google verification is pending) |
| Outlook.com / Hotmail | `outlook.office365.com:993` TLS | `smtp-mail.outlook.com:587` STARTTLS | App password; enable IMAP in Outlook.com settings |
| Microsoft 365 / Exchange Online | — | — | OAuth 2.0 only; unavailable until a Microsoft Entra client is registered |
| Fastmail | `imap.fastmail.com:993` TLS | `smtp.fastmail.com:465` TLS | App password |
| Zoho Mail | `imap.zoho.com:993` TLS | `smtp.zoho.com:465` TLS | App password |
| Migadu | `imap.migadu.com:993` TLS | `smtp.migadu.com:465` TLS | Mailbox password |
| iCloud Mail | `imap.mail.me.com:993` TLS | `smtp.mail.me.com:587` STARTTLS | App-specific password |
| Yahoo Mail | `imap.mail.yahoo.com:993` TLS | `smtp.mail.yahoo.com:465` TLS | App password |
| Other | User supplied | User supplied | Password / app password |

## OAuth client registration

Google Desktop OAuth builds must provide the generated client secret at compile
time. Keep it in the ignored `.env` file for `npm run dev`, and inject it
through the supervised local release-build environment for release builds:

```bash
export DAKIA_GOOGLE_CLIENT_SECRET='…'
npm run build
```

The registered Google desktop client ID is the Gmail default; it can be
overridden with `DAKIA_GOOGLE_CLIENT_ID`. Dakia binds an ephemeral `127.0.0.1`
port, validates OAuth state, and uses PKCE S256. Google still requires the
desktop client's generated secret during code exchange and refresh, even though
an installed app cannot treat that value as confidential. Do not commit the
secret. OAuth tokens and the client secret used to refresh them are stored in
Dakia's encrypted local credential vault. Add `DAKIA_MICROSOFT_CLIENT_ID` back
when the Microsoft Entra registration is complete and Outlook OAuth is
re-enabled.

Until Google verifies Dakia's restricted Gmail scope, use OAuth only with an
authorized test account and expect Google's unverified-app limits to apply.

Provider tenants and custom domains can still use the preset by selecting it manually. For nonstandard servers, choose “Other IMAP / SMTP” and enter both hosts.
