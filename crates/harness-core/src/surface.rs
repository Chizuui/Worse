//! Deployment surface detection.

/// The kind of runtime a build targets.
///
/// Desktop surfaces own a local Host and agent processes; mobile surfaces are
/// remote-control clients only and never spawn local processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Surface {
    /// Windows, macOS, or Linux desktop build.
    Desktop,
    /// Android or iOS remote-control build.
    Mobile,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
impl Surface {
    /// Returns the surface the current build targets.
    #[must_use]
    pub const fn current() -> Self {
        Self::Mobile
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl Surface {
    /// Returns the surface the current build targets.
    #[must_use]
    pub const fn current() -> Self {
        Self::Desktop
    }
}

#[cfg(test)]
mod tests {
    use super::Surface;

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn desktop_build_reports_desktop_surface() {
        assert_eq!(Surface::current(), Surface::Desktop);
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    #[test]
    fn mobile_build_reports_mobile_surface() {
        assert_eq!(Surface::current(), Surface::Mobile);
    }
}
