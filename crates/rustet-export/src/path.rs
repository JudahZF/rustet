//! Lexical path handling with Go `path/filepath` semantics.
//!
//! Export paths are built from request strings (output folder, filename template, tool paths),
//! and their safety checks are lexical: they must not depend on what exists on disk. `std::path`
//! is not lexical (`Path::join` with an absolute path replaces the base, `..` is kept), so this
//! module provides the few operations the checks need, on strings.

/// The path separator written by [`clean`].
pub(crate) const SEP: char = std::path::MAIN_SEPARATOR;

/// True for a character that separates path elements on this platform. Windows takes both.
pub(crate) fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// The volume prefix (`C:`) of a Windows path, else empty.
fn volume(p: &str) -> &str {
    let b = p.as_bytes();
    if cfg!(windows) && b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        &p[..2]
    } else {
        ""
    }
}

/// True for an absolute path.
pub(crate) fn is_abs(p: &str) -> bool {
    if cfg!(windows) {
        std::path::Path::new(p).is_absolute()
    } else {
        p.starts_with('/')
    }
}

/// Go's `filepath.Clean`: the shortest lexically equivalent path. Removes `.` elements and
/// repeated separators, resolves `..` against the preceding element (and drops it at the root),
/// and returns `.` for an empty result.
pub(crate) fn clean(p: &str) -> String {
    let vol = volume(p);
    let rest = &p[vol.len()..];
    let rooted = rest.starts_with(is_sep);
    let mut parts: Vec<&str> = Vec::new();
    for elem in rest.split(is_sep) {
        match elem {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if rooted => {}
                _ => parts.push(".."),
            },
            e => parts.push(e),
        }
    }
    let mut out = String::with_capacity(p.len());
    out.push_str(vol);
    if rooted {
        out.push(SEP);
    }
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(SEP);
        }
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

/// Go's `filepath.Join(base, rel)`: concatenation, then [`clean`]. Unlike `Path::join`, an
/// absolute `rel` does not replace `base`.
pub(crate) fn join(base: &str, rel: &str) -> String {
    match (base.is_empty(), rel.is_empty()) {
        (true, _) => clean(rel),
        (_, true) => clean(base),
        _ => clean(&format!("{base}{SEP}{rel}")),
    }
}

/// Go's `filepath.Base`: the last element, ignoring trailing separators.
pub(crate) fn base(p: &str) -> &str {
    let p = &p[volume(p).len()..];
    let trimmed = p.trim_end_matches(is_sep);
    if trimmed.is_empty() {
        return if p.is_empty() { "." } else { "/" };
    }
    trimmed.rsplit(is_sep).next().unwrap_or(trimmed)
}

/// Go's `filepath.Ext`: the suffix from the last `.` of the last element, or empty.
pub(crate) fn ext(p: &str) -> &str {
    for (i, c) in p.char_indices().rev() {
        if is_sep(c) {
            break;
        }
        if c == '.' {
            return &p[i..];
        }
    }
    ""
}

/// True when `p` is `root` or lies beneath it, on a path-element boundary. Both must be clean.
pub(crate) fn within(p: &str, root: &str) -> bool {
    std::path::Path::new(p).starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn clean_matches_go() {
        for (input, want) in [
            ("", "."),
            (".", "."),
            ("a/b/../c", "a/c"),
            ("../../x", "../../x"),
            ("a/../../x", "../x"),
            ("/../x", "/x"),
            ("//a//b/./", "/a/b"),
            ("/", "/"),
            ("a/..", "."),
        ] {
            assert_eq!(clean(input), want, "clean({input:?})");
        }
    }

    #[cfg(unix)]
    #[test]
    fn join_never_replaces_the_base() {
        assert_eq!(join("/out", "/etc/passwd"), "/out/etc/passwd");
        assert_eq!(join("/out", "../x"), "/x");
        assert_eq!(join("/out", "a/b.m4a"), "/out/a/b.m4a");
    }

    #[test]
    fn base_and_ext() {
        assert_eq!(base("/usr/bin/ffmpeg"), "ffmpeg");
        assert_eq!(base("ffmpeg/"), "ffmpeg");
        assert_eq!(ext("/a/Mr. Brightside.m4a"), ".m4a");
        assert_eq!(ext("/a.b/c"), "");
        assert_eq!(ext("x."), ".");
    }
}
