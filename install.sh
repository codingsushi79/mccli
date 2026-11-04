#!/usr/bin/env bash
#
# mchost installer
#
#   curl -fsSL https://raw.githubusercontent.com/codingsushi79/mccli/master/install.sh | bash
#
# Environment overrides:
#   MCHOST_INSTALL_DIR   where to put the binary (default: first sensible dir on PATH)
#   MCHOST_REPO          owner/name to clone from     (default: codingsushi79/mccli)
#   MCHOST_BRANCH        branch to build              (default: master)
#   MCHOST_NO_COLOR=1    plain output
#
set -euo pipefail

REPO="${MCHOST_REPO:-codingsushi79/mccli}"
BRANCH="${MCHOST_BRANCH:-master}"
BIN_NAME="mchost"
ALIAS_NAME="mccli"

# ------------------------------------------------------------------ styling --

if [ -t 1 ] && [ -z "${MCHOST_NO_COLOR:-}" ] && [ "${TERM:-dumb}" != "dumb" ]; then
    B=$'\033[1m'; DIM=$'\033[2m'; R=$'\033[0m'
    BLUE=$'\033[38;5;111m'; GREEN=$'\033[38;5;150m'
    AMBER=$'\033[38;5;179m'; RED=$'\033[38;5;204m'; PURPLE=$'\033[38;5;141m'
else
    B=""; DIM=""; R=""; BLUE=""; GREEN=""; AMBER=""; RED=""; PURPLE=""
fi

BOX_W=62
RULE="$(printf '─%.0s' $(seq 1 $BOX_W))"

box_line() {
    local edge="$1" rendered="$2" plain pad
    plain="$(printf '%s' "$rendered" | sed $'s/\033\[[0-9;]*m//g')"
    pad=$(( BOX_W - ${#plain} - 2 ))
    if [ "$pad" -lt 0 ]; then pad=0; fi
    printf '   %s│%s %s%*s %s│%s\n' "$edge" "$R" "$rendered" "$pad" "" "$edge" "$R"
}

box_top()    { printf '   %s┌%s┐%s\n' "$1" "$RULE" "$R"; }
box_bottom() { printf '   %s└%s┘%s\n' "$1" "$RULE" "$R"; }

banner() {
    printf '\n'
    box_top "$BLUE"
    box_line "$BLUE" ""
    box_line "$BLUE" "$B$PURPLE █▀▄▀█ █▀▀ █ █ █▀█ █▀ ▀█▀$R"
    box_line "$BLUE" "$B$PURPLE █ ▀ █ █▄▄ █▀█ █▄█ ▄█  █ $R"
    box_line "$BLUE" ""
    box_line "$BLUE" "${DIM} host Minecraft servers, control them from anywhere${R}"
    box_line "$BLUE" ""
    box_bottom "$BLUE"
    printf '\n'
}

step()  { printf '   %s▸%s %s\n' "$BLUE$B" "$R" "$1"; }
ok()    { printf '   %s✓%s %s\n' "$GREEN$B" "$R" "$1"; }
info()  { printf '     %s%s%s\n' "$DIM" "$1" "$R"; }
warn()  { printf '   %s!%s %s\n' "$AMBER$B" "$R" "$1"; }
die()   { printf '\n   %s✗ %s%s\n\n' "$RED$B" "$1" "$R" >&2; exit 1; }

# Run a command quietly with a spinner; show its log only if it fails.
spin() {
    local message="$1"; shift
    local log; log="$(mktemp)"
    local frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
    "$@" >"$log" 2>&1 &
    local pid=$!
    local start; start=$SECONDS
    if [ -t 1 ]; then
        local i=0
        while kill -0 "$pid" 2>/dev/null; do
            i=$(( (i + 1) % 10 ))
            printf '\r   %s%s%s %s %s(%ss)%s ' \
                "$BLUE" "${frames:$i:1}" "$R" "$message" "$DIM" "$((SECONDS - start))" "$R"
            sleep 0.1
        done
        printf '\r\033[K'
    else
        printf '   … %s\n' "$message"
    fi
    if wait "$pid"; then
        ok "$message $DIM($((SECONDS - start))s)$R"
        rm -f "$log"
    else
        printf '\n'
        warn "$message failed:"
        tail -n 25 "$log" | sed 's/^/     /'
        rm -f "$log"
        exit 1
    fi
}

cleanup() {
    if [ -n "${TMP_DIR:-}" ]; then rm -rf "$TMP_DIR"; fi
    return 0
}
trap cleanup EXIT

# ------------------------------------------------------------ requirements --

banner

case "$(uname -s)" in
    Linux|Darwin) ;;
    *) die "mchost supports Linux and macOS." ;;
esac

step "Checking prerequisites"
if ! command -v cargo >/dev/null 2>&1; then
    printf '\n'
    warn "Rust is not installed."
    info "mchost builds from source. Install Rust, then re-run this script:"
    printf "\n     %scurl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh%s\n\n" "$B" "$R"
    exit 1
fi
ok "cargo $(cargo --version | awk '{print $2}')"

if command -v java >/dev/null 2>&1 && java -version >/dev/null 2>&1; then
    ok "$(java -version 2>&1 | head -n1)"
else
    warn "no Java found"
    info "only needed on the machine that hosts servers (Java 21+ for MC 1.20.5+)"
fi

# ------------------------------------------------------------------ source --

if [ -f "Cargo.toml" ] && grep -q '^name = "mchost"' Cargo.toml 2>/dev/null; then
    SRC_DIR="$PWD"
    step "Building from the current checkout"
else
    command -v git >/dev/null 2>&1 || die "git is required to fetch the source."
    TMP_DIR="$(mktemp -d)"
    SRC_DIR="$TMP_DIR/mccli"
    step "Fetching $REPO"
    spin "cloning $BRANCH" git clone --depth 1 --branch "$BRANCH" \
        "https://github.com/$REPO.git" "$SRC_DIR"
fi

step "Building (this takes a minute the first time)"
spin "cargo build --release" cargo build --release --locked --manifest-path "$SRC_DIR/Cargo.toml"

BUILT="$SRC_DIR/target/release/$BIN_NAME"
[ -x "$BUILT" ] || die "build finished but $BUILT is missing."

# ----------------------------------------------------------------- install --

pick_dir() {
    if [ -n "${MCHOST_INSTALL_DIR:-}" ]; then
        printf '%s' "$MCHOST_INSTALL_DIR"; return
    fi
    local candidates="$HOME/.cargo/bin $HOME/.local/bin /usr/local/bin"
    for dir in $candidates; do
        case ":$PATH:" in *":$dir:"*)
            if [ -d "$dir" ] && [ -w "$dir" ]; then printf '%s' "$dir"; return; fi
        esac
    done
    for dir in $candidates; do
        if [ -d "$dir" ] && [ -w "$dir" ]; then printf '%s' "$dir"; return; fi
    done
    printf '%s' "$HOME/.local/bin"
}

