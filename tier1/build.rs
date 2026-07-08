// build.rs — placeholder icon generation
//
// Reruns only when tier1/build_placeholders.py is edited.  If Python or the
// required pip packages are absent the build continues using the committed
// JPEG files and emits a cargo warning instead of failing.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    let script = Path::new(&manifest).join("build_placeholders.py");
    let out_dir = Path::new(&manifest).join("assets/placeholders");

    // Rerun only if the generator script itself is edited.
    println!("cargo:rerun-if-changed={}", script.display());

    match Command::new("python3").arg(&script).arg("--out").arg(&out_dir).status() {
        Ok(s) if s.success() => {}
        Ok(s) => println!(
            "cargo:warning=build_placeholders.py exited with {s}; \
             using committed placeholder files"
        ),
        Err(e) => println!(
            "cargo:warning=build_placeholders.py could not run ({e}); \
             using committed placeholder files"
        ),
    }
}
