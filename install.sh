#!/usr/bin/env bash
#
# mimo installer — builds from source and puts `mimo` on your PATH.
#   curl -fsSL https://raw.githubusercontent.com/Lulzx/mimo-build/main/install.sh | bash
#
# Env: MIMO_BIN_DIR (default ~/.mimo/bin), MIMO_SRC_DIR (default ~/.mimo/src)
set -euo pipefail

REPO="https://github.com/Lulzx/mimo-build"
BIN_DIR="${MIMO_BIN_DIR:-$HOME/.mimo/bin}"
SRC_DIR="${MIMO_SRC_DIR:-$HOME/.mimo/src}"

say() { printf '\033[1;35m▌\033[0m %s\n' "$1" >&2; }
err() { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || err "Rust toolchain not found. Install it from https://rustup.rs then re-run."

# Locate the source: build in place if run from a checkout, else clone/update.
if [ -f "Cargo.toml" ] && grep -q 'name = "mimo-rs"' Cargo.toml 2>/dev/null; then
    SRC_DIR="$(pwd)"
    say "Building from current checkout."
else
    command -v git >/dev/null 2>&1 || err "git is required to fetch the source."
    if [ -d "$SRC_DIR/.git" ]; then
        say "Updating $SRC_DIR"
        git -C "$SRC_DIR" pull --ff-only --quiet || true
    else
        say "Cloning $REPO → $SRC_DIR"
        mkdir -p "$(dirname "$SRC_DIR")"
        git clone --depth 1 "$REPO" "$SRC_DIR"
    fi
fi

say "Compiling (cargo build --release)…"
( cd "$SRC_DIR" && cargo build --release )

mkdir -p "$BIN_DIR"
cp -f "$SRC_DIR/target/release/mimo" "$BIN_DIR/mimo"
chmod +x "$BIN_DIR/mimo"

# macOS: re-apply an ad-hoc signature (copying invalidates it → SIGKILL on launch).
if [ "$(uname -s)" = "Darwin" ] && command -v codesign >/dev/null 2>&1; then
    codesign --force -s - "$BIN_DIR/mimo" >/dev/null 2>&1 || true
fi
say "Installed $BIN_DIR/mimo"

# --- ensure BIN_DIR is on PATH ---
on_path() { case ":$PATH:" in *":$1:"*) return 0 ;; *) return 1 ;; esac; }

linked=""
if ! on_path "$BIN_DIR"; then
    for cand in "$HOME/.local/bin" "/usr/local/bin"; do
        if on_path "$cand" && [ -d "$cand" ] && [ -w "$cand" ]; then
            ln -sf "$BIN_DIR/mimo" "$cand/mimo"; linked="$cand"; break
        fi
    done
fi

if ! on_path "$BIN_DIR" && [ -z "$linked" ]; then
    shell_name="$(basename "${SHELL:-bash}")"
    case "$shell_name" in
        zsh)  rc="$HOME/.zshrc";  line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
        fish) rc="$HOME/.config/fish/config.fish"; line="fish_add_path $BIN_DIR" ;;
        *)    rc="$HOME/.bashrc"; line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
    esac
    mkdir -p "$(dirname "$rc")"
    grep -qsF "$BIN_DIR" "$rc" 2>/dev/null || printf '\n# mimo\n%s\n' "$line" >> "$rc"
    say "Added $BIN_DIR to PATH in $rc — restart your shell, then run: mimo"
else
    say "Run 'mimo' to get started."
fi

echo >&2
say "Configure a backend in ~/.mimo/mimo-rs.toml:  [provider] base_url, api_key, model"