INSTALL_DIR="$(pick_dir)"
mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] || die "$INSTALL_DIR is not writable. Set MCHOST_INSTALL_DIR to somewhere you own."

step "Installing"
install -m 755 "$BUILT" "$INSTALL_DIR/$BIN_NAME"
ln -sf "$INSTALL_DIR/$BIN_NAME" "$INSTALL_DIR/$ALIAS_NAME"
ok "$INSTALL_DIR/$BIN_NAME"
info "also linked as \`$ALIAS_NAME\`"
ok "$("$INSTALL_DIR/$BIN_NAME" --version 2>/dev/null || echo "$BIN_NAME")"

# ---------------------------------------------------------------- PATH help --

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        case "$(basename "${SHELL:-bash}")" in
            zsh)  RC="$HOME/.zshrc" ;;
            fish) RC="$HOME/.config/fish/config.fish" ;;
            *)    RC="$HOME/.bashrc" ;;
        esac
        printf '\n'
        warn "$INSTALL_DIR is not on your PATH."
        if [ "$(basename "${SHELL:-bash}")" = "fish" ]; then
            info "Add it with:"
            printf '\n     %sfish_add_path %s%s\n' "$B" "$INSTALL_DIR" "$R"
        else
            info "Add it to $RC:"
            printf '\n     %secho '"'"'export PATH="%s:$PATH"'"'"' >> %s%s\n' \
                "$B" "$INSTALL_DIR" "$RC" "$R"
        fi
        printf '     %sthen: source %s%s\n' "$B" "$RC" "$R"
        ;;
esac

# ------------------------------------------------------------- next steps ---

printf '\n'
box_top "$GREEN"
box_line "$GREEN" "${B}Installed.${R}"
box_line "$GREEN" ""
box_line "$GREEN" "${DIM}On the machine that hosts the servers:${R}"
box_line "$GREEN" "   ${B}mchost serve${R}   ${DIM}(prints the admin password once)${R}"
box_line "$GREEN" ""
box_line "$GREEN" "${DIM}From any machine:${R}"
box_line "$GREEN" "   ${B}mchost connect <host ip or url>${R}"
box_line "$GREEN" ""
box_bottom "$GREEN"
printf '\n'
