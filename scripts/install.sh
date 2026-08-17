#!/usr/bin/env sh
set -eu

REPO="011010/portzilla"
BIN_NAME="portzilla"
INSTALL_DIR="${PORTZILLA_INSTALL_DIR:-${HOME}/.local/bin}"
VERSION="${PORTZILLA_VERSION:-latest}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "portzilla installer: missing required command: $1" >&2
    exit 1
  fi
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux) os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *)
      echo "portzilla installer: unsupported OS: $os" >&2
      exit 1
      ;;
  esac

  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *)
      echo "portzilla installer: unsupported architecture: $arch" >&2
      exit 1
      ;;
  esac

  printf '%s-%s' "$arch" "$os"
}

download() {
  url="$1"
  out="$2"

  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --proto '=https' --tlsv1.2 --silent --show-error "$url" --output "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$out" "$url"
  else
    echo "portzilla installer: install curl or wget, or use: cargo install portzilla" >&2
    exit 1
  fi
}

verify_checksum() {
  checksum_file="$1"
  archive_file="$2"
  expected="$(awk '{print $1}' "$checksum_file")"

  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive_file" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$archive_file" | awk '{print $1}')"
  else
    echo "portzilla installer: cannot verify release checksum (sha256sum/shasum missing)" >&2
    return 1
  fi

  if [ "$expected" != "$actual" ]; then
    echo "portzilla installer: release checksum mismatch" >&2
    return 1
  fi
}

install_from_cargo() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "portzilla installer: no release asset found and cargo is not installed" >&2
    echo "Install Rust from https://rustup.rs, then run: cargo install portzilla" >&2
    exit 1
  fi

  cargo_root="$tmp_dir/cargo-root"
  if [ "$VERSION" != "latest" ]; then
    version_without_v="$(printf '%s' "$VERSION" | sed 's/^v//')"
    cargo install "$BIN_NAME" --root "$cargo_root" --version "$version_without_v"
  else
    cargo install "$BIN_NAME" --root "$cargo_root"
  fi

  # Keep the fallback consistent with PORTZILLA_INSTALL_DIR.
  mkdir -p "$INSTALL_DIR"
  install -m 755 "$cargo_root/bin/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
  echo "installed $BIN_NAME to $INSTALL_DIR/$BIN_NAME"
  exit 0
}

main() {
  target="$(detect_target)"
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT INT TERM
  archive="$tmp_dir/${BIN_NAME}.tar.gz"
  checksum="$archive.sha256"

  if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/${BIN_NAME}-${target}.tar.gz"
  else
    tag="$VERSION"
    case "$tag" in
      v*) ;;
      *) tag="v$tag" ;;
    esac
    url="https://github.com/${REPO}/releases/download/${tag}/${BIN_NAME}-${target}.tar.gz"
  fi

  if ! download "$url" "$archive"; then
    echo "portzilla installer: release asset unavailable; falling back to cargo install" >&2
    install_from_cargo
  fi

  if ! download "$url.sha256" "$checksum" || ! verify_checksum "$checksum" "$archive"; then
    echo "portzilla installer: release checksum unavailable or invalid; falling back to cargo install" >&2
    install_from_cargo
  fi

  need tar
  mkdir -p "$INSTALL_DIR"
  tar -xzf "$archive" -C "$tmp_dir"
  install -m 755 "$tmp_dir/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"

  echo "installed $BIN_NAME to $INSTALL_DIR/$BIN_NAME"
  if ! command -v "$BIN_NAME" >/dev/null 2>&1; then
    echo "note: add $INSTALL_DIR to PATH if '$BIN_NAME' is not found in new shells" >&2
  fi
}

main "$@"
