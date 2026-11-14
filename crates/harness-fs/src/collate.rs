//! `String.prototype.localeCompare` parity via ICU4X.
//!
//! The TS services sort directory listings and the workspace index with
//! `localeCompare` (ICU root collation, tertiary strength in Node). The Rust
//! Host uses the same CLDR root collation through `icu_collator`, so orderings
//! are drop-in compatible.

use std::cmp::Ordering;
use std::sync::OnceLock;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences};

fn collator() -> Option<&'static CollatorBorrowed<'static>> {
    static COLLATOR: OnceLock<Option<CollatorBorrowed<'static>>> = OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            Collator::try_new(CollatorPreferences::default(), CollatorOptions::default()).ok()
        })
        .as_ref()
}

/// Compares two strings like `a.localeCompare(b)` under ICU root collation.
/// Falls back to code-point order if the collator data failed to load.
#[must_use]
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    match collator() {
        Some(c) => c.compare(a, b),
        None => a.cmp(b),
    }
}
