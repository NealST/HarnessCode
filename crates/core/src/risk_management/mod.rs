//! # Risk Management
//!
//! Before HarnessCode touches any file it passes the path through the
//! [`RiskManager`].  The manager assigns a [`RiskLevel`] and, for high-risk
//! paths, returns an error that the caller must explicitly acknowledge.
//!
//! ## Risk classification (default rules)
//!
//! | Pattern | Level |
//! |---------|-------|
//! | `auth`, `secret`, `password`, `token`, `key`, `credential` in filename | `High` |
//! | `Cargo.toml`, `*.lock`, `.env*`, CI config files | `High` |
//! | `config`, `settings`, `*.yaml`, `*.yml`, `*.json` | `Medium` |
//! | Everything else | `Low` |

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
use tracing::{info, warn};

// ──────────────────────────────────────────────
// Risk level
// ──────────────────────────────────────────────

/// The assessed risk associated with modifying a particular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    /// Low risk — safe to modify without confirmation.
    Low,
    /// Medium risk — log a warning but proceed automatically.
    Medium,
    /// High risk — block the operation; require explicit human confirmation.
    High,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "LOW"),
            RiskLevel::Medium => write!(f, "MEDIUM"),
            RiskLevel::High => write!(f, "HIGH"),
        }
    }
}

// ──────────────────────────────────────────────
// Risk assessment result
// ──────────────────────────────────────────────

/// The complete risk assessment for a single file path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskAssessment {
    /// The file path that was assessed.
    pub filepath: String,
    /// The calculated risk level.
    pub level: RiskLevel,
    /// Human-readable explanation of why this risk level was assigned.
    pub reason: String,
}

// ──────────────────────────────────────────────
// Errors
// ──────────────────────────────────────────────

/// Errors produced by the [`RiskManager`].
#[derive(Debug, Error)]
pub enum RiskError {
    /// The operation was blocked because the file is classified as high risk.
    /// The caller must obtain explicit confirmation before retrying.
    #[error("HIGH RISK detected for '{filepath}': {reason}. Operation blocked.")]
    HighRiskBlocked { filepath: String, reason: String },
}

// ──────────────────────────────────────────────
// RiskManager
// ──────────────────────────────────────────────

/// Evaluates the risk of modifying a given file and either permits or blocks
/// the operation.
///
/// # Example
///
/// ```rust
/// use harnesscode_core::risk_management::RiskManager;
///
/// let rm = RiskManager::default();
///
/// // Low-risk file proceeds silently
/// let assessment = rm.check_file_risk("src/utils.rs").unwrap();
/// assert_eq!(assessment.level, harnesscode_core::risk_management::RiskLevel::Low);
///
/// // High-risk file is blocked
/// assert!(rm.check_file_risk("src/auth.rs").is_err());
/// ```
#[derive(Debug, Default)]
pub struct RiskManager;

impl RiskManager {
    /// Create a new [`RiskManager`] with the default classification rules.
    pub fn new() -> Self {
        Self
    }

    /// Assess the risk of modifying `filepath`.
    ///
    /// * Returns `Ok(RiskAssessment)` for `Low` and `Medium` risk files (with
    ///   a tracing warning emitted for `Medium`).
    /// * Returns `Err(RiskError::HighRiskBlocked)` for `High` risk files.
    pub fn check_file_risk(&self, filepath: &str) -> Result<RiskAssessment, RiskError> {
        let assessment = self.assess(filepath);

        match assessment.level {
            RiskLevel::Low => {
                info!(filepath, "Risk check passed (LOW)");
                Ok(assessment)
            }
            RiskLevel::Medium => {
                warn!(filepath, reason = %assessment.reason, "Risk check warning (MEDIUM)");
                Ok(assessment)
            }
            RiskLevel::High => {
                warn!(filepath, reason = %assessment.reason, "Risk check BLOCKED (HIGH)");
                Err(RiskError::HighRiskBlocked {
                    filepath: assessment.filepath,
                    reason: assessment.reason,
                })
            }
        }
    }

    /// Internal classification logic — pure function with no side-effects.
    fn assess(&self, filepath: &str) -> RiskAssessment {
        let path = Path::new(filepath);
        // Use file name for matching; fall back to the whole path string.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(filepath)
            .to_lowercase();

        // ── High-risk patterns ───────────────────────────────────────────────
        let high_risk_names = [
            "cargo.toml",
            "cargo.lock",
            ".env",
            ".env.local",
            ".env.production",
        ];
        let high_risk_keywords = [
            "auth", "secret", "password", "passwd", "token", "credential", "private_key",
        ];

        if high_risk_names.contains(&name.as_str()) {
            return RiskAssessment {
                filepath: filepath.to_string(),
                level: RiskLevel::High,
                reason: format!("'{name}' is a critical project file"),
            };
        }

        for kw in &high_risk_keywords {
            if name.contains(kw) {
                return RiskAssessment {
                    filepath: filepath.to_string(),
                    level: RiskLevel::High,
                    reason: format!("filename contains sensitive keyword '{kw}'"),
                };
            }
        }

        // ── Medium-risk patterns ─────────────────────────────────────────────
        let medium_risk_suffixes = [".yaml", ".yml", ".json", ".toml", ".ini", ".cfg"];
        let medium_risk_keywords = ["config", "settings", "setup", "deploy"];

        for suffix in &medium_risk_suffixes {
            if name.ends_with(suffix) {
                return RiskAssessment {
                    filepath: filepath.to_string(),
                    level: RiskLevel::Medium,
                    reason: format!("configuration file (suffix '{suffix}')"),
                };
            }
        }
        for kw in &medium_risk_keywords {
            if name.contains(kw) {
                return RiskAssessment {
                    filepath: filepath.to_string(),
                    level: RiskLevel::Medium,
                    reason: format!("filename contains configuration keyword '{kw}'"),
                };
            }
        }

        // ── Default: low risk ────────────────────────────────────────────────
        RiskAssessment {
            filepath: filepath.to_string(),
            level: RiskLevel::Low,
            reason: "no sensitive patterns detected".to_string(),
        }
    }
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rm() -> RiskManager {
        RiskManager::new()
    }

    #[test]
    fn low_risk_for_generic_source_file() {
        let result = rm().check_file_risk("src/utils.rs");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().level, RiskLevel::Low);
    }

    #[test]
    fn high_risk_for_auth_file() {
        let result = rm().check_file_risk("src/auth.rs");
        assert!(result.is_err());
        assert!(matches!(result, Err(RiskError::HighRiskBlocked { .. })));
    }

    #[test]
    fn high_risk_for_cargo_toml() {
        let result = rm().check_file_risk("Cargo.toml");
        assert!(result.is_err());
    }

    #[test]
    fn high_risk_for_env_file() {
        let result = rm().check_file_risk(".env");
        assert!(result.is_err());
    }

    #[test]
    fn medium_risk_for_yaml_config() {
        let result = rm().check_file_risk("deploy.yml");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().level, RiskLevel::Medium);
    }

    #[test]
    fn medium_risk_for_config_keyword() {
        let result = rm().check_file_risk("src/config.rs");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().level, RiskLevel::Medium);
    }
}
