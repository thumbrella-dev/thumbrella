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

    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "f3d",
        category: "geometry",
        command: "f3d",
        // F3D readers available in this runtime (mesh/CAD/simulation formats).
        // USDZ is NOT in this list - it is handled separately via ZIP extraction.
        extensions: &[
            "3ds", "brep", "dae", "dxf", "e", "exo", "ex2", "fbx", "glb", "gltf", "gml", "iges", "igs",
            "obj", "off", "p21", "ply", "pts", "step", "stl", "stp", "stpnc", "vtk", "vtm", "vti", "vtp",
            "vtr", "vts", "vtu", "vrml", "wrl", "210",
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
        extensions: &["usdz", "usdc", "usda"],
        description: "USDZ/USD geometry (usd-core extract → F3D render)",
    });

    tier3::env_check::register_handler(tier3::env_check::HandlerDecl {
        name: "pdftoppm",
        category: "document",
        command: "pdftoppm",
        extensions: &["pdf"],
        description: "PDF renderer (poppler pdftoppm)",
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
        let handlers = tier3::env_check::registered_handlers();
        let mut available: std::collections::HashSet<String> = std::collections::HashSet::new();
        for h in &handlers {
            let backend_ok = env_report.backends.get(h.name).map(|b| b.available).unwrap_or(false);
            if backend_ok {
                for ext in h.extensions {
                    available.insert(ext.to_string());
                }
            }
            // usdz handler: also depends on python3 + usd-core + f3d
            if h.name == "usdz" {
                let f3d_ok = env_report.backends.get("f3d").map(|b| b.available).unwrap_or(false);
                let py_ok = env_report.backends.get("python3").map(|b| b.available).unwrap_or(false);
                let usd_ok = env_report
                    .backends
                    .get("python3")
                    .and_then(|b| b.details.as_deref())
                    .map(|d| d.contains("usd-core available"))
                    .unwrap_or(false);
                if f3d_ok && py_ok && usd_ok {
                    for ext in h.extensions {
                        available.insert(ext.to_string());
                    }
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

    tier1::cli::run_with_hook(3, |rt| async move {
        let rt = if tier3_env.is_none() {
            tier1::with_renderer(rt, tier3::Tier3Renderer::shared())
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
                // Find the handler for this format.
                let handler = tier3::env_check::registered_handlers()
                    .into_iter()
                    .find(|h| h.extensions.contains(&f.extension));
                let (status, detail) = match handler {
                    Some(ref h) => {
                        // The usdz handler depends on both f3d and python3+usd-core.
                        // Check both before reporting as available.
                        if h.name == "usdz" {
                            let f3d_ok = env.backends.get("f3d").map(|b| b.available).unwrap_or(false);
                            let py_ok = env.backends.get("python3").map(|b| b.available).unwrap_or(false);
                            let usd_ok = env
                                .backends
                                .get("python3")
                                .and_then(|b| b.details.as_deref())
                                .map(|d| d.contains("usd-core available"))
                                .unwrap_or(false);
                            if f3d_ok && py_ok && usd_ok {
                                ("available".into(), "f3d + python3/usd-core".to_string())
                            } else {
                                let mut missing = Vec::new();
                                if !f3d_ok {
                                    missing.push("f3d");
                                }
                                if !py_ok {
                                    missing.push("python3");
                                } else if !usd_ok {
                                    missing.push("usd-core");
                                }
                                ("missing".into(), format!("requires: {}", missing.join(", ")))
                            }
                        } else {
                            let available = env.backends.get(h.name).map(|b| b.available).unwrap_or(false);
                            if available {
                                ("available".into(), h.command.to_string())
                            } else {
                                let reason = env
                                    .backends
                                    .get(h.name)
                                    .and_then(|b| b.unavailable_reason.clone())
                                    .unwrap_or_else(|| "not found".into());
                                ("missing".into(), reason)
                            }
                        }
                    }
                    None => ("unregistered".into(), String::new()),
                };
                tier1::check::CheckEntry {
                    label: format!("{:<6} {}", f.extension, f.label),
                    status,
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
        let general: &[&str] =
            &["f3d", "python3", "ffmpeg_cli", "magick", "oiiotool", "display_server"];
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
