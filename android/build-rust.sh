#!/usr/bin/env bash
# Build the shim for each ABI the app ships, and generate the Kotlin that calls
# it. Run by Gradle, and runnable by hand.
#
# Both halves come from the same build on purpose: the bindings are generated
# from the compiled library rather than from the source, so what the app calls
# and what the app links cannot disagree.
set -euo pipefail

out="${1:?where to put the generated library and bindings}"
here="$(cd "$(dirname "$0")" && pwd)"
crate="$here/rust"

: "${ANDROID_NDK_HOME:=$(ls -d "${ANDROID_HOME:-$HOME/Android/Sdk}"/ndk/* 2>/dev/null | sort -V | tail -1)}"
if [ ! -d "$ANDROID_NDK_HOME" ]; then
    echo "no NDK: set ANDROID_NDK_HOME" >&2
    exit 1
fi
tools="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
if [ ! -d "$tools" ]; then
    tools="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin"
fi

# The API level the app's `minSdk` names. The shim compiles C, so this is the
# compiler's business as well as the linker's.
api=24

build() {
    local target="$1" abi="$2" prefix="$3"
    local clang="$tools/$prefix$api-clang"
    local upper
    upper="$(echo "$target" | tr 'a-z-' 'A-Z_')"
    env \
        "CC_$(echo "$target" | tr - _)=$clang" \
        "AR_$(echo "$target" | tr - _)=$tools/llvm-ar" \
        "CARGO_TARGET_${upper}_LINKER=$clang" \
        cargo build --release --manifest-path "$crate/Cargo.toml" --target "$target"
    mkdir -p "$out/jniLibs/$abi"
    cp "$crate/target/$target/release/libmanymux_android.so" "$out/jniLibs/$abi/"
}

build aarch64-linux-android arm64-v8a aarch64-linux-android
build x86_64-linux-android x86_64 x86_64-linux-android

# The bindings, from a host build of the same crate: uniffi reads the library's
# own metadata, and a host library carries the same metadata as a cross one.
cargo build --release --manifest-path "$crate/Cargo.toml"
rm -rf "$out/uniffi"
cargo run --release --quiet --manifest-path "$crate/Cargo.toml" --bin uniffi-bindgen -- \
    generate \
    --library "$crate/target/release/libmanymux_android.so" \
    --language kotlin \
    --no-format \
    --out-dir "$out/uniffi"
