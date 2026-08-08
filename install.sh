#!/bin/sh
# Install blit — https://blit.sh
# Usage: curl -sf https://install.blit.sh | sh
#
# By default this installs an MIT-licensed binary (software H.264 via
# openh264). Set BLIT_GPL=1 to opt into the GPL flavor instead, which
# uses x264 (GPL-2.0-or-later) for better software H.264 — Linux only:
#   curl -sf https://install.blit.sh | BLIT_GPL=1 sh
# Either binary prints its exact terms with `blit --license`.
set -eu

REPO="https://install.blit.sh"
pick_prefix() {
  case ":$PATH:" in
    *":$HOME/.local/bin:"*) echo "$HOME/.local" ;;
    *":$HOME/bin:"*) echo "$HOME" ;;
    *) echo "/usr/local" ;;
  esac
}
# Legacy BLIT_INSTALL_DIR pointed at the bin directory; strip the trailing /bin
# so it can be used as a prefix.
legacy_prefix="${BLIT_INSTALL_DIR:-}"
case "$legacy_prefix" in
  */bin) legacy_prefix="${legacy_prefix%/bin}" ;;
esac
PREFIX="${BLIT_PREFIX:-${legacy_prefix:-$(pick_prefix)}}"

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

  # GPL opt-in (Linux only): the blit-gpl flavor carries x264.
  flavor="blit"
  if [ "${BLIT_GPL:-}" = "1" ]; then
    case "$os" in
      linux*) flavor="blit-gpl" ;;
      *) echo "note: BLIT_GPL only applies to Linux; installing the standard binary." ;;
    esac
  fi

  version=$(fetch "$REPO/latest") || err "failed to fetch latest version"
  version=$(echo "$version" | tr -d '[:space:]')

  if [ -x "$PREFIX/bin/blit" ]; then
    current=$("$PREFIX/bin/blit" --version 2>/dev/null | awk '{print $2}') || true
    # A flavor switch at the same version still needs a reinstall; the GPL
    # flavor is recognizable by the x264 notice in `blit --license`.
    current_flavor="blit"
    if "$PREFIX/bin/blit" --license 2>/dev/null | grep -q libx264; then
      current_flavor="blit-gpl"
    fi
    if [ "$current" = "$version" ] && [ "$current_flavor" = "$flavor" ]; then
      echo "blit ${version} already installed."
      exit 0
    fi
  fi

  tarball="${flavor}_${version}_${os}_${arch}.tar.gz"
  url="$REPO/bin/$tarball"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  echo "downloading blit ${version} for ${os}/${arch}..."
  fetch "$url" > "$tmp/$tarball" || err "download failed: $url"

  tar -xzf "$tmp/$tarball" -C "$tmp"

  # Install via a temp file + rename: writing over a running binary fails
  # with ETXTBSY ("Text file busy"), while rename(2) atomically swaps the
  # directory entry and leaves the old inode to the running process.
  dst="$PREFIX/bin/blit"
  install_bin() {
    # $1: elevation command (sudo/doas) or empty for a plain install.
    $1 mkdir -p "$PREFIX/bin" &&
      $1 cp "$tmp/bin/blit" "$dst.tmp.$$" &&
      $1 chmod +x "$dst.tmp.$$" &&
      $1 mv -f "$dst.tmp.$$" "$dst" || {
      $1 rm -f "$dst.tmp.$$" 2>/dev/null
      return 1
    }
  }

  # Try without elevation first; only escalate when that genuinely fails
  # (a `-w` writability guess would force sudo in writable homes too often).
  elevate=""
  echo "installing to $PREFIX/bin..."
  if install_bin "" 2>/dev/null; then
    :
  elif [ "$(id -u)" != "0" ]; then
    elevate=$(pick_elevate)
    echo "elevation required, retrying with $elevate..."
    install_bin "$elevate" || err "installation failed"
  else
    err "installation failed"
  fi

  # Ad-hoc codesign on macOS so Gatekeeper doesn't kill the binary.
  case "$os" in
    darwin) $elevate codesign -s - "$PREFIX/bin/blit" 2>/dev/null || true ;;
  esac

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
