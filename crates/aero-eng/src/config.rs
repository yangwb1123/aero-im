//! Engineering configuration (`engineering.toml`).
//!
//! Read once at startup; threshold values drive the individual check gates.
//! Falls back to sensible defaults when the file is absent.

use std::path::Path;

use serde::Deserialize;

/// Top-level engineering configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EngineeringConfig {
    pub filesize: FilesizeConfig,
    pub complexity: ComplexityConfig,
    pub check: CheckConfig,
}

impl Default for EngineeringConfig {
    fn default() -> Self {
        Self {
            filesize: FilesizeConfig::default(),
            complexity: ComplexityConfig::default(),
            check: CheckConfig::default(),
        }
    }
}

impl EngineeringConfig {
    /// Load from `engineering.toml` at the project root. Returns defaults if
    /// the file is missing or unparseable.
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let path = root.join("engineering.toml");
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
}

/// File size thresholds.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FilesizeConfig {
    /// Rust source: warn above this line count.
    pub rust_warn: usize,
    /// Rust source: hard-fail above this line count.
    pub rust_hard: usize,
    /// `routes.rs` exemption threshold.
    pub routes_hard: usize,
    /// JavaScript/TypeScript: warn above this line count.
    pub js_warn: usize,
}

impl Default for FilesizeConfig {
    fn default() -> Self {
        Self { rust_warn: 800, rust_hard: 1200, routes_hard: 3000, js_warn: 1000 }
    }
}

/// Complexity thresholds.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ComplexityConfig {
    /// Cyclomatic complexity per function: warn above this.
    pub warn: usize,
    /// Cyclomatic complexity per function: hard-fail above this.
    pub hard: usize,
}

impl Default for ComplexityConfig {
    fn default() -> Self {
        Self { warn: 12, hard: 20 }
    }
}

/// General check flags.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CheckConfig {
    /// Whether to run `cargo clippy` as part of `check`.
    pub clippy: bool,
    /// Whether to run the full test suite as part of `check`.
    pub full_test: bool,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self { clippy: true, full_test: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_reasonable() {
        let cfg = EngineeringConfig::default();
        assert_eq!(cfg.filesize.rust_warn, 800);
        assert_eq!(cfg.filesize.rust_hard, 1200);
        assert_eq!(cfg.complexity.warn, 12);
        assert!(cfg.check.clippy);
    }

    #[test]
    fn load_from_nonexistent_path_returns_defaults() {
        let cfg = EngineeringConfig::load(Path::new("/nonexistent/path"));
        assert_eq!(cfg.filesize.rust_warn, 800);
    }
}
