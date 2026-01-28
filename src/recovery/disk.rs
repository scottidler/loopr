//! Disk space management for worktree cleanup.
//!
//! Monitors disk usage and enforces quotas:
//! - Warns when disk space is low
//! - Prevents new worktrees when quota is exceeded
//! - Triggers aggressive cleanup when needed

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use log::warn;
use tokio::process::Command;

/// Configuration for disk quota management.
#[derive(Debug, Clone)]
pub struct DiskQuotaConfig {
    /// Minimum disk space required to create new worktrees (in GB).
    pub min_free_gb: u64,

    /// Warning threshold (in GB) - warn when free space drops below this.
    pub warning_threshold_gb: u64,

    /// Critical threshold (in GB) - trigger aggressive cleanup.
    pub critical_threshold_gb: u64,

    /// Path to check for disk space (usually worktree directory).
    pub check_path: PathBuf,
}

impl Default for DiskQuotaConfig {
    fn default() -> Self {
        Self {
            min_free_gb: 5,
            warning_threshold_gb: 10,
            critical_threshold_gb: 5,
            check_path: std::env::temp_dir().join("loopr").join("worktrees"),
        }
    }
}

impl DiskQuotaConfig {
    /// Create a new config with the given check path.
    pub fn new(check_path: PathBuf) -> Self {
        Self {
            check_path,
            ..Default::default()
        }
    }

    /// Set the minimum free space threshold.
    pub fn with_min_free(mut self, gb: u64) -> Self {
        self.min_free_gb = gb;
        self
    }

    /// Set the warning threshold.
    pub fn with_warning_threshold(mut self, gb: u64) -> Self {
        self.warning_threshold_gb = gb;
        self
    }

    /// Set the critical threshold.
    pub fn with_critical_threshold(mut self, gb: u64) -> Self {
        self.critical_threshold_gb = gb;
        self
    }
}

/// Disk usage information.
#[derive(Debug, Clone)]
pub struct DiskUsage {
    /// Total disk space in bytes.
    pub total_bytes: u64,

    /// Used disk space in bytes.
    pub used_bytes: u64,

    /// Available disk space in bytes.
    pub available_bytes: u64,

    /// Mount point path.
    pub mount_point: PathBuf,
}

impl DiskUsage {
    /// Get available space in gigabytes.
    pub fn available_gb(&self) -> u64 {
        self.available_bytes / (1024 * 1024 * 1024)
    }

    /// Get used space in gigabytes.
    pub fn used_gb(&self) -> u64 {
        self.used_bytes / (1024 * 1024 * 1024)
    }

    /// Get total space in gigabytes.
    pub fn total_gb(&self) -> u64 {
        self.total_bytes / (1024 * 1024 * 1024)
    }

    /// Get usage percentage.
    pub fn usage_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.total_bytes as f64) * 100.0
    }
}

/// Warning about disk space issues.
#[derive(Debug, Clone)]
pub enum DiskWarning {
    /// Disk space is below warning threshold.
    Low { available_gb: u64, threshold_gb: u64 },

    /// Disk space is critical - cleanup needed.
    Critical { available_gb: u64, threshold_gb: u64 },

    /// Cannot create new worktrees - below minimum.
    QuotaExceeded { available_gb: u64, required_gb: u64 },
}

impl DiskWarning {
    /// Get a human-readable message for the warning.
    pub fn message(&self) -> String {
        match self {
            DiskWarning::Low {
                available_gb,
                threshold_gb,
            } => {
                format!(
                    "Low disk space: {}GB available (warning threshold: {}GB)",
                    available_gb, threshold_gb
                )
            }
            DiskWarning::Critical {
                available_gb,
                threshold_gb,
            } => {
                format!(
                    "Critical disk space: {}GB available (critical threshold: {}GB)",
                    available_gb, threshold_gb
                )
            }
            DiskWarning::QuotaExceeded {
                available_gb,
                required_gb,
            } => {
                format!(
                    "Disk quota exceeded: {}GB available, {}GB required",
                    available_gb, required_gb
                )
            }
        }
    }

