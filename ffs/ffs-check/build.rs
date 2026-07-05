// Runs BEFORE ffmpeg-sys-next's build script (because ffs-check is a
// build-dependency of ffs-build).  Validates the environment and emits the
// build description string so ffs-check's lib.rs can expose it as a constant.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Tell cargo to re-run this build script when these env vars change
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");

    let build_string = validate();
    println!("cargo:rustc-env=FFS_BUILD_STRING={build_string}");
}

/// Wrap `body` in a prominent banner so it stands out from cargo's own output.
fn banner(body: &str) -> ! {
    let width = body
        .lines()
        .map(|l| l.len())
        .max()
        .unwrap_or(60)
        .max(50);
    let line = "=".repeat(width);
    panic!("\n\n{line}\n{body}\n{line}\n");
}

fn validate() -> String {
    #[cfg(windows)]
    {
        check_msvc();
        check_git();

        if let Ok(dir) = std::env::var("FFMPEG_DIR") {
            let dir = PathBuf::from(&dir);
            check_ffmpeg_libs(&dir, "FFMPEG_DIR", &dir);
            return "custom".to_string();
        }

        if let Ok(root) = std::env::var("VCPKG_ROOT") {
            let installed = PathBuf::from(&root).join("installed").join("x64-windows-static");
            check_ffmpeg_libs(&installed, "VCPKG_ROOT", &PathBuf::from(&root));
            return "bundled-vcpkg".to_string();
        }

        banner(concat!(
            "FFmpeg was not found.\n\n",
            "Run:  powershell -File ffs\\build-windows.ps1\n",
            "  to build a bundled FFmpeg automatically.\n\n",
            "Or set one of these environment variables:\n",
            "  FFMPEG_DIR=<path>    path to a custom FFmpeg build\n",
            "  VCPKG_ROOT=<path>    path to a vcpkg tree with ffmpeg installed",
        ));
    }

    #[cfg(not(windows))]
    {
        if let Ok(dir) = std::env::var("FFMPEG_DIR") {
            let dir = PathBuf::from(&dir);
            check_ffmpeg_libs_unix(&dir, "FFMPEG_DIR", &dir);
            return "custom".to_string();
        }

        banner(concat!(
            "FFmpeg was not found.\n\n",
            "Run:  ./ffs/build-linux.sh\n",
            "  to build a bundled FFmpeg automatically.\n\n",
            "Or set:\n",
            "  FFMPEG_DIR=<path>    path to a custom FFmpeg build",
        ));
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn check_msvc() {
    let vs_paths = [
        r"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC",
        r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\MSVC",
        r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\MSVC",
    ];
    let found = vs_paths.iter().any(|p| std::path::Path::new(p).exists());
    if !found {
        banner(concat!(
            "MSVC Build Tools (Visual Studio 2022) were not found.\n\n",
            "Install:  winget install Microsoft.VisualStudio.2022.BuildTools \\\n",
            "  --override \"--wait --add Microsoft.VisualStudio.Workload.VCTools\"\n\n",
            "Then open a new terminal and try again.",
        ));
    }
}

fn check_git() {
    let output = Command::new("git").arg("--version").output();
    match output {
        Ok(o) if o.status.success() => {}
        _ => banner(concat!(
            "Git was not found on this system.\n\n",
            "Install:  winget install Git.Git\n\n",
            "Then open a new terminal and try again.",
        )),
    }
}

#[cfg(windows)]
fn check_ffmpeg_libs(dir: &std::path::Path, env_var: &str, env_value: &std::path::Path) {
    let required = [
        "avcodec.lib", "avdevice.lib", "avfilter.lib", "avformat.lib",
        "avutil.lib", "swresample.lib", "swscale.lib",
    ];
    let lib_dir = dir.join("lib");
    if !lib_dir.exists() {
        banner(&format!(
            "{env_var} is set to\n  {}\n\
             but that is not a valid FFmpeg install location \
             (no \"lib\" directory found).\n\n\
             Either set {env_var} to a valid FFmpeg build, \
             or unset it and run:  powershell -File ffs\\build-windows.ps1",
            env_value.display(),
        ));
    }
    let mut missing = Vec::new();
    for lib in &required {
        if !lib_dir.join(lib).exists() {
            missing.push(*lib);
        }
    }
    if !missing.is_empty() {
        banner(&format!(
            "{env_var} points to\n  {}\n\
             but the FFmpeg build there is incomplete.\n\n\
             Missing libraries:\n  {}\n\n\
             Either rebuild FFmpeg at that location, \
             or unset {env_var} and run:  powershell -File ffs\\build.ps1",
            env_value.display(),
            missing.join("\n  "),
        ));
    }
}

#[cfg(not(windows))]
fn check_ffmpeg_libs_unix(dir: &std::path::Path, env_var: &str, env_value: &std::path::Path) {
    let required = [
        "libavcodec.a", "libavdevice.a", "libavfilter.a", "libavformat.a",
        "libavutil.a", "libswresample.a", "libswscale.a",
    ];
    let lib_dir = dir.join("lib");
    if !lib_dir.exists() {
        banner(&format!(
            "{env_var} is set to\n  {}\n\
             but that is not a valid FFmpeg install location \
             (no \"lib\" directory found).\n\n\
             Either set {env_var} to a valid FFmpeg build, \
             or run:  ./ffs/build-linux.sh",
            env_value.display(),
        ));
    }
    let mut missing = Vec::new();
    for lib in &required {
        if !lib_dir.join(lib).exists() {
            missing.push(*lib);
        }
    }
    if !missing.is_empty() {
        banner(&format!(
            "{env_var} points to\n  {}\n\
             but the FFmpeg build there is incomplete.\n\n\
             Missing libraries:\n  {}\n\n\
             Either rebuild FFmpeg at that location, \
             or run:  ./ffs/build-linux.sh",
            env_value.display(),
            missing.join("\n  "),
        ));
    }
}
