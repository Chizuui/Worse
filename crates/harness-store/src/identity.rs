//! Port of `isRemoteWorkspaceIdentity` (`packages/shared/src/remote-workspace-identity.ts`).

const PREFIX: &str = "remote:";

/// Mandatory authority segments after the kind: ssh = host/port/username, others = one.
fn authority_segments(kind: &str) -> Option<usize> {
    match kind {
        "ssh" => Some(3),
        "wsl" | "docker" | "server" => Some(1),
        _ => None,
    }
}

/// Whether `identity` parses as a remote workspace identity.
#[must_use]
pub fn is_remote_workspace_identity(identity: &str) -> bool {
    let Some(rest) = identity.strip_prefix(PREFIX) else {
        return false;
    };
    let Some(kind_end) = rest.find(':') else {
        return false;
    };
    if kind_end == 0 {
        return false;
    }
    let kind = &rest[..kind_end];
    let Some(segments) = authority_segments(kind) else {
        return false;
    };
    let mut cursor = kind_end + 1;
    for index in 0..segments {
        // 与 TS 一致：SSH 第三段（用户名）以绝对路径前的 ":/" 收口，用户名可含冒号。
        let separator = if kind == "ssh" && index == 2 {
            ":/"
        } else {
            ":"
        };
        let Some(offset) = rest.get(cursor..).and_then(|tail| tail.find(separator)) else {
            return false;
        };
        if offset == 0 {
            return false;
        }
        cursor += offset + 1;
    }
    if kind == "wsl" && !rest[cursor..].starts_with('/') {
        let Some(offset) = rest[cursor..].find(':') else {
            return false;
        };
        if offset == 0 {
            return false;
        }
        cursor += offset + 1;
    }
    rest.get(cursor..).is_some_and(|path| path.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::is_remote_workspace_identity as remote;

    #[test]
    fn matches_ts_parser() {
        assert!(remote("remote:ssh:host:22:user:/srv/app"));
        assert!(remote("remote:ssh:host:22:us:er:/srv"));
        assert!(remote("remote:wsl:Ubuntu:/home/a"));
        assert!(remote("remote:wsl:Ubuntu:me:/home/a"));
        assert!(remote("remote:docker:c1:/w"));
        assert!(remote("remote:server:s-1:/w"));
        assert!(!remote("remote:ftp:x:/w"));
        assert!(!remote("remote:docker::/w"));
        assert!(!remote("remote:docker:c1:w"));
        assert!(!remote("C:\\work"));
        assert!(!remote("remote:ssh:host:22:/srv"));
    }
}
