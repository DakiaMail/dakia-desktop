# Provider setup

Dakia auto-detects common personal domains and also accepts custom IMAP/SMTP hosts.

| Provider | IMAP | SMTP | Authentication |
| --- | --- | --- | --- |
| Gmail / Google Workspace | `imap.gmail.com:993` TLS | `smtp.gmail.com:465` TLS | Google app password |
| Outlook.com / Hotmail | `outlook.office365.com:993` TLS | `smtp-mail.outlook.com:587` STARTTLS | App password; enable IMAP in Outlook.com settings |
| Microsoft 365 / Exchange Online | — | — | OAuth 2.0 only; unavailable until a Microsoft Entra client is registered |
| Fastmail | `imap.fastmail.com:993` TLS | `smtp.fastmail.com:465` TLS | App password |
| Zoho Mail | `imap.zoho.com:993` TLS | `smtp.zoho.com:465` TLS | App password |
| Migadu | `imap.migadu.com:993` TLS | `smtp.migadu.com:465` TLS | Mailbox password |
| iCloud Mail | `imap.mail.me.com:993` TLS | `smtp.mail.me.com:587` STARTTLS | App-specific password |
| Yahoo Mail | `imap.mail.yahoo.com:993` TLS | `smtp.mail.yahoo.com:465` TLS | App password |
| Other | User supplied | User supplied | Password / app password |

## Gmail and Google Workspace

New Gmail and Google Workspace accounts connect with an app password, not a
Google OAuth sign-in. Enable 2-Step Verification, then create an app password
for Dakia by following Google's [app-password guide](https://support.google.com/accounts/answer/185833?hl=en).
Use that generated password in Dakia. Do not use your personal Gmail or regular Google Account password in Dakia.

New Google OAuth sign-in is temporarily disabled because Google requires an
expensive CASA certification even for this local desktop app. It can be
restored after that certification is completed.

Google Workspace administrators and Google Advanced Protection can disable app
passwords. Dakia cannot currently connect to those accounts when an app
password is unavailable.

Existing Dakia accounts that already use Google OAuth continue to work while
their saved token remains valid. If that authentication fails, update the
account with a Google app password in Settings to convert it to password
authentication.

Provider tenants and custom domains can still use the preset by selecting it manually. For nonstandard servers, choose “Other IMAP / SMTP” and enter both hosts.
