use std::path::{Path, PathBuf};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = root.join("app.manifest");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
    resources(root);
}

/// Compiles the icon and version info with the Windows SDK's rc.exe and links the .res.
/// Without the SDK the exe still builds, just with no icon or version details.
fn resources(root: &Path) {
    println!("cargo:rerun-if-changed=assets/app.ico");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; building without icon and version info");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let name = std::env::var("CARGO_PKG_NAME").unwrap();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let description = std::env::var("CARGO_PKG_DESCRIPTION").unwrap();
    let nums: Vec<u16> = version
        .split(['.', '-'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let (a, b, c) = (nums[0], nums[1], nums[2]);
    let ico = root.join("assets").join("app.ico");
    let script = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "{name}"
      VALUE "ProductName", "{name}"
      VALUE "Comments", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "{name}.exe"
      VALUE "InternalName", "{name}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico.display().to_string().replace('\\', "\\\\"),
    );
    let rc_file = out.join("app.rc");
    let res = out.join("app.res");
    std::fs::write(&rc_file, script).unwrap();
    let status = std::process::Command::new(&rc)
        .args(["/nologo", "/fo"])
        .arg(&res)
        .arg(&rc_file)
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", res.display()),
        _ => println!("cargo:warning=rc.exe failed; building without icon and version info"),
    }
}

/// The newest x64 rc.exe from the Windows 10/11 SDK.
fn find_rc() -> Option<PathBuf> {
    let base = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("x64").join("rc.exe").is_file())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join("x64").join("rc.exe"))
}
