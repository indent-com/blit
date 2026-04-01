#!/bin/sh
# Install blit — https://blit.sh
# Usage: curl https://install.blit.sh | sh
set -eu

REPO="https://install.blit.sh"
INSTALL_DIR="${BLIT_INSTALL_DIR:-/usr/local/bin}"

main() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)

  case "$os" in
    linux)  os="linux" ;;
    darwin) os="darwin" ;;
    *) err "unsupported OS: $os" ;;
  esac

  case "$arch" in
    x86_64|amd64)   arch="x86_64" ;;
    aarch64|arm64)   arch="aarch64" ;;
    *) err "unsupported architecture: $arch" ;;
  esac

  version=$(fetch "$REPO/latest") || err "failed to fetch latest version"
  version=$(echo "$version" | tr -d '[:space:]')

  if [ -x "$INSTALL_DIR/blit" ]; then
    current=$("$INSTALL_DIR/blit" --version 2>/dev/null | awk '{print $2}') || true
    if [ "$current" = "$version" ]; then
      echo "blit ${version} already installed."
      exit 0
    fi
  fi

  tarball="blit_${version}_${os}_${arch}.tar.gz"
  url="$REPO/bin/$tarball"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  echo "downloading blit ${version} for ${os}/${arch}..."
  fetch "$url" > "$tmp/$tarball" || err "download failed: $url"

  tar -xzf "$tmp/$tarball" -C "$tmp"

  elevate=""
  if ! [ -w "$INSTALL_DIR" ] && [ "$(id -u)" != "0" ]; then
    elevate=$(pick_elevate)
    echo "installing to $INSTALL_DIR (requires $elevate)..."
  fi
  $elevate mkdir -p "$INSTALL_DIR"
  $elevate mv "$tmp/blit" "$INSTALL_DIR/blit"
  $elevate chmod +x "$INSTALL_DIR/blit"
  echo "installed blit ${version} to $INSTALL_DIR/blit"
}

pick_elevate() {
  if command -v sudo >/dev/null 2>&1; then
    echo "sudo"
  elif command -v doas >/dev/null 2>&1; then
    echo "doas"
  else
    err "cannot write to $INSTALL_DIR and neither sudo nor doas is available"
  fi
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$1"
  else
    err "curl or wget required"
  fi
}

err() {
  echo "error: $1" >&2
  exit 1
}

main
