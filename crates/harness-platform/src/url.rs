//! External URL validation.

use crate::PlatformError;
use std::fmt;
use url::Url;

/// An external URL whose scheme is safe to hand to the operating system.
///
/// Only `http`, `https`, and `mailto` URLs are accepted; schemes such as
/// `javascript`, `file`, or `data` are rejected before they reach a native
/// launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedUrl(Url);

impl ValidatedUrl {
    /// Parses `raw` and validates its scheme.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::InvalidInput`] when `raw` is not an absolute
    /// URL or uses a scheme outside the allowlist.
    pub fn parse(raw: &str) -> Result<Self, PlatformError> {
        let url = Url::parse(raw)
            .map_err(|error| PlatformError::InvalidInput(format!("invalid URL: {error}")))?;

        if !is_allowed_scheme(url.scheme()) {
            return Err(PlatformError::InvalidInput(format!(
                "unsupported URL scheme: {}",
                url.scheme()
            )));
        }

        Ok(Self(url))
    }

    /// Returns the normalised URL string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns the parsed URL.
    #[must_use]
    pub fn as_url(&self) -> &Url {
        &self.0
    }
}

impl fmt::Display for ValidatedUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0.as_str())
    }
}

fn is_allowed_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "mailto")
}

#[cfg(test)]
mod tests {
    use super::ValidatedUrl;
    use crate::PlatformError;

    #[test]
    fn accepts_http_https_and_mailto() {
        for raw in [
            "https://example.com/path?q=1",
            "http://example.com",
            "mailto:dev@example.com",
        ] {
            assert!(
                ValidatedUrl::parse(raw).is_ok(),
                "expected {raw} to be accepted"
            );
        }
    }

    #[test]
    fn normalises_scheme_case() {
        let url = ValidatedUrl::parse("HTTPS://Example.com/Path").unwrap();

        assert_eq!(url.as_str(), "https://example.com/Path");
        assert_eq!(url.as_url().scheme(), "https");
    }

    #[test]
    fn rejects_dangerous_and_unknown_schemes() {
        for raw in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/plain,hello",
            "ftp://example.com/file",
            "chrome://settings",
        ] {
            assert!(
                matches!(
                    ValidatedUrl::parse(raw),
                    Err(PlatformError::InvalidInput(_))
                ),
                "expected {raw} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_relative_and_empty_input() {
        for raw in ["", "example.com", "/tmp/file", "//example.com"] {
            assert!(
                matches!(
                    ValidatedUrl::parse(raw),
                    Err(PlatformError::InvalidInput(_))
                ),
                "expected {raw} to be rejected"
            );
        }
    }
}
