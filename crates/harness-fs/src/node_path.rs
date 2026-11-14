//! Node `path.join` / `path.normalize` semantics for result paths.
//!
//! The TS service builds child paths with `node:path.join`, so a drop-in Rust
//! implementation must produce byte-identical strings. Only the subset used by
//! the services is implemented: separator normalisation, duplicate separator
//! collapse, `.`/`..` resolution, drive and UNC roots.

/// Platform separator used by Node on this OS.
const SEP: char = if cfg!(windows) { '\\' } else { '/' };

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// Splits a path into its root (`C:\`, `\\server\share\`, `/`, `C:` or empty)
/// and the remainder.
fn split_root(path: &str) -> (String, &str) {
    if cfg!(windows) {
        let bytes = path.as_bytes();
        // UNC: \\server\share
        if bytes.len() >= 2 && is_sep(char::from(bytes[0])) && is_sep(char::from(bytes[1])) {
            let rest = &path[2..];
            let mut parts = rest.splitn(3, is_sep);
            let server = parts.next().unwrap_or_default();
            let share = parts.next();
            if let (false, Some(share)) = (server.is_empty(), share) {
                if !share.is_empty() {
                    let consumed = 2 + server.len() + 1 + share.len();
                    let remainder = path.get(consumed..).unwrap_or_default();
                    return (format!("\\\\{server}\\{share}\\"), remainder);
                }
            }
        }
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            let drive = &path[..2];
            let rest = &path[2..];
            if rest.starts_with(is_sep) {
                return (format!("{drive}\\"), rest);
            }
            return (drive.to_owned(), rest);
        }
        if path.starts_with(is_sep) {
            return ("\\".to_owned(), path);
        }
        (String::new(), path)
    } else if path.starts_with('/') {
        ("/".to_owned(), path)
    } else {
        (String::new(), path)
    }
}

/// Equivalent of Node's `path.normalize` for the current platform.
#[must_use]
pub fn node_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let (root, rest) = split_root(path);
    let is_absolute = root.ends_with(SEP) || root == "/";
    let trailing = rest.ends_with(is_sep) && !rest.trim_matches(is_sep).is_empty();
    let mut stack: Vec<&str> = Vec::new();
    for part in rest.split(is_sep) {
        match part {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|last| *last != "..") {
                    stack.pop();
                } else if !is_absolute {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let body = stack.join(&SEP.to_string());
    let mut out = root;
    out.push_str(&body);
    if out.is_empty() {
        return ".".to_owned();
    }
    if trailing && !body.is_empty() {
        out.push(SEP);
    }
    out
}

/// Equivalent of Node's `path.join(base, name)` for the current platform.
#[must_use]
pub fn node_join(base: &str, name: &str) -> String {
    let joined = match (base.is_empty(), name.is_empty()) {
        (true, true) => return ".".to_owned(),
        (true, false) => name.to_owned(),
        (false, true) => base.to_owned(),
        (false, false) => format!("{base}{SEP}{name}"),
    };
    node_normalize(&joined)
}
