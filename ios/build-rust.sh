#!/usr/bin/env bash
# Build the shim for the iPhone and the simulator, and generate the Swift that
# calls it. Run by Xcode before every build, and runnable by hand.
#
# The crate is the Android app's (`android/rust`): nothing in it is Android's
# but the name. As there, the bindings come from the compiled library rather
# than from the source, so what the app calls and what it links cannot
# disagree.
#
# What comes out, under the directory given:
#   ManymuxCore.xcframework   the static library for each target, and the C
#                             header and module map the bindings import
#   ManymuxCore.swift         the bindings, compiled into the app
set -euo pipefail

out="${1:?where to put the framework and bindings}"
here="$(cd "$(dirname "$0")" && pwd)"
crate="$here/../android/rust"

# Xcode runs this with a PATH of its own that has no cargo on it.
export PATH="$HOME/.cargo/bin:$PATH"
# And with the SDK of whatever it is building set, which the C half of ring
# would otherwise compile against instead of the target's own.
unset SDKROOT IPHONEOS_DEPLOYMENT_TARGET MACOSX_DEPLOYMENT_TARGET

# The app's deployment target. ring compiles C, so this is the compiler's
# business as well as the linker's.
export IPHONEOS_DEPLOYMENT_TARGET=17.0

targets=(aarch64-apple-ios aarch64-apple-ios-sim)
for target in "${targets[@]}"; do
    # `staticlib` asked for here rather than added to the manifest: the
    # Android build has no use for one and would pay for it on every build.
    cargo rustc --quiet --release --locked --lib \
        --manifest-path "$crate/Cargo.toml" \
        --target "$target" \
        --crate-type staticlib
done

# The host library keeps the metadata the release builds strip, and is what
# the bindings are generated from. It never enters the app.
cargo build --quiet --locked --manifest-path "$crate/Cargo.toml"
generated="$out/generated"
rm -rf "$generated"
cargo run --locked --quiet --manifest-path "$crate/Cargo.toml" --bin uniffi-bindgen -- \
    generate \
    --library "$crate/target/debug/libmanymux_android.dylib" \
    --config "$crate/uniffi.toml" \
    --language swift \
    --out-dir "$generated"

# Copied only when different, and the framework remade only when something in
# it is newer: this runs before every build, and a fresh timestamp on either is
# Xcode recompiling the app and relinking it for nothing.
same_or_copy() {
    cmp -s "$1" "$2" || cp "$1" "$2"
}
headers="$out/headers"
mkdir -p "$headers"
same_or_copy "$generated/ManymuxCoreFFI.h" "$headers/ManymuxCoreFFI.h"
# Named `module.modulemap` because that is the file Xcode looks for in a
# framework's headers; uniffi names it after the module.
same_or_copy "$generated/ManymuxCoreFFI.modulemap" "$headers/module.modulemap"
same_or_copy "$generated/ManymuxCore.swift" "$out/ManymuxCore.swift"

framework="$out/ManymuxCore.xcframework"
args=()
stale=false
if [ ! -f "$framework/Info.plist" ]; then stale=true; fi
for target in "${targets[@]}"; do
    library="$crate/target/$target/release/libmanymux_android.a"
    args+=(-library "$library" -headers "$headers")
    if [ "$library" -nt "$framework/Info.plist" ]; then stale=true; fi
done
for header in "$headers"/*; do
    if [ "$header" -nt "$framework/Info.plist" ]; then stale=true; fi
done
if $stale; then
    rm -rf "$framework"
    xcodebuild -quiet -create-xcframework "${args[@]}" -output "$framework"
fi
