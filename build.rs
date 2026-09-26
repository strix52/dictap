fn main() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("gemdict.manifest");
    println!("cargo:rerun-if-changed=gemdict.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
