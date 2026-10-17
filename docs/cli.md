# CLI

The `dakia` CLI operates on the same local profile as the Dakia desktop app:
accounts, encrypted credential vault, mail catalogue, and search index are
shared. Accounts can be configured in either the desktop app or the terminal.
Use `dakia account list` to obtain an account ID for mail commands.

On macOS the CLI follows the current Desktop profile at
`~/Library/Application Support/dev.dakia.mail`. Use `DAKIA_DATA_DIR` only to
intentionally select a different Desktop profile. The catalogue does not retain
message bodies or attachments indefinitely; opening a message fetches it from
the provider or Dakia's bounded content cache. Notes, agent memory, and local
drafts remain available independently of that cache.

```bash
# Set up an account. Dakia asks for the password without displaying it.
# For Gmail, use an app password.
dakia account add --email you@gmail.com --display-name 'Your name' --provider gmail
dakia account test --account ACCOUNT_UUID

# Inspect the desktop accounts, then refresh mail from every enabled account.
dakia --json account list
dakia sync

# Limit a sync to one account or one mailbox.
dakia sync --account ACCOUNT_UUID --folder 'Projects' --limit 250

# Watch the INBOX of one enabled account until Ctrl-C.
dakia watch --account ACCOUNT_UUID

# Search the local catalogue. Use --remote to search the authoritative server
# too; newly discovered hits are saved as header metadata for the desktop app.
dakia search 'release signing' --unread
dakia --json search 'sender@example.test' --remote --limit 50

# Open a message by the stable Dakia ID returned by search.
dakia show MESSAGE_ID
dakia message list --account ACCOUNT_UUID --unread --limit 50
dakia message flag MESSAGE_ID --read
dakia message flag MESSAGE_ID --system-flag answered
dakia message flag MESSAGE_ID --clear-system-flag flagged
dakia message move MESSAGE_ID --destination 'Projects'
dakia message copy MESSAGE_ID --destination 'Follow up'
dakia folder list --account ACCOUNT_UUID
dakia attachment list MESSAGE_ID
dakia attachment download MESSAGE_ID ATTACHMENT_ID --output ./report.pdf

# Apply the same mailbox actions used by the desktop app.
dakia archive MESSAGE_ID
dakia spam MESSAGE_ID
dakia trash MESSAGE_ID --yes
dakia delete MESSAGE_ID --yes

# Send an email. Omit --body to read it from stdin.
printf 'The release matrix is attached.\n' | \
  dakia send --account ACCOUNT_UUID --to team@example.com --subject 'Release matrix' \
  --attach ./release-matrix.pdf

# Optional AI actions.
DAKIA_AI_PROVIDER=ollama DAKIA_AI_MODEL=qwen2.5:1.5b \
  dakia ai summarize MESSAGE_ID MESSAGE_ID
```

For scripts, account setup supports `--password-stdin` and `--password-env NAME`.
Passwords are never accepted as command arguments. Custom accounts accept
`--imap-host`, `--smtp-host`, ports, usernames, and security options. Use
`dakia account presets` to inspect built-in providers and `--help` on a command
to inspect its full options.

`sync` without `--folder` refreshes the account's standard mailboxes. With
`--folder`, it refreshes only that mailbox. `watch` follows INBOX only and
continues until you stop it with Ctrl-C.

## Folders and raw messages

Use folder commands for remote IMAP folders. Deleting a folder requires an
explicit confirmation.

```bash
dakia folder create --account ACCOUNT_UUID --name 'Projects/2026'
dakia folder rename --account ACCOUNT_UUID --name 'Projects/2026' \
  --new-name 'Projects/Current'
dakia folder delete --account ACCOUNT_UUID --name 'Projects/Current' --yes

# Import a complete RFC 822 message or export a stored message as .eml.
dakia message import --account ACCOUNT_UUID --mailbox INBOX --input ./message.eml
dakia message export --message-id MESSAGE_ID --output ./message.eml
```

## Drafts and replies

Saving a draft does not send mail. Drafts are saved locally first and provider
Drafts synchronization is tracked separately.

