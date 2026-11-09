//! Blender command construction.
//!
//! Shared by the startup probe ([`crate::env_check`]) and the render path so
//! the security-relevant flags and environment containment cannot drift apart.
//!
//! # Restrictions
//!
//! Blender is invoked with:
//!
//! - `--background` - no UI event loop.
//! - `--factory-startup` - ignore any user `startup.blend`.
//! - `--disable-autoexec` - never run scripts/drivers embedded in .blend files.
//! - `--offline-mode` - deny network access.
//! - `--disable-crash-handler` - no crash log/strace collection.
//! - `-noaudio` - force the sound system to `None`.
//! - `--quiet` - suppress status noise (our marker prints still reach stdout).
//! - `--gpu-backend opengl` - skip Vulkan probing; deterministic with llvmpipe.
//!
//! Environment containment sends `HOME`, `TMPDIR`, XDG dirs, and Blender's
//! user resources to subdirectories of the scratch arena, so a render cannot
//! read or write the service user's real home directory.  Python user-site
//! packages and bytecode caches are disabled, and the Mesa shader cache is
//! turned off (it is a write-only startup cost for single-shot renders).

use std::path::Path;
use std::process::Command;

use crate::scratch::ScratchArena;

/// Common Blender arguments shared by probe and render invocations.
///
/// They are placed before `--python` because Blender processes arguments in
/// order; options that affect the Python session must already be active when
/// the script starts.
const COMMON_ARGS: &[&str] = &[
    "--background",
    "--factory-startup",
    "--disable-autoexec",
    "--offline-mode",
    "--disable-crash-handler",
    "-noaudio",
    "--quiet",
    "--gpu-backend",
    "opengl",
    "--threads",
    "8",
    "--python-exit-code",
    "1",
    "--python",
];

/// Xvfb screen geometry used when no external display is present.
///
/// Blender renders at 384x384, so 800x600 leaves enough slack for driver and
/// window overhead while keeping the Xvfb shadow framebuffer small.
const XVFB_SCREEN: &str = "-screen 0 800x600x24";

/// Build a Blender command that runs `script` inside `arena`.
///
/// Any arguments appended by the caller follow the script path and must be
/// introduced with `--` to reach the script unchanged (for example
/// `-- render INPUT OUTPUT EXTENSION`).
///
/// When `DISPLAY` is unset the command is wrapped in `xvfb-run`.  The caller
/// is still responsible for attaching `stdout`/`stderr` and running the
/// command through [`crate::sandbox`].
pub(crate) fn command(script: &Path, arena: &ScratchArena) -> std::io::Result<Command> {
    let cmd = if std::env::var_os("DISPLAY").is_some() {
        Command::new("blender")
    } else {
        let mut cmd = Command::new("xvfb-run");
        cmd.args(["-a", "-s", XVFB_SCREEN, "blender"]);
        cmd
    };
    configure(cmd, script, arena)
}

/// Apply the common arguments and environment containment to `cmd`.
fn configure(mut cmd: Command, script: &Path, arena: &ScratchArena) -> std::io::Result<Command> {
    let base = arena.root();
    let home = base.join("blender-home");
    let user = base.join("blender-user");
    let tmp = base.join("blender-tmp");
    for dir in [&home, &user, &tmp] {
        std::fs::create_dir_all(dir)?;
    }

    cmd.args(COMMON_ARGS)
        .arg(script)
        // Software rendering only.  Keep thread counts aligned with
        // `--threads 8` so Blender does not oversubscribe the container.
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("MESA_LOADER_DRIVER_OVERRIDE", "llvmpipe")
        .env("GALLIUM_DRIVER", "llvmpipe")
        .env("LP_NUM_THREADS", "8")
        .env("OMP_NUM_THREADS", "8")
        .env("CUDA_VISIBLE_DEVICES", "")
        // Contain every writable path inside the arena.
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("BLENDER_USER_RESOURCES", &user)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        // Do not import user Python packages or write bytecode caches.
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        // llvmpipe shader caching is a write-only cost for one-shot renders.
        .env("MESA_SHADER_CACHE_DISABLE", "true");

    Ok(cmd)
}
