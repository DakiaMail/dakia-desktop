# Architecture

Dakia uses one Rust engine from two user interfaces.

```text
React + Mantine desktop UI ─┐
                            ├─ dakia-core ─ IMAP / SMTP providers
Rust CLI (`dakia`) ─────────┘      │
                                  ├─ SQLite + FTS5 local index
                                  ├─ encrypted local credential vault
                                  └─ OpenAI-compatible / Ollama / llama.cpp AI
```

The Tauri webview never connects directly to a mail or AI server. Tauri commands deserialize typed inputs, select an account from the local database, and delegate to `dakia-core`. Passwords, OAuth refresh tokens, and AI API keys are encrypted with AES-256-GCM in a dedicated SQLite table. The random vault key is stored beside the database in `vault.key`, with owner-only permissions on Unix systems. This favors prompt-free access and launch reliability over protection from an attacker who can copy both files.

## Local data

`directories::ProjectDirs` selects the platform application-data directory. The database stores account metadata and indexed message text. Attachments are not persisted in the first release. SQLite FTS5 indexes subject, sender, recipient, and plain-text body fields.

## Mail transport

- IMAP connections use TLS 1.2+ with the Mozilla WebPKI root set.
- Password accounts authenticate with IMAP `LOGIN`; OAuth accounts use `XOAUTH2`.
- SMTP uses `lettre` with implicit TLS or mandatory STARTTLS and selects XOAUTH2 for OAuth accounts.
- Archive and spam operations prefer IMAP `MOVE`, with a `COPY` + `\Deleted` fallback.

### Near-real-time inbox delivery

While the desktop process is running, Tauri supervises one INBOX watcher per
enabled account. A watcher uses IMAP `IDLE` when advertised, renews the session
every 25 minutes, and otherwise polls with a jittered one-minute interval.
Disconnects retry with bounded exponential backoff.

New UIDs are persisted from headers first so the UI and native notification do
not wait for attachments, MIME parsing, DKIM work, or classification. Full
message hydration follows through an atomic per-message claim, making restart,
manual-sync, and notification-click races idempotent. Mailbox sync state stores
UIDVALIDITY and the highest committed UID; a changed UIDVALIDITY rebuilds only
that mailbox and resets its notification baseline.

Closing the main window hides Dakia to its menu-bar item. Explicit Quit stops
the watchers. Launch at login is optional and disabled by default. This is a
local system: it does not deliver notifications while Dakia is quit or the
computer is asleep, and no credentials are sent to a Dakia service.

## AI boundary

AI is optional. The desktop and CLI accept:

- an OpenAI-compatible `/v1/chat/completions` endpoint and model;
- an Ollama endpoint, defaulting to local `qwen2.5:1.5b`;
- a local llama.cpp-compatible executable and GGUF model path.

Prompts tell the model not to invent facts or commitments. Translation preserves names, dates, URLs, and formatting. No AI request is made until the user invokes an AI action.

## Internationalization

Visible desktop copy is stored in `apps/desktop/src/locales/en.ts` and accessed with `react-i18next`. New locales add a sibling resource and locale picker without rewriting components.
