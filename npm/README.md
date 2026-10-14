# Mio (`@ravomi/mio`)

Node.js wrapper and lazy prebuilt binary installer for Mio (`mio-agent`), the headless coding agent engine extracted from Ravomi.

## Requirements

- **Node.js**: `>=18.0.0`
- **Supported Prebuilt Platforms**:
  - Linux x86_64 (`x86_64-unknown-linux-gnu`)
  - Linux ARM64 (`aarch64-unknown-linux-gnu`)
  - macOS Intel (`x86_64-apple-darwin`)
  - macOS Apple Silicon (`aarch64-apple-darwin`)
  - Windows x64 (`x86_64-pc-windows-msvc`)
- **Note on Bun / pnpm**: Bun and pnpm work seamlessly. If running via `bun add -g @ravomi/mio`, ensure Node.js is present in the system environment as the wrapper script runs via `node`.
- **Pre-release status**: Prebuilt releases on GitHub and the npm package are pending initial release; Windows support is implemented per standard MSVC target specifications but not yet hardware-tested.

## How it works

1. On first run of the `mio` command, this package checks for a cached native binary under user cache (e.g. `~/.cache/mio/bin/<version>/<target>/` or `%LOCALAPPDATA%\mio\bin\<version>\<target>\`).
2. If absent, it securely downloads the official archive and `SHA256SUMS` from GitHub Releases using HTTPS (with redirection checks and strict SHA256 checksum verification).
3. The binary is cached atomically and executed transparently, passing all command-line arguments, environment variables, standard streams, and exit codes.
4. Subsequent calls bypass network requests entirely.

## Manual Binary Override

You can override the downloaded binary at any time by setting the `MIO_BIN_PATH` environment variable:

```bash
export MIO_BIN_PATH=/path/to/custom/mio
mio --version
```

## License

MIT (see LICENSE)
