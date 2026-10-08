//! Expansion for operator-configured filesystem paths, never source URLs.

/// Expand leading `~`, `$VAR`, `${VAR}`, and (on Windows) `%VAR%`.
/// Expansion is single-pass; variable values are literal paths, not expressions.
/// Unset variables and unavailable home directories are configuration errors.
pub fn expand(path: &str) -> Result<String, String> {
    expand_with(
        path,
        cfg!(windows),
        || {
            std::env::home_dir()
                .ok_or_else(|| "cannot determine the current user's home directory".to_string())?
                .into_os_string()
                .into_string()
                .map_err(|_| "home directory is not valid UTF-8".to_string())
        },
        |name| {
            std::env::var(name)
                .map_err(|error| format!("cannot expand environment variable '{name}': {error}"))
        },
    )
}

fn expand_with(
    path: &str,
    windows: bool,
    home: impl FnOnce() -> Result<String, String>,
    mut variable: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = path;
    if path == "~" || path.starts_with("~/") || (windows && path.starts_with("~\\")) {
        let directory = home()?;
        if directory.is_empty() {
            return Err("home directory is empty".into());
        }
        out.push_str(&directory);
        rest = &path[1..];
    }
    let mut index = 0;
    let bytes = rest.as_bytes();
    while index < bytes.len() {
        match bytes[index] {
            b'$' if bytes.get(index + 1) == Some(&b'$') => {
                out.push('$');
                index += 2;
            }
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                let start = index + 2;
                let end = rest[start..]
                    .find('}')
                    .map(|offset| start + offset)
                    .ok_or("unterminated ${VAR} reference in path")?;
                if start == end {
                    return Err("empty environment variable name in path".into());
                }
                out.push_str(&variable(&rest[start..end])?);
                index = end + 1;
            }
            b'$' if bytes.get(index + 1).is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') => {
                let start = index + 1;
                let mut end = start + 1;
                while bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') {
                    end += 1;
                }
                out.push_str(&variable(&rest[start..end])?);
                index = end;
            }
            b'%' if windows && bytes.get(index + 1) == Some(&b'%') => {
                out.push('%');
                index += 2;
            }
            b'%' if windows && rest[index + 1..].contains('%') => {
                let start = index + 1;
                let end = start + rest[start..].find('%').expect("closing percent exists");
                out.push_str(&variable(&rest[start..end])?);
                index = end + 1;
            }
            _ => {
                let ch = rest[index..].chars().next().expect("index is within path");
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    if out.is_empty() || out.contains('\0') {
        return Err("expanded path is empty or contains a NUL character".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(path: &str, windows: bool) -> Result<String, String> {
        expand_with(
            path,
            windows,
            || Ok(if windows { r"C:\Users\test" } else { "/home/test" }.into()),
            |name| match name {
                "DATA" => Ok("/data/a+b,c".into()),
                "USERPROFILE" => Ok(r"C:\Users\test".into()),
                "ProgramFiles(x86)" => Ok(r"C:\Program Files (x86)".into()),
                "LITERAL" => Ok("~/$OTHER/%OTHER%".into()),
                "EMPTY" => Ok(String::new()),
                _ => Err(format!("unset variable '{name}'")),
            },
        )
    }

    #[test]
    fn expands_home_and_environment_without_normalizing_paths() {
        for (input, expected) in [
            ("~", "/home/test"),
            ("~/cache.db", "/home/test/cache.db"),
            ("$DATA/cache.db", "/data/a+b,c/cache.db"),
            ("${DATA}suffix", "/data/a+b,csuffix"),
            ("~/trace-${EMPTY}log", "/home/test/trace-log"),
            ("relative/../cache.db", "relative/../cache.db"),
            (":memory:", ":memory:"),
            ("cost$$5.db", "cost$5.db"),
            ("cost$5.db", "cost$5.db"),
        ] {
            assert_eq!(resolve(input, false).unwrap(), expected);
        }
    }

    #[test]
    fn windows_percent_variables_and_backslash_home_paths_are_supported() {
        for (input, expected) in [
            (r"~\cache.db", r"C:\Users\test\cache.db"),
            (r"%USERPROFILE%\cache.db", r"C:\Users\test\cache.db"),
            (r"%ProgramFiles(x86)%\log.ndjson", r"C:\Program Files (x86)\log.ndjson"),
            (r"${USERPROFILE}\cache.db", r"C:\Users\test\cache.db"),
            (r"cost%%5.db", r"cost%5.db"),
            (r"\\server\share\cache.db", r"\\server\share\cache.db"),
        ] {
            assert_eq!(resolve(input, true).unwrap(), expected);
        }
        assert_eq!(resolve("%USERPROFILE%/cache.db", false).unwrap(), "%USERPROFILE%/cache.db");
        assert_eq!(resolve(r"~\cache.db", false).unwrap(), r"~\cache.db");
    }

    #[test]
    fn substituted_values_are_never_expanded_again() {
        assert_eq!(resolve("$LITERAL/path", true).unwrap(), "~/$OTHER/%OTHER%/path");
        assert_eq!(
            expand_with(
                "~/file",
                false,
                || Ok("/home/$USER".into()),
                |_| Err("unexpected lookup".into())
            )
            .unwrap(),
            "/home/$USER/file"
        );
        assert_eq!(resolve("日本語/${DATA}/é.db", false).unwrap(), "日本語//data/a+b,c/é.db");
    }

    #[test]
    fn invalid_or_unresolved_expansions_are_errors() {
        for input in ["$MISSING/file", "${MISSING}", "${", "${}", "$EMPTY", "", "\0"] {
            assert!(resolve(input, false).is_err(), "{input:?}");
        }
        assert!(resolve("%MISSING%/file", true).is_err());
        assert!(expand_with("~/file", false, || Err("missing home".into()), |_| Ok(String::new())).is_err());
        assert!(expand_with("~/file", false, || Ok(String::new()), |_| Ok(String::new())).is_err());
    }
}
