//! Symbol claims for subagent coordination (`docs/specs/harness/symbol-graph.md`): a symbol id maps to the
//! workspace-relative files the existing path claims already enforce.

use std::fmt;

/// Languages Kythe can index today; `Cxx` also covers `cpp` and `cc` aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolLanguage {
    Cxx,
    Go,
    Java,
}

impl SymbolLanguage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cxx => "cxx",
            Self::Go => "go",
            Self::Java => "java",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cxx" | "cpp" | "cc" | "c++" => Some(Self::Cxx),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            _ => None,
        }
    }
}

impl fmt::Display for SymbolLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `<language>:<workspace-relative file>#<qualified name>`, already normalized.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId {
    pub language: SymbolLanguage,
    /// Normalized workspace-relative file (forward slashes, no `.`/`..`, lower case on Windows).
    pub file: String,
    pub qualified: String,
}

impl SymbolId {
    /// Parses and normalizes `raw`; the file part goes through the same rules as a path claim.
    #[allow(clippy::missing_errors_doc)]
    pub fn parse(raw: &str) -> Result<Self, String> {
        let text = raw.trim();
        let (language, rest) = text
            .split_once(':')
            .ok_or_else(|| format!("{text}: expected <language>:<file>#<qualified name>"))?;
        let Some(language) = SymbolLanguage::parse(language) else {
            return Err(format!(
                "{language}: unknown symbol language (use cxx, go or java)"
            ));
        };
        let (file, qualified) = rest
            .split_once('#')
            .ok_or_else(|| format!("{text}: expected <language>:<file>#<qualified name>"))?;
        let qualified = qualified.trim();
        if qualified.is_empty() {
            return Err(format!("{text}: the symbol name after `#` is empty"));
        }
        let file = crate::delegate_coord::normalize(file)?;
        if file.is_empty() {
            return Err(format!("{text}: the file before `#` is empty"));
        }
        Ok(Self {
            language,
            file,
            qualified: qualified.to_owned(),
        })
    }
}

impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            language,
            file,
            qualified,
        } = self;
        write!(f, "{language}:{file}#{qualified}")
    }
}

/// One claim request: a symbol plus the extra files the caller names (declaration/definition split).
#[derive(Debug, Clone, Copy)]
pub struct SymbolClaim<'a> {
    pub symbol: &'a str,
    pub related: &'a [String],
}

/// Files a set of symbol claims covers, in first-seen order, plus the ids that could not be mapped.
#[must_use]
pub fn claim_paths(claims: &[SymbolClaim<'_>]) -> (Vec<String>, Vec<(String, String)>) {
    let mut files = Vec::new();
    let mut rejected = Vec::new();
    let mut push = |file: String| {
        if !files.contains(&file) {
            files.push(file);
        }
    };
    for claim in claims {
        match SymbolId::parse(claim.symbol) {
            Ok(symbol) => push(symbol.file),
            Err(reason) => {
                rejected.push((claim.symbol.to_owned(), reason));
                continue;
            }
        }
        for related in claim.related {
            match crate::delegate_coord::normalize(related) {
                Ok(file) if !file.is_empty() => push(file),
                Ok(_) => rejected.push((
                    claim.symbol.to_owned(),
                    format!("{related}: the related file is empty"),
                )),
                Err(reason) => rejected.push((claim.symbol.to_owned(), reason)),
            }
        }
    }
    (files, rejected)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parses_the_three_kythe_languages_and_cxx_aliases() {
        let go = SymbolId::parse("go:internal/server/server.go#Server.Start").unwrap();
        assert_eq!(go.language, SymbolLanguage::Go);
        assert_eq!(go.file, "internal/server/server.go");
        assert_eq!(go.qualified, "Server.Start");
        for alias in ["cpp", "cc", "CXX", "c++"] {
            let id = SymbolId::parse(&format!("{alias}:src/a.cc#ns::A::run")).unwrap();
            assert_eq!(id.language, SymbolLanguage::Cxx, "{alias}");
            assert_eq!(id.to_string(), "cxx:src/a.cc#ns::A::run");
        }
        let java = SymbolId::parse(" java:src/Store.java#Store#put ").unwrap();
        assert_eq!(java.language, SymbolLanguage::Java);
        assert_eq!(java.qualified, "Store#put");
    }

    #[test]
    fn file_part_uses_the_path_claim_rules() {
        let id = SymbolId::parse(r"go:.\internal/./server\server.go#Serve").unwrap();
        assert_eq!(id.file, "internal/server/server.go");
        assert!(SymbolId::parse("go:C:/src/a.go#A").is_err());
        assert!(SymbolId::parse("go:/abs/a.go#A").is_err());
        assert!(SymbolId::parse("go:../up/a.go#A").is_err());
    }

    #[test]
    fn rejects_incomplete_or_unknown_ids() {
        assert!(SymbolId::parse("go:src/a.go").is_err());
        assert!(SymbolId::parse("python:src/a.py#A").is_err());
        assert!(SymbolId::parse("src/a.go#A").is_err());
        assert!(SymbolId::parse("go:src/a.go#").is_err());
        assert!(SymbolId::parse("go:#A").is_err());
    }

    #[test]
    fn claims_map_to_deduped_files_and_keep_the_reason_of_bad_ids() {
        let related = vec![String::from("internal/server/z.go")];
        let claims = [
            SymbolClaim {
                symbol: "go:internal/server/server.go#Server.Start",
                related: &related,
            },
            SymbolClaim {
                symbol: "go:./internal/server/server.go#Server.Stop",
                related: &[],
            },
            SymbolClaim {
                symbol: "nonsense",
                related: &[],
            },
        ];
        let (files, rejected) = claim_paths(&claims);
        assert_eq!(
            files,
            vec!["internal/server/server.go", "internal/server/z.go"]
        );
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].0, "nonsense");
        assert!(rejected[0].1.contains("expected"));
    }

    #[test]
    fn a_symbol_claim_and_a_path_claim_see_the_same_file() {
        let (files, rejected) = claim_paths(&[SymbolClaim {
            symbol: "go:Internal/Server/server.go#Serve",
            related: &[],
        }]);
        assert_eq!(rejected.len(), 0);
        let path = crate::delegate_coord::normalize("internal/server/server.go").unwrap();
        assert_eq!(files, vec![path]);
    }
}
