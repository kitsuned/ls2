#!/bin/sh
set -eu

mode=full
if [ "${1:-}" = --isolated-only ]; then
    mode=isolated
    shift
fi
if [ "$#" -ne 1 ]; then
    echo "Usage: $0 [--isolated-only] user@tv" >&2
    exit 2
fi

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
host=$1
target_dir=${CARGO_TARGET_DIR:-$root/target}
rust_fixture=$target_dir/armv7-unknown-linux-gnueabi/release/examples/tv_smoke
native_fixture=$target_dir/webos-fixtures/native-fixture
if [ ! -x "$rust_fixture" ] || [ ! -x "$native_fixture" ]; then
    echo 'Run tools/build-tv.sh to build the TV fixtures first.' >&2
    exit 1
fi

tv() {
    ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=5 -- "$host" "$@"
}

stage=$(tv 'mktemp -d /tmp/ls2-interop.XXXXXX')
suffix=${stage#/tmp/ls2-interop.}
case "$suffix" in
    *[!a-zA-Z0-9]*|'') echo 'Unexpected temporary path from TV' >&2; exit 1 ;;
esac
if [ "$stage" = "$suffix" ] || [ "${#suffix}" -ne 6 ]; then
    echo 'Unexpected temporary path from TV' >&2
    exit 1
fi

cleanup() {
    tv "rm -rf -- '$stage'" >/dev/null
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

tv "cat > '$stage/tv_smoke'" < "$rust_fixture"
tv "cat > '$stage/native-fixture'" < "$native_fixture"
tv "cat > '$stage/session.sh'" < "$root/tests/native/tv-session.sh"
tv "chmod 700 '$stage/tv_smoke' '$stage/native-fixture' && /bin/sh '$stage/session.sh' '$stage' '$mode'"
