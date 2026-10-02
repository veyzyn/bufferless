fn main() {
    // Embed the manifest with the MSVC linker directly; no resource compiler needed.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bufferless.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
}
