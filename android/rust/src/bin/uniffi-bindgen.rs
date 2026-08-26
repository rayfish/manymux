//! Writes the Kotlin that calls this library.
//!
//! Run by the Gradle build, and by hand when the shape of the boundary
//! changes. It reads the compiled library rather than the source, so what the
//! app is handed and what the app links are the same thing by construction.

fn main() {
    uniffi::uniffi_bindgen_main()
}
