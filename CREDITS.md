# CREDITS & PROVENANCE

## Origin and Historical Attribution

This project is extracted directly from [Ravomi](https://github.com/Ravomi/ravomi-app), licensed under the GNU General Public License v3.0 (GPL-3.0-only).

Source files in `mio-coding-agent` map directly to their originals in `Ravomi/crates/ravomi-core/src/`:
- `src/lib.rs`: Extracted from `crates/ravomi-core/src/agent.rs` (core streaming loop, multi-turn tool interaction, path isolation, and timeout logic)
- `src/accounts.rs`: Extracted from `crates/ravomi-core/src/accounts.rs`
- `src/antigravity.rs`: Extracted from `crates/ravomi-core/src/antigravity.rs`
- `src/deepseek.rs`: Extracted from `crates/ravomi-core/src/deepseek.rs`
- `src/deepseek_pow.rs`: Extracted from `crates/ravomi-core/src/deepseek_pow.rs`
- `src/test_http.rs`: Extracted from `crates/ravomi-core/src/test_http.rs`

Original historical extraction took place when Ravomi was licensed under GPL-3.0-only; Mio itself is licensed under the MIT License (see `LICENSE`). This project does not claim or represent any change to the upstream Ravomi project's license.

## Upstream Software & Licenses

- **DeepSeek Web API Protocol Reference**: In `src/deepseek.rs`, the REST/patch protocol flow references `xuweizhengo/deepseek-web-api` (`ds_core`, GPL-3.0). The reference is informational for protocol compatibility; no upstream files are vendored, and third-party license terms remain respected.
- **Subagent interface design reference**: [guyiicn/pi-subagent](https://github.com/guyiicn/pi-subagent) (MIT). Named delegation, asynchronous status collection, cancellation and session reuse informed Mio's native Rust implementation. No upstream runtime or source files are vendored by this feature.

- **Source provenance: Rig 0.44.0** (no external Rig runtime dependency): the maintained `mio-core`, `mio-agent`, `mio-http`, `mio-reqwest`, `mio-tungstenite` and `mio-derive` forks retain the original MIT notice, Copyright (c) 2024 Playgrounds Analytics Inc. See `licenses/Rig-MIT.txt`.
- **Antigravity Protocol Reference**: MIT License, based on `NoeFabris/opencode-antigravity-auth`. See `licenses/Antigravity-MIT.txt`.
- **Antigravity authorization and model catalog compatibility**: [iamxeph/pi-provider-antigravity](https://github.com/iamxeph/pi-provider-antigravity), MIT, local reference version 0.22.0. OAuth payload format, current endpoint/header conventions and catalog tier mapping inform the native Rust adapter. OAuth application metadata is imported locally, not bundled; no JavaScript runtime is invoked. See `licenses/Antigravity-Provider-MIT.txt`.
- **DeepSeek PoW Solver Reference**: MIT License, based on `shaohuahuawww/deepseek-pow`. See `licenses/DeepSeek-PoW-MIT.txt`.

Resolved non-development dependencies are indexed in [licenses/dependencies/index.json](licenses/dependencies/index.json), with 316 package identities and 215 deduplicated, unmodified notice texts. The index records declared licenses, archive checksums, text SHA-256 values, and pinned upstream source URLs where the published crate omitted its notice. It covers the locked all-features workspace dependency graph across target conditions, including every package and original text covered by the earlier default-feature bundle. Development dependencies are not bundled. This is provenance evidence, not an assertion that all distribution obligations have been audited; see [the inventory procedure](docs/licenses.md).
