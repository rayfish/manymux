# manymux for Android

`rust/` is a `cdylib` shim: an ssh transport that ends in the `(read, write)`
pair `client::Stream::from_halves` takes, and a screen driven by `avt` on this
side of the wire. Everything between the two is the library one directory up,
linked in with `default-features = false`.

It is deliberately outside the root crate's build. It carries its own
`[workspace]` table and its own `Cargo.lock`, so `cargo test`, `cargo clippy`
and every `--locked` cross build of `mm` see exactly the crate they saw before
this directory existed.

## Building

The shim compiles C, which the root crate does not: `russh` refuses to build
without either `ring` or `aws-lc-rs`, and both are C. `ring` is the one chosen,
because `aws-lc-rs` additionally wants cmake. So the NDK is needed for its
clang, not only for its linker.

```bash
cd rust
cargo test -q                      # host target, no NDK needed

export ANDROID_NDK_HOME=/path/to/ndk/29.0.14206865
TOOLS="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CC_aarch64_linux_android="$TOOLS/aarch64-linux-android24-clang"
export AR_aarch64_linux_android="$TOOLS/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
cargo build -q --release --target aarch64-linux-android
```

API 24 to match the root crate's own Android target, which is the oldest
Termux still supports. `cargo-ndk` sets all three variables from
`ANDROID_NDK_HOME` and is what the Gradle build uses; the exports above are for
running it by hand.
