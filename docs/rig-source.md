# Rig source integration

Mio builds against local Rig crates in `third_party/rig/`. This preserves upstream module boundaries while allowing source-level changes. The imported Rust source is unchanged.

## Provenance

Upstream: https://github.com/0xPlaygrounds/rig, version **0.44.0**, commit `2d1071e0e0c602f994e9d6cfd8cf5faea6f53650`. Each crate retains its upstream MIT license, README, `Cargo.toml.orig`, and `.cargo_vcs_info.json`. Copyright and the acknowledgment in `CREDITS.md` remain intact.

The source was copied from the published crate archives resolved by Mio's existing lockfile. Archive SHA-256 values:

| Crate | SHA-256 |
| --- | --- |
| rig-agent | `6e3fa130bb81762e875dff982301fb599046535bbc04d908a2ee1fea583d1c67` |
| rig-core | `6c60db6e3da7681abbdfa9637c1a76dbd5a713f935f0a6753d72c5d2770ed4c6` |
| rig-http | `e9b68e5fe7258d64aeffb0a976ad0665ad2f94104d41eb4cdf463065ba57ec0b` |
| rig-reqwest | `d478ca821bdf7030b4cabfdc7acc07ab5f0449886f9a7e472de205a6ec4b2b1d` |
| rig-tungstenite | `d145d2d41722fd040dbb47f05b797c8119884697bb4ee7e53fc5c1c1ec9610df` |

Registry cache markers and nested `Cargo.lock` files were omitted. Relative to the normalized published manifests, only Rig-to-Rig runtime dependencies were changed to sibling paths. Root dependencies on `rig-core`, `rig-agent`, and `rig-reqwest` now use paths. The root lockfile removes their registry identities, without upgrading unrelated dependencies.

The optional `rig-derive` dependency remains declared upstream and is disabled in Mio. The upstream agent crate's own development dependency on `rig-reqwest` remains upstream; it is not part of Mio's build. `rig-tungstenite` is included for the existing optional WebSocket dependency but is not enabled by the default Mio feature set. Enabling new Rig features requires reviewing and internalizing any additional Rig crates first.

## Maintenance

Keep local source changes small and separate from upstream imports. Before updating, obtain the exact new crate archives, verify their checksums and license notices, compare against the retained originals, and reapply documented local changes. Preserve source revision metadata and update this file. Do not blanket-format or rename upstream code.

Validate with `cargo fmt --check`, `cargo check --all-targets --locked`, `cargo test --locked`, and `cargo clippy --all-targets --locked -- -D warnings`. Inspect `cargo tree --locked` to ensure enabled Rig crates resolve to `third_party/rig` and no second registry copy is linked.

## Publishing

This source-integrated version deliberately uses path-only dependencies. It can build from a checkout, including when embedded in Alacritty, but cannot currently publish directly to crates.io. Cargo requires registry-resolvable dependencies for publication: https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#multiple-locations.

Before publishing this version, publish appropriately named Mio-maintained fork crates and update the package mappings, or adopt another explicit source-packaging design. Do not add upstream version fallbacks merely to make packaging pass: published users would then lose Mio's local Rig changes. The project's MIT license does not solve this packaging constraint.
