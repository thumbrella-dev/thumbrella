//! Local filesystem paths and their `file://` URL form.
//!
//! Local files are only accepted when `TBR_ALLOW_LOCAL` is set.  A path travels
//! through the pipeline as a `file://` URL and is turned back into a native path
//! when the file is opened, so both directions live here.  The canonical form is
//! `file://`, a `/`, then an absolute path:
//!
//! ```text
//! POSIX      /data/a.png    ->  file:///data/a.png
//! Windows    C:/data/a.png  ->  file:///C:/data/a.png
//!            C:\data\a.png  ->  file:///C:/data/a.png
//! ```
//!
//! On Windows the `/` after the scheme belongs to the URL rather than the path,
//! and [`path_from_file_url`] takes it off again.  On POSIX that very same text
//! is a genuine absolute path (`/C:/data/a.png`), so the platform has to decide.
//!
//! Both directions defer to [`std::path::Path::is_absolute`], which already
//! knows that `C:data` is drive-relative while `C:/data` is not.  It follows
//! that `/data/a.png` counts as relative on Windows, where it names the root of
//! whichever drive the process happens to be on; it is accepted on POSIX only.

use std::path::Path;

/// The path portion of a `file://` URL for an absolute local path.
///
/// Returns `None` for a relative path, which callers reject.  The result always
/// begins with `/`.
pub fn path_for_file_url(path: &str) -> Option<String> {
    if !Path::new(path).is_absolute() {
        return None;
    }

    // Windows accepts either separator in a path but wants `/` in the URL, and a
    // drive path has no leading separator to contribute, so one is added.  A
    // POSIX filename may legally contain a backslash, so nothing is rewritten
    // there.
    #[cfg(windows)]
    let url_path = {
        // UNC (`\\server\share`) is absolute, but its canonical file URL puts the
        // server in the URL's authority (`file://server/share/...`), which is not
        // implemented; refuse rather than emit a URL that would read back as a
        // relative path.
        if path.starts_with(r"\\") || path.starts_with("//") {
            return None;
        }
        format!("/{}", path.replace('\\', "/"))
    };
    #[cfg(not(windows))]
    let url_path = path.to_string();

    Some(url_path)
}

/// Recover the native filesystem path from a `file://` URL.
///
/// Returns `None` when the URL does not use the `file://` scheme.  Both the
/// canonical `file:///C:/data/a.png` and the bare `file://C:/data/a.png` work.
pub fn path_from_file_url(url: &str) -> Option<&str> {
    let path = url.strip_prefix("file://")?;

    // Windows only: in `file:///C:/a.png` the `/` before `C:` belongs to the
    // URL, and Win32 would read what is left as `\C:\a.png` - the root of the
    // current drive - so it has to come off.  On POSIX that same `/C:/a.png` is
    // an absolute path that merely contains a colon, so it must not.
    #[cfg(windows)]
    if let Some(rest) = path.strip_prefix('/')
        && Path::new(rest).is_absolute()
    {
        return Some(rest);
    }

    Some(path)
}

// These exercise the host platform's own rules, which is all a test run can
// reach: CI builds for Windows but does not run tests there, so the Windows
// branches are covered by the reasoning above rather than by these.
#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_become_a_file_url() {
        let url_path = path_for_file_url("/data/a.png").unwrap();
        assert_eq!(url_path, "/data/a.png");
        assert_eq!(format!("file://{url_path}"), "file:///data/a.png");
    }

    #[test]
    fn relative_paths_have_no_file_url() {
        for raw in ["data/a.png", "./a.png", "", ".", "C:a.png"] {
            assert_eq!(path_for_file_url(raw), None, "{raw:?} should be rejected");
        }
    }

    #[test]
    fn a_drive_letter_is_only_meaningful_on_windows() {
        // Nothing about the `C:` is special here, so the path is relative and
        // the Windows-only drive handling must not fire.
        assert_eq!(path_for_file_url("C:/Users/pete/a.png"), None);
    }

    #[test]
    fn a_drive_shaped_url_keeps_its_root_slash_on_posix() {
        // `/C:/a.png` is an absolute POSIX path that happens to contain a colon,
        // and Windows' URL-root rule does not apply to it.
        assert_eq!(path_from_file_url("file:///C:/a.png"), Some("/C:/a.png"));
        assert_eq!(path_from_file_url("file:///data/a.png"), Some("/data/a.png"));
    }

    #[test]
    fn only_file_urls_have_a_path() {
        assert_eq!(path_from_file_url("https://example.com/a.png"), None);
    }
}