```bash
dakia draft create --account ACCOUNT_UUID --to john@example.com \
  --subject 'Today' --body 'I cannot make it today.'
dakia draft reply --message-id MESSAGE_ID --body 'Thank you for the update.'
dakia draft list --account ACCOUNT_UUID
dakia draft show --draft-id DRAFT_UUID
dakia draft edit --draft-id DRAFT_UUID --revision 1 \
  --body 'I cannot make it today after all.'
dakia forward --message-id MESSAGE_ID --body 'For your reference.' --attach-original
dakia draft delete --draft-id DRAFT_UUID --yes
dakia draft send --draft-id DRAFT_UUID
```

If delivery becomes uncertain, Dakia prevents another send of that draft.
Check the provider's Sent folder before creating a new reply.

## Thread notes and memory

Your notes and automatically learned agent memory are separate local documents.
Read the current revision before editing so another client's changes cannot
be overwritten silently. Use revision `0` for an empty document.

```bash
dakia --json context get --account ACCOUNT_UUID --message-id MESSAGE_ID
dakia context set --account ACCOUNT_UUID --message-id MESSAGE_ID \
  --revision 0 --text 'The insurer is waiting for the repair estimate.'
dakia context history --account ACCOUNT_UUID --message-id MESSAGE_ID

# Keep using the permanent conversation ID after source mail is removed.
dakia context get --account ACCOUNT_UUID --conversation-id CONVERSATION_UUID
dakia context restore --account ACCOUNT_UUID --conversation-id CONVERSATION_UUID \
  --revision-id HISTORY_REVISION_UUID --revision 1
```

Pass `--kind agent-memory` to inspect or edit the agent's document. Thread
context uses permanent local conversation identities so notes survive normal
mail moves and threading changes. Uncertain associations are marked for review.
The desktop reader shows both documents and their history.

## Email agent

The macOS installation includes the agent runtime. A separate Node.js or Pi
installation is not needed.

```bash
dakia provider connect chatgpt
dakia provider connect claude
dakia provider list
dakia provider models chatgpt
dakia provider default openai-codex/gpt-6.1-sol
dakia agent
dakia agent exec "What is the latest status of my home insurance case?"
dakia agent exec "Draft a reply to John telling him I cannot make it today."
dakia agent sessions list
dakia agent memory show
dakia agent memory history
dakia agent memory learning off
```

The agent can read, search, draft, update memory, and organize mail. Sending and
permanent deletion require approval of the exact action. Piped and JSON runs
return an approval-required result instead of waiting for a terminal prompt.

Agent instructions live in `agent/DAKIA.md` inside the selected Dakia profile.
Global preferences live in `agent/MEMORY.md`; conversation facts live in thread
memory. Files from the current working directory do not become instructions.
Email content and learned memory cannot grant permission to act.

Only relevant messages and context are sent to your selected AI provider.
Thread notes are not uploaded to your email provider or added to sent email.
OpenCode Go is deferred.

Subscription connections open a login flow and ask you to choose a default
model. API connections accept a key from stdin or a named environment variable:

```bash
dakia provider connect openai --api-key-env OPENAI_API_KEY
dakia provider connect custom --id local-mail --api openai-completions \
  --base-url http://127.0.0.1:1234/v1 --model mail-model \
  --api-key-env LOCAL_MAIL_API_KEY
dakia provider connect custom --id my-anthropic --api anthropic-messages \
  --base-url https://api.example.test --model mail-model \
  --api-key-env MAIL_API_KEY
```

Custom connections support `openai-completions`, `openai-responses`, and
`anthropic-messages`. OpenAI-compatible base URLs normally include `/v1`.
For Anthropic-compatible connections, omit the final `/v1`; Dakia adds
`/v1/messages`. Remote connections require HTTPS. Local HTTP endpoints are
allowed. Endpoint settings and keys are stored together in the encrypted vault.

`trash` and `delete` require `--yes`: Trash is reversible at the provider;
`delete` expunges the message permanently. For non-interactive account setup,
use `--password-stdin` or `--password-env NAME`. Otherwise, credentials
configured by the desktop app are used.
