//! Tier 3 binary - starts tier 1's pipeline with the tier 3 renderer registered.
//!
//! At startup the environment is probed for available backends (F3D, usd-core,
//! etc.).  The `tier3 check` command prints a detailed capability report.
//!
//! When `TBR_TIER2` or `TBR_TIER3` is set, the corresponding in-process
//! renderer is skipped so the server delegates to an external handoff target
//! (e.g. the cloud service) instead.

#[tokio::main]
async fn main() {
    //  The `probe` command must ignore TBR_PROBE and run the real probes.
    if std::env::args().skip(1).any(|a| a == "probe") {
        tier3::env_check::set_force_probe();
    }

    let tier2_env = std::env::var("TBR_TIER2").ok().filter(|s| !s.is_empty());
    let tier3_env = std::env::var("TBR_TIER3").ok().filter(|s| !s.is_empty());

    //  Mark tiers as builtin
    // Tier 3 includes tier 2 functionality.  When the corresponding env var
    // is set we skip the builtin mark so the check command reports Handoff
    // instead of Builtin and the handoff URL is actually used.
    if tier2_env.is_none() {
        tier1::check::mark_tier2_builtin();
    }
    if tier3_env.is_none() {
        tier1::check::mark_tier3_builtin();
    }

    //  Register subprocess handlers
    // These are probed at startup.  Only handlers whose command exists and is
    // executable are marked available in the env report.

    // Blender is preferred for self-contained scenes (blend, abc), USD,
    // glTF/GLB (best material fidelity), and the remaining interchange formats
    // whose importers we want Blender to own (dae, ply).  F3D stays registered
    // as the fallback for every extension it supports.  OBJ, STL, and FBX skip
    // Blender because F3D renders their materials well and starts far faster.
    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "blender",
        category: "geometry",
        command: "blender",
        extensions: &[
            "abc", "blend", "dae", "glb", "gltf", "ply", "usd", "usda", "usdc", "usdz",
        ],
        description: "Blender Workbench software renderer",
    });

    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "f3d",
        category: "geometry",
        command: "f3d",
        // F3D readers available in this runtime (mesh/CAD/simulation formats).
        // Also the fallback for any Blender-handled format when Blender is
        // unavailable or cannot render it.  USDZ is NOT in this list - it is
        // handled separately via ZIP extraction.
        extensions: &[
            "3ds", "brep", "dae", "dxf", "e", "exo", "ex2", "fbx", "glb", "gltf", "gml", "iges", "igs",
            "obj", "off", "p21", "ply", "pts", "step", "stl", "stp", "stpnc", "vtk", "vtm", "vti",
            "vtp", "vtr", "vts", "vtu", "vrml", "wrl", "210",
        ],
        description: "3D geometry renderer (F3D)",
    });

    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "usdz",
        category: "geometry",
        command: "(builtin)",
        // USDZ/USDC/USDA are extracted to OBJ via Python usd-core, then
        // rendered through the F3D handler.  Availability depends on both
        // python3+usd-core and f3d being present at runtime.
        extensions: &["usd", "usdz", "usdc", "usda"],
        description: "USDZ/USD geometry (usd-core extract → F3D render)",
    });

    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "pdftoppm",
        category: "document",
        command: "pdftoppm",
        extensions: &["pdf"],
        description: "PDF renderer (poppler pdftoppm)",
    });

    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: tier3::office::HANDLER_NAME,
        category: "document",
        command: "soffice",
        extensions: tier3::office::OFFICE_EXTENSIONS,
        description: "Office document renderer (LibreOffice headless)",
    });

    // FFmpeg CLI handles FLV video plus every image format that tiers 1/2
    // cannot decode at all.  Image decodes go through
    // `renderer::render_image_ffmpeg_fallback`, which needs both `ffmpeg` and
    // `ffprobe`, so one declaration covers both roles.
    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "ffmpeg_cli",
        category: "video",
        command: "ffmpeg",
        extensions: &["flv", "avif", "dcm", "jp2", "pam", "qoi", "xwd"],
        description: "FLV video and image-only formats (FFmpeg CLI)",
    });

    // Frame/plate image formats oiiotool reads better than FFmpeg.  The
    // extension list is shared with `renderer::is_oiio_format`, which is what
    // actually picks the tool at render time, so the two cannot drift apart.
    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "oiiotool",
        category: "image",
        command: "oiiotool",
        extensions: tier3::renderer::OIIO_IMAGE_EXTENSIONS,
        description: "Frame and plate image formats (OpenImageIO)",
    });

    // HTML pages are rendered by the Playwright CLI, which loads the original
    // URL itself (no staging).  The probe confirms the browser engine works.
    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: tier3::html::HANDLER_NAME,
        category: "document",
        command: "playwright-cli",
        extensions: tier3::html::HTML_EXTENSIONS,
        description: "HTML web page snapshot (playwright-cli)",
    });

    //  Probe the environment
    let env_report = tier3::env_check::probe_environment();

    //  Populate tier 1's tier-3 format-availability registry
    // Collect all extensions from handlers whose backend executable was found
    // at startup.  The fallback dispatch chain in tier 1 uses this to decide
    // whether it should try tier 3 for a given format.
    //
    // When TBR_TIER3 is set we skip this — the external server decides what
    // it can handle; tier3_can_handle() returns true for all extensions when
    // the registry is unpopulated.
    if tier3_env.is_none() {
        let mut available: std::collections::HashSet<String> = std::collections::HashSet::new();
        for h in tier3::env_check::registered_handlers() {
            for ext in h.extensions {
                if !tier3::env_check::available_handlers(&env_report, ext).is_empty() {
                    available.insert(ext.to_string());
                }
            }
        }
        tier1::set_tier3_available_extensions(available);
    }

    let show_all = matches!(std::env::var("TBR_LOG").as_deref(), Ok("full"));
    if show_all {
        eprintln!("[tier3] {}", env_report.summary);
    }

    //  Register check sections
    register_tier3_check(&env_report);

    //  Record the TBR_PROBE value for the `probe` command and `check` output.
    tier1::check::set_probe_envvar(tier3::env_check::probe_envvar(&env_report));

    //  When running local (no TBR_TIER3), report on the external tools:
    //  - `formats` footer lists which tools were found vs missing
    //  - the serve banner gets a one-line cloud nudge when headline tools are missing
    //  - `check` gets a summary (all found, or missing + cloud hint)
    if tier3_env.is_none() {
        let mut found: Vec<&str> = Vec::new();
        let mut missing: Vec<&str> = Vec::new();
        for (name, info) in &env_report.backends {
            if matches!(info.method, tier3::env_check::ProbeMethod::Executable { .. }) {
                if info.available {
                    found.push(name.as_str());
                } else {
                    missing.push(name.as_str());
                }
            }
        }
        found.sort_unstable();
        missing.sort_unstable();

        tier1::check::set_format_tools(
            found.iter().map(|s| s.to_string()).collect(),
            missing.iter().map(|s| s.to_string()).collect(),
        );

        if let Some(tip) = missing_tools_tip(&env_report) {
            tier1::ux::add_startup_tip(tip);
        }

        let notes = if missing.is_empty() {
            vec!["all external formats and tools found".to_string()]
        } else {
            vec![
                "external tools and formats missing, see `thumbrella formats` for details,".to_string(),
                "or set TBR_TIER3 to your Thumbrella Cloud token to enable all formats".to_string(),
            ]
        };
        tier1::check::register_check_notes(notes);
    }

    tier1::cli::run_with_hook(3, |rt| async move {
        let rt = if tier3_env.is_none() {
            tier1::with_renderer(rt, tier3::Tier3Renderer::shared())
        } else if tier2_env.is_none() {
            tier1::with_renderer(rt, tier2::Tier2Renderer::shared())
        } else {
            rt
        };
        if tier2_env.is_none() {
            tier1::with_shortcut_limits(rt, tier1::ShortcutLimits::TIER2)
        } else {
            rt
        }
    })
    .await;
}

