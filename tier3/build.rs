//! Embeds Windows version metadata and an application manifest into
//! thumbrella.exe. A bare Rust binary ships neither a version resource nor a
//! supportedOS manifest, which makes AV scanners and SmartScreen treat it as
//! more suspicious than a normal Windows app. The version strings are derived
//! from CARGO_PKG_VERSION, so they track the workspace version automatically.

#[cfg(windows)]
fn main() {
    let mut res = winres::WindowsResource::new();
    res.set_manifest(include_str!("thumbrella.manifest"));

    res.set("FileDescription", "Thumbrella server");
    res.set("ProductName", "Thumbrella");
    res.set("CompanyName", "Thumbrella");
    res.set("LegalCopyright", "Copyright (c) Thumbrella");
    res.set("OriginalFilename", "thumbrella.exe");
    res.set("InternalName", "thumbrella");

    let version = env!("CARGO_PKG_VERSION");
    res.set("FileVersion", version);
    res.set("ProductVersion", version);

    let mut parts = version
        .split(|c: char| c == '.' || c == '-')
        .map(|p| p.parse::<u64>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    let file_version = (major << 48) | (minor << 32) | (patch << 16);
    res.set_version_info(winres::VersionInfo::FILEVERSION, file_version);
    res.set_version_info(winres::VersionInfo::PRODUCTVERSION, file_version);

    if let Err(e) = res.compile() {
        eprintln!("failed to embed Windows resources: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {}
