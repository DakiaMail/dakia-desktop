# AI privacy test evidence

This report records the bounded optional AI privacy tests added on 2026-10-21.
The shipped desktop build remains AI-disabled. These tests cover the core
service and the Composer boundary for an explicitly enabled future build.

## Inputs

- Source revision: `796c297dbc4ddde75a5bfc771a629d7524bc984a`
- Node: `v22.23.2`
- Rust: `1.89.0 (29483883e 2025-08-04)`
- `crates/dakia-core/tests/ai_privacy.rs`: SHA-256 `8f01a74bc1df0149bab2523ceecf1cc2e91b96b55b1630499b734e3acad897f0`
- `apps/desktop/src/ComposeApp.ai-privacy.test.tsx`: SHA-256 `e8dbe1d5e77994cd4fafdb49cf68f3db7c17623308d45c6abdca4a3d0360fa3a`

All endpoints and mail data in these tests are fictional and loopback-only.
The local model test uses a temporary executable and model file. No external
AI endpoint, mailbox, credential, or private message is used.

## Results

`cargo test -p dakia-core --test ai_privacy` passed 2/2 tests:

- Ollama drafts use only the explicit fictional model, system/user messages,
  and selected context. The captured wire request has no authorization header
  and contains no supplied API key.
- Local command output containing a tool call remains exact inert text. The
  fixture executable validates `-m`, the owned model path, `--prompt`, the
  fictional message content, and empty stdin before emitting the hostile text.

`npx vitest run apps/desktop/src/ComposeApp.ai-privacy.test.tsx` passed 1/1:

- A hostile draft response is inserted as escaped text. There is no
  `tool_call` element, send request, compose close, seed discard, Outbox
  notification, or native error action.

## Limits

These are bounded contract tests. They do not enable the shipped AI commands,
contact a real provider, prove provider-side retention, or replace native
WebKit and sandbox acceptance. The local command test proves the service
invokes the configured executable with the expected argument boundary and
does not interpret its output. It does not certify an arbitrary executable's
own behavior. Existing native IPC tests remain the evidence that shipped
AI-disabled commands reject forged requests before provider or process access.
