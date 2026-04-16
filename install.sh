#!/bin/sh
# Install blit — https://blit.sh
# Usage: curl -sf https://install.blit.sh | sh
set -eu

REPO="https://install.blit.sh"
pick_prefix() {
  case ":$PATH:" in
    *":$HOME/.local/bin:"*) echo "$HOME/.local" ;;
    *":$HOME/bin:"*) echo "$HOME" ;;
    *) echo "/usr/local" ;;
  esac
}
PREFIX="${BLIT_PREFIX:-${BLIT_INSTALL_DIR:-$(pick_prefix)}}"

detect_libc() {
  # Prefer glibc when available (dlopen works for GPU drivers).
  # Only use musl on musl-only systems (Alpine, Void musl, etc.).
  if command -v ldd >/dev/null 2>&1; then
    case "$(ldd --version 2>&1)" in
      *GNU*|*GLIBC*) echo "gnu"; return ;;
    esac
  fi
  # No glibc ldd found — check for glibc's ld.so directly.
  for f in /lib64/ld-linux-* /lib/ld-linux-*; do
    if [ -e "$f" ]; then
      echo "gnu"
      return
    fi
  done
  echo "musl"
}

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

  # On Linux, detect musl vs glibc to pick the right binary.
  if [ "$os" = "linux" ]; then
    libc=$(detect_libc)
    if [ "$libc" = "musl" ]; then
      os="linux-musl"
    fi
  fi

  version=$(fetch "$REPO/latest") || err "failed to fetch latest version"
  version=$(echo "$version" | tr -d '[:space:]')

  if [ -x "$PREFIX/bin/blit" ]; then
    current=$("$PREFIX/bin/blit" --version 2>/dev/null | awk '{print $2}') || true
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
  if ! [ -w "$PREFIX/bin" ] 2>/dev/null && [ "$(id -u)" != "0" ]; then
    elevate=$(pick_elevate)
    echo "installing to $PREFIX (requires $elevate)..."
  fi
  $elevate mkdir -p "$PREFIX/bin"
  $elevate cp "$tmp/bin/blit" "$PREFIX/bin/blit"
  $elevate chmod +x "$PREFIX/bin/blit"
  echo "installed blit ${version} to $PREFIX/bin/blit"

  # Generate man pages and shell completions alongside the binary
  if $elevate "$PREFIX/bin/blit" generate "$PREFIX/share" 2>/dev/null; then
    echo "generated man pages and completions in $PREFIX/share"
  fi
}

pick_elevate() {
  if command -v sudo >/dev/null 2>&1; then
    echo "sudo"
  elif command -v doas >/dev/null 2>&1; then
    echo "doas"
  else
    err "cannot write to $PREFIX and neither sudo nor doas is available"
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