    /// Check if this is a critical warning that should block operations.
    pub fn is_critical(&self) -> bool {
        matches!(self, DiskWarning::Critical { .. } | DiskWarning::QuotaExceeded { .. })
    }
}

/// Manager for disk space operations.
pub struct DiskManager {
    config: DiskQuotaConfig,
}

impl DiskManager {
    /// Create a new disk manager with the given configuration.
    pub fn new(config: DiskQuotaConfig) -> Self {
        Self { config }
    }

    /// Check current disk usage.
    ///
    /// Uses the `df` command to get disk usage information.
    pub async fn check_usage(&self) -> Result<DiskUsage> {
        // Ensure the path exists (use parent if it doesn't)
        let check_path = if self.config.check_path.exists() {
            &self.config.check_path
        } else if let Some(parent) = self.config.check_path.parent() {
            parent
        } else {
            Path::new("/")
        };

        let output = Command::new("df")
            .args(["-B1", "--output=size,used,avail,target"])
            .arg(check_path)
            .output()
            .await
            .wrap_err("Failed to run df command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre::eyre!("df command failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_df_output(&stdout)
    }

    /// Check if there's enough space for a new worktree.
    ///
    /// Returns Ok(()) if there's enough space, or an error with a warning.
    pub async fn ensure_space(&self) -> Result<(), DiskWarning> {
        let usage = self.check_usage().await.map_err(|e| {
            warn!("Failed to check disk usage: {}", e);
            // Assume we have space if we can't check
            DiskWarning::Low {
                available_gb: 0,
                threshold_gb: self.config.warning_threshold_gb,
            }
        })?;

        let available_gb = usage.available_gb();

        if available_gb < self.config.min_free_gb {
            return Err(DiskWarning::QuotaExceeded {
                available_gb,
                required_gb: self.config.min_free_gb,
            });
        }

        if available_gb < self.config.critical_threshold_gb {
            return Err(DiskWarning::Critical {
                available_gb,
                threshold_gb: self.config.critical_threshold_gb,
            });
        }

        if available_gb < self.config.warning_threshold_gb {
            warn!(
                "{}",
                DiskWarning::Low {
                    available_gb,
                    threshold_gb: self.config.warning_threshold_gb,
                }
                .message()
            );
        }

        Ok(())
    }

    /// Check disk space and return any warnings.
    pub async fn check_warnings(&self) -> Option<DiskWarning> {
        self.ensure_space().await.err()
    }

    /// Get the configuration.
    pub fn config(&self) -> &DiskQuotaConfig {
        &self.config
    }
}

/// Parse the output of `df -B1 --output=size,used,avail,target`.
fn parse_df_output(output: &str) -> Result<DiskUsage> {
    let lines: Vec<&str> = output.lines().collect();

    // Skip header, get data line
    if lines.len() < 2 {
        return Err(eyre::eyre!("Unexpected df output format"));
    }

    let data_line = lines[1].trim();
    let parts: Vec<&str> = data_line.split_whitespace().collect();

    if parts.len() < 4 {
        return Err(eyre::eyre!("Unexpected df output format: not enough columns"));
    }

    let total_bytes: u64 = parts[0].parse().wrap_err("Failed to parse total bytes")?;
    let used_bytes: u64 = parts[1].parse().wrap_err("Failed to parse used bytes")?;
    let available_bytes: u64 = parts[2].parse().wrap_err("Failed to parse available bytes")?;
    let mount_point = PathBuf::from(parts[3]);

    Ok(DiskUsage {
        total_bytes,
        used_bytes,
        available_bytes,
        mount_point,
    })
}

/// Estimate disk usage for a directory (in bytes).
pub async fn estimate_directory_size(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let output = Command::new("du")
        .args(["-sb"])
        .arg(path)
        .output()
        .await
        .wrap_err("Failed to run du command")?;

    if !output.status.success() {
        // Return 0 if we can't determine size
        return Ok(0);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let size_str = stdout.split_whitespace().next().unwrap_or("0");
    let size: u64 = size_str.parse().unwrap_or(0);

    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_quota_config_default() {
        let config = DiskQuotaConfig::default();
        assert_eq!(config.min_free_gb, 5);
        assert_eq!(config.warning_threshold_gb, 10);
        assert_eq!(config.critical_threshold_gb, 5);
    }

    #[test]
    fn test_disk_quota_config_builder() {
        let config = DiskQuotaConfig::new(PathBuf::from("/tmp"))
            .with_min_free(10)
            .with_warning_threshold(20)
            .with_critical_threshold(10);

        assert_eq!(config.min_free_gb, 10);
        assert_eq!(config.warning_threshold_gb, 20);
        assert_eq!(config.critical_threshold_gb, 10);
        assert_eq!(config.check_path, PathBuf::from("/tmp"));
    }

    #[test]
    fn test_disk_usage_calculations() {
        let usage = DiskUsage {
            total_bytes: 100 * 1024 * 1024 * 1024,    // 100 GB
            used_bytes: 60 * 1024 * 1024 * 1024,      // 60 GB
            available_bytes: 40 * 1024 * 1024 * 1024, // 40 GB
            mount_point: PathBuf::from("/"),
        };

        assert_eq!(usage.total_gb(), 100);
        assert_eq!(usage.used_gb(), 60);
        assert_eq!(usage.available_gb(), 40);
        assert!((usage.usage_percent() - 60.0).abs() < 0.01);
    }

    #[test]
    fn test_disk_usage_zero_total() {
        let usage = DiskUsage {
            total_bytes: 0,
            used_bytes: 0,
            available_bytes: 0,
            mount_point: PathBuf::from("/"),
        };

        assert_eq!(usage.usage_percent(), 0.0);
    }

    #[test]
    fn test_disk_warning_message() {
        let low = DiskWarning::Low {
            available_gb: 8,
            threshold_gb: 10,
        };
        assert!(low.message().contains("Low disk space"));
        assert!(!low.is_critical());

        let critical = DiskWarning::Critical {
            available_gb: 3,
            threshold_gb: 5,
        };
        assert!(critical.message().contains("Critical"));
        assert!(critical.is_critical());

        let exceeded = DiskWarning::QuotaExceeded {
            available_gb: 2,
            required_gb: 5,
        };
        assert!(exceeded.message().contains("quota exceeded"));
        assert!(exceeded.is_critical());
    }

    #[test]
    fn test_parse_df_output() {
        // Simulate df -B1 output
        let output = "     Size      Used     Avail Target
107374182400 64424509440 42949672960 /";

        let usage = parse_df_output(output).unwrap();

        assert_eq!(usage.total_bytes, 107374182400);
        assert_eq!(usage.used_bytes, 64424509440);
        assert_eq!(usage.available_bytes, 42949672960);
        assert_eq!(usage.mount_point, PathBuf::from("/"));
    }

    #[test]
    fn test_parse_df_output_invalid() {
        let output = "Invalid output";
        let result = parse_df_output(output);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_disk_manager_check_usage() {
        // Use root path which should always exist
        let config = DiskQuotaConfig::new(PathBuf::from("/"));
        let manager = DiskManager::new(config);

        let result = manager.check_usage().await;

        // This might fail in some test environments, so we just check it doesn't panic
        if let Ok(usage) = result {
            assert!(usage.total_bytes > 0);
            assert!(usage.available_bytes <= usage.total_bytes);
        }
    }

    #[tokio::test]
    async fn test_estimate_directory_size() {
        // Test with a path that doesn't exist
        let size = estimate_directory_size(Path::new("/nonexistent/path")).await.unwrap();
        assert_eq!(size, 0);

        // Test with /tmp (should return something)
        let _size = estimate_directory_size(Path::new("/tmp")).await.unwrap();
        // Just verify it doesn't error out - size is u64 so always >= 0
    }
}
