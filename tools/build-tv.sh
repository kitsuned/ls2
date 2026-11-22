#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

sdk=${WEBOS_SDK:-/opt/webos-sdk}
compiler=$sdk/bin/arm-webos-linux-gnueabi-gcc
readelf=$sdk/bin/arm-webos-linux-gnueabi-readelf
if [ ! -x "$compiler" ] || [ ! -x "$readelf" ]; then
    echo "Set WEBOS_SDK to the relocated buildroot-nc4 2026.08-webos SDK." >&2
    exit 1
fi

target=armv7-unknown-linux-gnueabi
sysroot=$("$compiler" -print-sysroot)
polyfills=$sysroot/usr/lib/libglibc_polyfills.a
if [ ! -f "$polyfills" ]; then
    echo "Missing buildroot-nc4 compatibility archive $polyfills" >&2
    exit 1
fi

export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABI_LINKER="$compiler"

target_dir=${CARGO_TARGET_DIR:-$root/target}
export CARGO_TARGET_DIR="$target_dir"
mkdir -p "$target_dir/webos-fixtures"

# Rust's precompiled standard library uses APIs backported by the SDK archive.
# Link the archive after the Rust libraries so GNU ld can resolve their symbols.
cargo rustc --release --target "$target" --example tv_smoke "$@" -- \
    -C "link-arg=$polyfills"

"$compiler" -std=c11 -Wall -Wextra -Werror -O2 \
    -I"$sysroot/usr/include/glib-2.0" -I"$sysroot/usr/lib/glib-2.0/include" \
    tests/native/fixture.c -o "$target_dir/webos-fixtures/native-fixture" \
    -Wl,-rpath-link,"$sysroot/usr/lib" -lluna-service2 -lglib-2.0

rust_fixture=$target_dir/$target/release/examples/tv_smoke
for fixture in "$rust_fixture" "$target_dir/webos-fixtures/native-fixture"; do
    header=$(LC_ALL=C "$readelf" -h -A -W "$fixture")
    if ! printf '%s\n' "$header" | awk '
        /Class:.*ELF32/ { elf32 = 1 }
        /Data:.*little endian/ { le = 1 }
        /Machine:.*ARM$/ { arm = 1 }
        /Flags:.*Version5 EABI, soft-float ABI/ { abi = 1 }
        /Tag_CPU_arch: v7$/ { armv7 = 1 }
        END { exit !(elf32 && le && arm && abi && armv7) }
    '; then
        echo "Unexpected TV ABI in $fixture" >&2
        exit 1
    fi
    LC_ALL=C "$readelf" -l -W "$fixture" | grep -F 'interpreter: /lib/ld-linux.so.3]'
    printf '%s\n' "$fixture"
    LC_ALL=C "$readelf" -d -W "$fixture" | grep NEEDED
done

if LC_ALL=C "$readelf" -d -W "$rust_fixture" | grep -Eq 'NEEDED.*lib(luna-service2|glib-2\.0)'; then
    echo 'Rust TV fixture unexpectedly links native LS2 or GLib.' >&2
    exit 1
fi
