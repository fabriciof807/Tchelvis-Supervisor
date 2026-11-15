#!/bin/sh
set -eu

REPO="fabriciof807/Tchelvis-Supervisor"
CHANNEL="stable"
DIR="${HOME}/.local/bin"

usage() {
    cat <<'EOF'
Usage: install.sh [--channel stable|nightly] [--dir DIR]

Options:
  --channel <stable|nightly>  Release channel to install (default: stable)
  --dir <DIR>                 Installation directory (default: $HOME/.local/bin)
  -h, --help                  Show this help message
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --channel)
            if [ $# -lt 2 ]; then
                printf 'Error: --channel requires an argument (stable or nightly)\n' >&2
                usage >&2
                exit 2
            fi
            CHANNEL="$2"
            shift 2
            ;;
        --channel=*)
            CHANNEL="${1#--channel=}"
            shift
            ;;
        --dir)
            if [ $# -lt 2 ]; then
                printf 'Error: --dir requires a directory path\n' >&2
                usage >&2
                exit 2
            fi
            DIR="$2"
            shift 2
            ;;
        --dir=*)
            DIR="${1#--dir=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'Error: unknown option: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$CHANNEL" in
    stable|nightly) ;;
    *)
        printf 'Error: invalid channel "%s" (expected stable or nightly)\n' "$CHANNEL" >&2
        usage >&2
        exit 2
        ;;
esac

RAW_ARCH="$(uname -m)"
case "$RAW_ARCH" in
    x86_64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *)
        printf 'Error: unsupported architecture "%s" (expected x86_64 or aarch64)\n' "$RAW_ARCH" >&2
        exit 1
        ;;
esac

if command -v curl >/dev/null 2>&1; then
    FETCHER="curl"
elif command -v wget >/dev/null 2>&1; then
    FETCHER="wget"
else
    printf 'Error: neither curl nor wget is installed\n' >&2
    exit 1
fi

TMPDIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT INT TERM

download_file() {
    url="$1"
    dest="$2"
    if [ "$FETCHER" = "curl" ]; then
        curl -fsSL -H "User-Agent: tchelvis-installer" "$url" -o "$dest"
    else
        wget -q -U "tchelvis-installer" -O "$dest" "$url"
    fi
}

if [ "$CHANNEL" = "nightly" ]; then
    TAG="nightly"
    VER="nightly"
else
    LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"
    LATEST_JSON="${TMPDIR}/latest.json"
    if [ "$FETCHER" = "curl" ]; then
        HTTP_CODE="$(curl -sSL -H "User-Agent: tchelvis-installer" -o "$LATEST_JSON" -w '%{http_code}' "$LATEST_URL")"
    else
        HDR_FILE="${TMPDIR}/headers"
        set +e
        wget -qS -U "tchelvis-installer" -O "$LATEST_JSON" "$LATEST_URL" 2>"$HDR_FILE"
        WGET_RC=$?
        set -e
        HTTP_CODE="$(sed -n 's/^[[:space:]]*HTTP\/[^[:space:]]*[[:space:]]*\([0-9][0-9][0-9]\).*/\1/p' "$HDR_FILE" | tail -n 1)"
        if [ -z "$HTTP_CODE" ] && [ "$WGET_RC" -ne 0 ]; then
            HTTP_CODE="000"
        fi
    fi

    if [ "$HTTP_CODE" = "404" ]; then
        printf 'No stable release yet — use --channel nightly\n' >&2
        exit 1
    fi
    if [ "$HTTP_CODE" != "200" ]; then
        printf 'Error: failed to query %s (HTTP %s)\n' "$LATEST_URL" "$HTTP_CODE" >&2
        exit 1
    fi

    TAG="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LATEST_JSON" | head -n 1)"
    if [ -z "$TAG" ]; then
        printf 'Error: could not parse tag_name from %s\n' "$LATEST_URL" >&2
        exit 1
    fi
    VER="${TAG#v}"
fi

ASSET="tchelvis-${VER}-${ARCH}-unknown-linux-musl.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
ARCHIVE_PATH="${TMPDIR}/${ASSET}"
SHA_PATH="${TMPDIR}/${ASSET}.sha256"

printf 'Downloading %s (%s)...\n' "$ASSET" "$CHANNEL"
download_file "${BASE_URL}/${ASSET}" "$ARCHIVE_PATH"
download_file "${BASE_URL}/${ASSET}.sha256" "$SHA_PATH"

(
    cd "$TMPDIR"
    sha256sum -c "${ASSET}.sha256"
)

tar -xzf "$ARCHIVE_PATH" -C "$TMPDIR"
if [ ! -f "${TMPDIR}/tchelvis" ]; then
    printf 'Error: archive did not contain tchelvis binary\n' >&2
    exit 1
fi

mkdir -p "$DIR"
install -m 0755 "${TMPDIR}/tchelvis" "${DIR}/tchelvis"
printf 'Installed tchelvis to %s/tchelvis\n' "$DIR"

case ":${PATH}:" in
    *":${DIR}:"*) ;;
    *)
        printf 'Warning: %s is not on your PATH. Add it to your shell profile.\n' "$DIR" >&2
        ;;
esac

"${DIR}/tchelvis" setup --set-default-layout

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/tchelvis"
mkdir -p "$STATE_DIR"
printf '%s\n' "$CHANNEL" > "${STATE_DIR}/update_channel"

printf 'Restart Herdr Desktop to reload the mod.\n'
