use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=assets/periphhub.ico");

    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    let file_version = format!(
        "{},{},{},0",
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0)
    );

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest_dir
        .join("assets")
        .join("periphhub.ico")
        .display()
        .to_string()
        .replace('\\', "\\\\");

    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {file_version}
PRODUCTVERSION {file_version}
FILEOS 0x4
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "qiuyongjun"
      VALUE "FileDescription", "PeriphHub"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "PeriphHub"
      VALUE "LegalCopyright", "Copyright (C) 2026 qiuyongjun"
      VALUE "OriginalFilename", "PeriphHub.exe"
      VALUE "ProductName", "PeriphHub"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("periphhub.rc");
    std::fs::write(&out, rc).expect("write generated rc");

    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .expect("embed Windows resources");
}
