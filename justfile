# manymux
#
# `just` with no arguments lists what there is.
#
# Two crates live here. The root one is the node and the desktop client; the
# one under `android/rust` is the phone's client core, deliberately outside the
# workspace so a root `cargo test` never tries to build russh and uniffi. The
# recipes keep that split: `test` is the root crate, `android-test` is the shim,
# and `everything` is what CI runs.

default:
    @just --list --unsorted

# ---- the root crate --------------------------------------------------------

# Unit tests plus the end-to-end suites.
test *args:
    cargo test -q {{args}}

# A release binary.
build:
    cargo build -q --release

# Format the root crate.
fmt:
    cargo fmt -q

# Format check and clippy, for the root crate.
lint:
    cargo fmt -q --check
    cargo clippy -q --all-targets --locked -- -D warnings

# The library without `desktop`: what the app links, and a CI gate.
core:
    cargo check -q --lib --no-default-features --locked

# Run the binary on a socket of its own, not your real node: `just run ls`.
run *args:
    MM_CONFIG_DIR=/tmp/mm-dev cargo run -q -- --socket /tmp/mm-dev.sock {{args}}

# ---- the android app -------------------------------------------------------

# The debug APK, both ABIs. Wants ANDROID_HOME and an NDK.
android:
    cd android && ./gradlew --no-daemon assembleDebug
    @echo
    @ls -lh android/app/build/outputs/apk/debug/app-debug.apk

# The same, from nothing. Slower, and the answer to "did it actually rebuild".
android-clean:
    cd android && ./gradlew --no-daemon clean
    rm -rf android/app/build android/build
    cd android && ./gradlew --no-daemon assembleDebug
    @ls -lh android/app/build/outputs/apk/debug/app-debug.apk

# Put it on whatever is plugged in.
android-install: android
    adb install -r android/app/build/outputs/apk/debug/app-debug.apk

# What the app is saying, without the rest of the system's noise.
android-log:
    adb logcat -v brief manymux:V manymux_android:V AndroidRuntime:E '*:S'

# The shim's own tests, on this machine's target. No NDK needed.
android-test *args:
    cd android/rust && cargo test -q {{args}}

# Format and clippy, for the shim.
android-lint:
    cd android/rust && cargo fmt -q --check
    cd android/rust && cargo clippy -q --all-targets --locked -- -D warnings

# The app's client stack from a terminal: `just reach user@host [session]`.
reach *args:
    cd android/rust && cargo run -q --example reach -- {{args}}

# ---- everything ------------------------------------------------------------

# What CI runs, in the order it runs it.
everything: lint core test android-lint android-test
    @echo "all green"
