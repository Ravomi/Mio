# Mio layers and Rig provenance

Mio maintains three layers in one Cargo workspace:

| Package | Source | Responsibility |
| --- | --- | --- |
| `mio-core` | `crates/mio-core` | Model/provider contracts, messages, streaming and request preparation |
| `mio-agent` | `crates/mio-agent` | General agent loop, tools, hooks and run state |
| `mio-coding-agent` | root `src` | Coding tools, sessions, context, host integration and the `mio` CLI |

The bottom two layers are source forks of Rig, not wrapper crates. Rust imports are
`mio_core`, `mio_agent` and `mio_coding_agent`. Existing product integrations must
rename their dependency/import from `mio-agent` / `mio_agent` to
`mio-coding-agent` / `mio_coding_agent`; the `mio` command, npm package and session
format remain unchanged. This is a Rust package/API naming break.

## Provenance

Upstream: https://github.com/0xPlaygrounds/rig, version **0.44.0**, commit `2d1071e0e0c602f994e9d6cfd8cf5faea6f53650`. The imported crates retain their MIT notices, original manifests and revision metadata. The two Mio layer READMEs describe the maintained forks. Copyright and the acknowledgment in `CREDITS.md` remain intact.

The source was copied from the published crate archives resolved by Mio's existing lockfile. Archive SHA-256 values:

| Crate | SHA-256 |
| --- | --- |
| rig-agent | `6e3fa130bb81762e875dff982301fb599046535bbc04d908a2ee1fea583d1c67` |
| rig-core | `6c60db6e3da7681abbdfa9637c1a76dbd5a713f935f0a6753d72c5d2770ed4c6` |
| rig-http | `e9b68e5fe7258d64aeffb0a976ad0665ad2f94104d41eb4cdf463065ba57ec0b` |
| rig-reqwest | `d478ca821bdf7030b4cabfdc7acc07ab5f0449886f9a7e472de205a6ec4b2b1d` |
| rig-tungstenite | `d145d2d41722fd040dbb47f05b797c8119884697bb4ee7e53fc5c1c1ec9610df` |

| rig-derive | `1146bdcb55a3b2dbdd9d2df7a8846714499c9985e9042926c06ffe98b3a821bc` |

## Local integration

Transport helpers remain under `third_party/rig` (`rig-http`, `rig-reqwest`,
`rig-tungstenite`). Runtime and development dependencies resolve to these local
sources. `rig-derive` is also local: its resolver recognizes `mio-core` and
`mio-agent`, including renamed Cargo dependencies. The public `rig_tool` macro
name and internal `rig` self-aliases remain for source compatibility; they do
not link a second upstream runtime. The published upstream `rig-cassette`
adapters are not type-compatible with these forks. Implement the local
`mio_core::serve::Recorder` contract when recording is needed.

The upstream builder rejection test uses a local recorder that panics if called,
instead of importing the otherwise absent cassette crate. The switched-model warning test warms and rebuilds tracing callsite interest
before asserting, following the upstream tracing-isolation guidance. All three layers are
default workspace members, so the root quality commands cover their tests as
well as the product. Auxiliary transport and macro crates retain their own
manifests outside the workspace.

Source-walk assertions target the maintained Mio layers, rather than absent
upstream Bedrock, Gemini gRPC and ECS packages.

The provider tests also retain the referenced cassette fixtures under
`crates/mio-core/tests/fixtures/cassettes`, extracted from the exact upstream
commit above (not present in the published crate archives). This is the subset
referenced by the local tests, not the entire upstream cassette corpus.

## Maintenance and validation

Preserve upstream MIT notices and archive/revision provenance when updating.
Compare new upstream sources against the original archives, then reapply the
local naming and behavior changes. `Cargo.toml.orig` files are archival only.

Run `cargo fmt --check`, `cargo check --all-targets --locked`,
`cargo test --locked`, and `cargo clippy --all-targets --locked -- -D warnings`.
Inspect `cargo tree --locked` to ensure no registry `rig-core` or `rig-agent`
is linked. Optional feature combinations need their own validation.

## Publishing

This checkout uses local path dependencies. Renaming alone does not make these
packages publishable: supporting transport/macro forks also need a deliberate
registry packaging strategy before crates.io publication. Do not substitute
upstream registry versions for the local forks to bypass packaging checks.
Binary release builds still select the `mio` executable from the product crate.
