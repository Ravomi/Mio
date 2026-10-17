# Mio layers and Rig provenance

This is a source-origin record, not a current dependency declaration. The
resolved workspace contains no external `rig-*` packages; the maintained
runtime and transport packages use Mio names. Original copyright and MIT
permission notices remain with the forked source.

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

Upstream: https://github.com/0xPlaygrounds/rig, version **0.44.0**, commit `2d1071e0e0c602f994e9d6cfd8cf5faea6f53650`. The source forks retain their MIT notices, original manifests and revision metadata. The two Mio layer READMEs describe the maintained forks. Copyright and the acknowledgment in `CREDITS.md` remain intact.

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

Transport contracts, HTTP and websocket implementations, and procedural macros are
maintained as `mio-http`, `mio-reqwest`, `mio-tungstenite` and `mio-derive` under
`crates/`. All seven packages are default workspace members. Dependencies and
Rust imports use Mio names; no upstream Rig package is linked. The public
`rig_tool` macro and internal `rig` self-aliases remain for source compatibility.
The macro resolver recognizes `mio-core` and `mio-agent`, including renamed
Cargo dependencies. The published upstream `rig-cassette` adapters are not
type-compatible; implement `mio_core::serve::Recorder` for recording.

The upstream builder rejection test uses a local recorder that panics if called,
instead of importing the otherwise absent cassette crate. The switched-model warning test warms and rebuilds tracing callsite interest
before asserting, following the upstream tracing-isolation guidance. All layers and supporting crates are default workspace members, so root quality
commands also cover transport and macro tests.

Source-walk assertions target the maintained Mio layers, rather than absent
upstream Bedrock, Gemini gRPC and ECS packages.

The provider tests also retain the referenced cassette fixtures under
`crates/mio-core/tests/fixtures/cassettes`, extracted from the exact upstream
commit above (not present in the published crate archives). This is the subset
referenced by the local tests, not the entire upstream cassette corpus.

## History ownership optimizations

Request preparation inserts documents into its owned history in place; the
non-mutating telemetry view retains its existing behavior. A fresh agent run
takes explicit history from its runner after memory resolution, preserving
loaded-history precedence. The coding layer moves completed response messages
into the persisted transcript. Regression tests check allocation reuse,
document ordering/idempotence and history precedence. No latency percentage is
claimed; the product still retains its initial history copy, and resumed run
snapshots keep their existing clone semantics.

## Maintenance and validation

Preserve upstream MIT notices and archive/revision provenance when updating.
Compare new upstream sources against the original archives, then reapply the
local naming and behavior changes. `Cargo.toml.orig` files are archival only.

Run `cargo fmt --check`, `cargo check --all-targets --locked`,
`cargo test --locked`, and `cargo clippy --all-targets --locked -- -D warnings`.
Inspect `cargo tree --locked` to ensure no registry Rig package
is linked. Optional feature combinations need their own validation.

## Publishing

This checkout uses local path dependencies. Renaming alone does not make these
packages publishable: supporting transport/macro forks also need a deliberate
registry packaging strategy before crates.io publication. Do not substitute
upstream registry versions for the local forks to bypass packaging checks.
Binary release builds still select the `mio` executable from the product crate.
