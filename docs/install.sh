#!/usr/bin/env bash
set -euo pipefail

# Mio official installer for Linux and macOS
# Repository: https://github.com/Ravomi/Mio

REPO="Ravomi/Mio"
MIO_VERSION="${MIO_VERSION:-latest}"
INSTALL_DIR="${MIO_INSTALL_DIR:-$HOME/.local/bin}"
BASE_URL="${MIO_RELEASE_BASE_URL:-https://github.com/${REPO}/releases}"

echo "==> Installing Mio coding agent..."

detect_target() {
  local os arch
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"

  case "$os" in
    linux) os="unknown-linux-gnu" ;;
    darwin) os="apple-darwin" ;;
    *)
      echo "Error: Unsupported operating system: $(uname -s)" >&2
      echo "Mio prebuilt binaries support Linux and macOS." >&2
      exit 1
      ;;
  esac

  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *)
      echo "Error: Unsupported architecture: $(uname -m)" >&2
      echo "Supported architectures are x86_64 and aarch64/arm64." >&2
      exit 1
      ;;
  esac

  echo "${arch}-${os}"
}

download_file() {
  local url="$1"
  local dest="$2"

  if command -v curl >/dev/null 2>&1; then
    if [ "${MIO_ALLOW_INSECURE_TEST_HTTP:-}" = "1" ]; then
      curl --connect-timeout 15 --max-time 120 -fsSL "$url" -o "$dest"
    else
      curl --connect-timeout 15 --max-time 120 -fsSL \
        --proto '=https' \
        --proto-redir '=https' \
        --tlsv1.2 \
        "$url" -o "$dest"
    fi
  elif command -v wget >/dev/null 2>&1; then
    if [ "${MIO_ALLOW_INSECURE_TEST_HTTP:-}" = "1" ]; then
      wget --connect-timeout=15 --timeout=120 -q "$url" -O "$dest"
    else
      wget --connect-timeout=15 --timeout=120 -q --https-only "$url" -O "$dest"
    fi
  else
    echo "Error: Neither curl nor wget was found in PATH." >&2
    exit 1
  fi
}

calculate_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    echo "Error: Neither sha256sum nor shasum was found in PATH." >&2
    exit 1
  fi
}

TARGET="$(detect_target)"
MIO_TEMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'mio-install')"
STAGE_FILE=""
cleanup() {
  local exit_code=$?
  if [ -n "$STAGE_FILE" ] && [ -f "$STAGE_FILE" ]; then
    rm -f "$STAGE_FILE"
  fi
  rm -rf "$MIO_TEMP_DIR"
  exit "$exit_code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$MIO_VERSION" = "latest" ]; then
  DOWNLOAD_ROOT="${BASE_URL%/}/latest/download"
else
  DOWNLOAD_ROOT="${BASE_URL%/}/download/${MIO_VERSION}"
fi

ARCHIVE_NAME="mio-${TARGET}.tar.gz"
ARCHIVE_URL="${DOWNLOAD_ROOT}/${ARCHIVE_NAME}"
CHECKSUMS_URL="${DOWNLOAD_ROOT}/SHA256SUMS"

echo "Detected target: ${TARGET}"
echo "Target version:  ${MIO_VERSION}"
echo "Downloading checksums: ${CHECKSUMS_URL}"

if ! download_file "$CHECKSUMS_URL" "$MIO_TEMP_DIR/SHA256SUMS"; then
  echo "Error: Failed to download SHA256SUMS from: $CHECKSUMS_URL" >&2
  echo "Note: If the release is not published yet, releases on Ravomi/Mio may not exist." >&2
  exit 1
fi

echo "Downloading archive: ${ARCHIVE_URL}"
if ! download_file "$ARCHIVE_URL" "$MIO_TEMP_DIR/$ARCHIVE_NAME"; then
  echo "Error: Failed to download archive from: $ARCHIVE_URL" >&2
  exit 1
fi

# Exact 64-char hex match for archive filename, reject duplicates
FOUND_HASHES=$(awk -v target="$ARCHIVE_NAME" '
  /^[0-9a-fA-F]{64}[ \t]+(\*)?[^ \t\r\n]+$/ {
    filename = $2;
    sub(/^\*/, "", filename);
    if (filename == target) {
      print tolower($1);
    }
  }
' "$MIO_TEMP_DIR/SHA256SUMS")

HASH_COUNT=$(echo "$FOUND_HASHES" | sed '/^$/d' | wc -l)
if [ "$HASH_COUNT" -eq 0 ]; then
  echo "Error: Archive ${ARCHIVE_NAME} not found in SHA256SUMS file or invalid hash format." >&2
  exit 1
fi

if [ "$HASH_COUNT" -gt 1 ]; then
  echo "Error: Multiple entries found for ${ARCHIVE_NAME} in SHA256SUMS." >&2
  exit 1
fi

EXPECTED_SHA="$FOUND_HASHES"

ACTUAL_SHA=$(calculate_sha256 "$MIO_TEMP_DIR/$ARCHIVE_NAME" | tr '[:upper:]' '[:lower:]')
if [ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]; then
  echo "Error: SHA256 verification failed!" >&2
  echo "Expected: $EXPECTED_SHA" >&2
  echo "Actual:   $ACTUAL_SHA" >&2
  exit 1
fi

echo "SHA256 checksum verified: ${ACTUAL_SHA}"

mkdir -p "$MIO_TEMP_DIR/extracted"
tar -xzf "$MIO_TEMP_DIR/$ARCHIVE_NAME" -C "$MIO_TEMP_DIR/extracted"

if [ ! -f "$MIO_TEMP_DIR/extracted/mio" ]; then
  echo "Error: Archive did not contain 'mio' binary executable at root." >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
chmod 755 "$MIO_TEMP_DIR/extracted/mio"

# Atomic replacement of target binary using mktemp inside INSTALL_DIR
STAGE_FILE="$(mktemp "${INSTALL_DIR}/.mio.stage.XXXXXX")"
cp "$MIO_TEMP_DIR/extracted/mio" "$STAGE_FILE"
chmod 755 "$STAGE_FILE"
mv -f "$STAGE_FILE" "${INSTALL_DIR}/mio"
STAGE_FILE=""

echo "==> Successfully installed mio to: ${INSTALL_DIR}/mio"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "Warning: ${INSTALL_DIR} is not in your PATH."
    echo "Add it to your profile (e.g. ~/.bashrc or ~/.zshrc):"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
