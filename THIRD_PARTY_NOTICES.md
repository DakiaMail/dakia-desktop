# Third-party notices

The audited license texts and provenance records for the bundled classifier,
its base model and tokenizer, and ONNX Runtime are distributed with Dakia
under `licenses/`. In the source tree, they are maintained at
`apps/desktop/src-tauri/resources/licenses/`.

## Ippoboi/mmbert-s-email-classifier

Dakia bundles an ONNX export of `Ippoboi/mmbert-s-email-classifier` at revision
`72de7110305b5e1d98d26aa0578482a230739c0c`, including the tokenizer and
associated configuration files. The immutable model card for that revision
declares the model under the Apache License 2.0.

Source and license declaration:
https://huggingface.co/Ippoboi/mmbert-s-email-classifier/tree/72de7110305b5e1d98d26aa0578482a230739c0c

Full license text: `licenses/Apache-2.0.txt`

## jhu-clsp/mmBERT-small

The classifier export identifies `jhu-clsp/mmBERT-small` as its base model, and
the bundled tokenizer originates from that model family. The upstream model
card declares mmBERT-small under the MIT License. The export metadata does not
record the exact base-model revision; for reproducible license provenance, the
license declaration was verified at upstream revision
`abc32620dd4f6ab06f5fbe905dc25f310618e09f`.
That upstream revision publishes no standalone license file or copyright
notice, so Dakia records the omission rather than inventing one.

Source and license declaration:
https://huggingface.co/jhu-clsp/mmBERT-small/tree/abc32620dd4f6ab06f5fbe905dc25f310618e09f

MIT terms and upstream provenance notice:
`licenses/mmBERT-small-MIT-NOTICE.txt`

## ONNX Runtime 1.23.2

Dakia bundles the ONNX Runtime 1.23.2 macOS dynamic library from Microsoft's
official `onnxruntime-osx-universal2-1.23.2.tgz` release archive. ONNX Runtime
is licensed under the MIT License. Microsoft's complete third-party notices
from that exact release are distributed without modification.

Source: https://github.com/microsoft/onnxruntime/tree/v1.23.2

Release:
https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-osx-universal2-1.23.2.tgz

Full license text: `licenses/ONNX-Runtime-1.23.2-LICENSE.txt`

Official third-party notices:
`licenses/ONNX-Runtime-1.23.2-ThirdPartyNotices.txt`

## Bergamot Translator

Dakia includes the Bergamot Translator WebAssembly runtime from the
`@browsermt/bergamot-translator` package. Bergamot is licensed under the
Mozilla Public License 2.0.

Source: https://github.com/browsermt/bergamot-translator

License: https://www.mozilla.org/MPL/2.0/

Full license text: `licenses/MPL-2.0.txt`

## Firefox Translations language models

Offline translation language packs are selected from Mozilla's production
Translations Remote Settings registry and downloaded on demand. Dakia pins the
URL, byte size, and SHA-256 digest of every accepted model artifact. Mozilla's
translation implementation and model tooling are licensed under the Mozilla
Public License 2.0.

Registry: https://firefox.settings.services.mozilla.com/v1/buckets/main/collections/translations-models/records

Source: https://github.com/mozilla/translations

Full license text: `licenses/MPL-2.0.txt`

## Whatlang

Dakia uses the `whatlang` Rust crate for on-device language identification.
Whatlang is licensed under the MIT License.

Source: https://github.com/greyblake/whatlang-rs
