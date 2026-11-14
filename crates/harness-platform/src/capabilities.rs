//! Capability advertisement per deployment surface.

use harness_core::Surface;
use harness_protocol::Capabilities;

/// Returns the capabilities the frontend may rely on for `surface`.
///
/// Mobile shells never spawn a local host and do not expose native file
/// pickers or path reveal; they still open external URLs and receive
/// notifications.
#[must_use]
pub const fn capabilities_for(surface: Surface) -> Capabilities {
    match surface {
        Surface::Desktop => Capabilities {
            open_external: true,
            reveal_path: true,
            select_directory: true,
            select_file: true,
            local_host: true,
            notifications: true,
        },
        Surface::Mobile => Capabilities {
            open_external: true,
            reveal_path: false,
            select_directory: false,
            select_file: false,
            local_host: false,
            notifications: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::capabilities_for;
    use harness_core::Surface;

    #[test]
    fn desktop_exposes_every_capability() {
        let capabilities = capabilities_for(Surface::Desktop);

        assert!(capabilities.open_external);
        assert!(capabilities.reveal_path);
        assert!(capabilities.select_directory);
        assert!(capabilities.select_file);
        assert!(capabilities.local_host);
        assert!(capabilities.notifications);
    }

    #[test]
    fn mobile_is_a_remote_control_client_only() {
        let capabilities = capabilities_for(Surface::Mobile);

        assert!(capabilities.open_external);
        assert!(!capabilities.reveal_path);
        assert!(!capabilities.select_directory);
        assert!(!capabilities.select_file);
        assert!(!capabilities.local_host);
        assert!(capabilities.notifications);
    }
}
