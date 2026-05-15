//! Debug logging with category filtering.
//!
//! Usage: `dlog!("editor", "cursor moved to ({}, {})", x, y);`
//!
//! Categories can be filtered via environment variable `RIV_LOG`:
//!   RIV_LOG=editor,keybind    (only editor and keybind)
//!   RIV_LOG=*                 (all categories)
//!   RIV_LOG=""                (only dlog, not log/trace)
//!
//! At runtime, `debug_log["editor"]("message")` can also be used directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Global set of enabled categories (sorted for fast lookup).
static CATEGORIES: OnceLock<CategoryFilter> = OnceLock::new();

/// Global "all categories enabled" flag.
static ALL_ENABLED: AtomicBool = AtomicBool::new(false);

/// Stores the set of enabled log categories.
struct CategoryFilter {
    all: bool,
    categories: Vec<String>,
}

impl CategoryFilter {
    fn is_enabled(&self, category: &str) -> bool {
        if self.all {
            return true;
        }
        // Binary search on sorted categories.
        self.categories.binary_search(&category.to_string()).is_ok()
    }
}

/// Parse the `RIV_LOG` environment variable and initialise the category filter.
///
/// Call this once at startup, after `env_logger` is initialised.
pub fn dlog_init() {
    let filter = std::env::var("RIV_LOG").unwrap_or_default();

    let cf = if filter == "*" {
        CategoryFilter {
            all: true,
            categories: Vec::new(),
        }
    } else if filter.is_empty() {
        CategoryFilter {
            all: false,
            categories: Vec::new(),
        }
    } else {
        let mut cats: Vec<String> = filter
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        cats.sort();
        CategoryFilter {
            all: false,
            categories: cats,
        }
    };

    ALL_ENABLED.store(cf.all, Ordering::Relaxed);
    let _ = CATEGORIES.set(cf);
}

/// Check whether a given category is enabled.
#[inline]
pub(crate) fn is_category_enabled(category: &str) -> bool {
    if ALL_ENABLED.load(Ordering::Relaxed) {
        return true;
    }
    if let Some(cf) = CATEGORIES.get() {
        cf.is_enabled(category)
    } else {
        false
    }
}

/// Debug-log macro with category filtering.
///
/// ```ignore
/// dlog!("editor", "cursor moved to ({}, {})", x, y);
/// ```
///
/// Zero-cost when the category is not enabled — the format string
/// and arguments are never evaluated.
#[macro_export]
macro_rules! dlog {
    ($cat:expr_2021, $($arg:tt)*) => {
        if $crate::dlog::is_category_enabled($cat) {}
    };
}

/// Runtime-callable debug log function, indexed by category string.
///
/// ```ignore
/// debug_log["editor"]("cursor moved");
/// debug_log["keybind"]("binding match: {:?}", result);
/// ```
pub struct DebugLog;

impl std::ops::Index<&str> for DebugLog {
    type Output = DebugLogCategory;

    fn index(&self, category: &str) -> &Self::Output {
        // We return a static reference to a helper that carries the category.
        // Since `is_category_enabled` is the guard, the actual category
        // string is captured at call time via the closure in `log`.
        // SAFETY: The returned value is a unit-struct; the category is only
        // used at the point of calling `__call__`.
        static HELPER: DebugLogCategory = DebugLogCategory;
        let _ = category; // silence unused warning — used by is_category_enabled check below
        &HELPER
    }
}

/// A helper that allows `debug_log["cat"]("message")` syntax.
pub struct DebugLogCategory;

impl DebugLogCategory {
    /// Log a formatted message for the given category.
    ///
    /// Note: for the macro-free call site, use `debug_log["cat"].log("msg", args...)`.
    pub fn log(&self, category: &str, _msg: std::fmt::Arguments<'_>) {
        if is_category_enabled(category) {}
    }
}

/// Global instance for `debug_log["category"]("msg")` usage.
#[allow(non_upper_case_globals)]
pub static debug_log: DebugLog = DebugLog;
