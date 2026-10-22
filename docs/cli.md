# CLI

The `dakia` CLI operates on the same local profile as the Dakia desktop app:
accounts, encrypted credential vault, mail catalogue, and search index are
shared. Account setup remains in the desktop app; `dakia account list` is
read-only and exists to obtain an account ID for mail commands.

On macOS the CLI follows the current Desktop profile at
`~/Library/Application Support/dev.dakia.mail`. Use `DAKIA_DATA_DIR` only to
intentionally select a different Desktop profile. The catalogue does not retain
message bodies or attachments; opening a message fetches it from the provider.

```bash
# Inspect the desktop accounts, then refresh mail from every enabled account.
dakia --json account list
dakia sync

# Search the local catalogue. Use --remote to search the authoritative server
# too; newly discovered hits are saved as header metadata for the desktop app.
dakia search 'release signing' --unread
dakia --json search 'sender@example.test' --remote --limit 50

# Open a message by the stable Dakia ID returned by search.
dakia show MESSAGE_ID --expected-uid-validity UIDVALIDITY
dakia attachment list MESSAGE_ID --expected-uid-validity UIDVALIDITY

# Get each message's UIDVALIDITY receipt before a command fetches, changes,
# downloads, or sends its content outside Dakia. Keep values in message-ID order.
dakia --json search 'release signing'
dakia attachment download MESSAGE_ID ATTACHMENT_ID --expected-uid-validity UIDVALIDITY --output ./report.pdf

# Apply the same mailbox actions used by the desktop app. Dakia refuses to act
# if the mailbox generation changed since the search result was captured.
dakia archive MESSAGE_ID --expected-uid-validity UIDVALIDITY
dakia spam MESSAGE_ID --expected-uid-validity UIDVALIDITY
dakia trash MESSAGE_ID --expected-uid-validity UIDVALIDITY --yes
dakia delete MESSAGE_ID --expected-uid-validity UIDVALIDITY --yes

# Send an email. Omit --body to read it from stdin.
printf 'The release matrix is attached.\n' | \
  dakia send --account ACCOUNT_UUID --to team@example.com --subject 'Release matrix' \
  --attach ./release-matrix.pdf

# Optional AI actions.
DAKIA_AI_PROVIDER=ollama DAKIA_AI_MODEL=qwen2.5:1.5b \
  dakia ai summarize MESSAGE_ID MESSAGE_ID \
  --expected-uid-validity UIDVALIDITY --expected-uid-validity UIDVALIDITY
```

`trash` and `delete` require `--yes`: Trash is reversible at the provider;
`delete` expunges the message permanently. For non-interactive credential
injection, set `DAKIA_PASSWORD_<ACCOUNT_UUID>` (hyphens become underscores and
letters are uppercase). Otherwise, credentials configured by the desktop app
are used.

`--expected-uid-validity` is a receipt, not a mailbox name or message UID. It
comes from the `mailbox_uid_validity` field in `dakia --json search`. Dakia
requires it for reads, mailbox actions, attachment downloads, and AI summaries
so a recycled IMAP UID cannot affect a different email. For commands with more
than one `MESSAGE_ID`, repeat it once per message in the same order. If Dakia
says a message changed, run the search again and decide whether to repeat the
action.

The CLI never retries `send` automatically. It does not yet persist a send
attempt journal across separate CLI invocations. If a send reports an uncertain
result, check the provider's Sent mailbox before manually sending again.
