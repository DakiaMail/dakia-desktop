# Offline translation model policy

Dakia detects language locally with Whatlang and translates locally with
Bergamot. Detection and translation availability are deliberately separate:

1. Whatlang considers its complete language set and returns a BCP-47 language
   code, English display name, and reliability result.
2. Dakia checks the pinned Mozilla model manifest for a released
   source-to-English model.
3. A reliable detected language without a pinned model is reported by its real
   name as unavailable. It is never relabeled as another supported language.

## Inclusion rule

A model is included when all of the following are true:

- Mozilla publishes a complete model, lexical shortlist, and vocabulary set;
- the records are enabled for Firefox Desktop Release;
- the direction is from the detected language to English;
- Whatlang 0.18 can identify that language distinctly; and
- the files have sizes and SHA-256 hashes that Dakia can pin and verify.

The generated `excludedModels` section documents every Mozilla
source-to-English model that fails one of these conditions. This makes an
omission reviewable rather than an undocumented hardcoded choice.

Whatlang reports Chinese as one Mandarin language and cannot reliably
distinguish Simplified from Traditional script. Dakia therefore currently
routes Chinese to Mozilla's released Simplified Chinese model and records the
separate Traditional Chinese model as excluded until script selection can be
made reliably.

## Refreshing the catalog

Run:

```sh
node scripts/update-translation-model-manifest.mjs
```

The generator reads Mozilla's production Remote Settings collection, evaluates
the Desktop Release filters, selects the newest complete version, and rewrites
the pinned manifest. Runtime downloads are allowed only from Mozilla's
attachment CDN and every file is verified before the pack is installed.

## Verification

The permanent test layers are:

- Rust unit tests for real-language and email-shaped detection fixtures;
- Rust integration tests for detection-to-model routing, integrity checks,
  cancellation, and atomic installation;
- TypeScript integration tests for refusal, approval, download, and translation
  orchestration without rendering UI; and
- a real Bergamot WASM worker test for Estonian, Arabic, Chinese, and Japanese:

```sh
npm run test:translation-worker
```

The runtime unloads the previous translator before switching source languages
so several large models are not retained in one WASM worker.

Before HTML reaches Bergamot, Dakia parses and serializes it as one valid HTML
document. This is required for real-world email where MIME alternatives or
forwarding software can produce concatenated `<html>` documents that abort
Bergamot's native HTML parser. A failed worker is discarded before a later
translation attempt, and native runtime details are logged rather than shown
in the reader UI.

Conversation items are sent to the single WASM worker sequentially. This caps
peak memory use for long threads rather than batching several large HTML
documents into one native call. Empty and markup-only messages bypass the
worker, malformed worker responses are rejected, and a worker that does not
respond within two minutes is terminated and recreated for the next attempt.