/// Representative headline format per external tool that tier 3 needs.
/// Used only for the startup cloud nudge.
const TOOL_FORMAT_EXAMPLES: &[(&str, &str)] =
    &[("oiiotool", "dpx"), ("f3d", "gltf"), ("pdftoppm", "pdf")];

/// Build the one-line cloud nudge: at most three of the missing tools' formats
/// plus `and more` when there are more.  `None` when every tool is present.
fn missing_tools_tip(env: &tier3::env_check::EnvReport) -> Option<String> {
    let missing: Vec<&str> = TOOL_FORMAT_EXAMPLES
        .iter()
        .filter(|(name, _)| env.backends.get(*name).map(|b| !b.available).unwrap_or(false))
        .map(|(_, ext)| *ext)
        .collect();
    if missing.is_empty() {
        return None;
    }

    let shown = &missing[..missing.len().min(3)];
    let mut list = shown.iter().map(|ext| format!(".{ext}")).collect::<Vec<_>>().join(" ");
    if missing.len() > shown.len() {
        list.push_str(" and more");
    }
    Some(format!("install external tools or use TBR_TIER3 to enable {list}"))
}

/// Build tier-3 diagnostic sections from the format manifest and env report.
fn register_tier3_check(env: &tier3::env_check::EnvReport) {
    let manifest = tier1::format_manifest();

    // Tier 2 section: static formats that are always available when tier2
    // is compiled into the binary.
    {
        let tier2_formats: Vec<_> = manifest.iter().filter(|f| f.tier == 2).collect();
        let entries: Vec<tier1::check::CheckEntry> = tier2_formats
            .iter()
            .map(|f| tier1::check::CheckEntry {
                label: format!("{:<6} {}", f.extension, f.label),
                status: "builtin".into(),
                detail: Some(f.renderer.into()),
            })
            .collect();
        tier1::check::register_section(tier1::check::CheckSection {
            heading: format!("Tier 2 - Supported Formats ({} formats)", entries.len()),
            entries,
        });
    }

    // Tier 3 section: formats backed by subprocess handlers, with
    // availability from the env probe.
    {
        let tier3_formats: Vec<_> = manifest.iter().filter(|f| f.tier == 3).collect();
        let entries: Vec<tier1::check::CheckEntry> = tier3_formats
            .iter()
            .map(|f| {
                let handlers = tier3::env_check::registered_handlers();
                let available = tier3::env_check::available_handlers(env, f.extension);
                let (status, detail) = if !available.is_empty() {
                    ("available", available.iter().map(|h| h.name).collect::<Vec<_>>().join(" -> "))
                } else {
                    let candidates: Vec<_> = handlers
                        .iter()
                        .filter(|h| h.extensions.contains(&f.extension))
                        .map(|h| h.name)
                        .collect();
                    if candidates.is_empty() {
                        ("unregistered", String::new())
                    } else {
                        ("missing", format!("requires: {}", candidates.join(" or ")))
                    }
                };
                tier1::check::CheckEntry {
                    label: format!("{:<6} {}", f.extension, f.label),
                    status: status.into(),
                    detail: if detail.is_empty() { None } else { Some(detail) },
                }
            })
            .collect();
        tier1::check::register_section(tier1::check::CheckSection {
            heading: format!("Tier 3 - Subprocess Handlers ({} formats)", entries.len()),
            entries,
        });
    }

    // General tools section - backends not tied to specific extensions.
    {
        let general: &[&str] = &[
            "playwright_cli", "blender", "f3d", "python3", "ffmpeg_cli", "ffprobe", "magick",
            "oiiotool", "display_server",
        ];
        let entries: Vec<tier1::check::CheckEntry> = general
            .iter()
            .filter_map(|name| {
                let info = env.backends.get(*name)?;
                let status = if info.available { "available" } else { "missing" };
                Some(tier1::check::CheckEntry {
                    label: info.name.clone(),
                    status: status.into(),
                    detail: info.details.clone().or_else(|| info.unavailable_reason.clone()),
                })
            })
            .collect();
        tier1::check::register_section(tier1::check::CheckSection {
            heading: "Tier 3 - General Tools".into(),
            entries,
        });
    }
}
