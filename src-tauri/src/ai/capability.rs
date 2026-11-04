//! Device capability detection → ML tier selection.
//!
//! Tier is used to route the LLM between local/cloud (and historically
//! drove the now-removed embedding-backend selection). Logic per
//! `_ml-architecture.md`:
//!
//! - Tier 0 — Cloud Only: RAM < 3GB OR disk < 500MB
//! - Tier 1 — Hybrid (default): RAM 3–6GB, 2+ cores → local embeds + cloud LLM
//! - Tier 2 — Mostly Local: RAM 6–12GB, 4+ cores → local embeds + local LLM
//! - Tier 3 — Fully Local: RAM ≥ 12GB OR discrete GPU

use serde::{Deserialize, Serialize};
use sysinfo::{Disks, System};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum MlTier {
    Tier0,
    Tier1,
    Tier2,
    Tier3,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct DeviceProfile {
    /// Total physical RAM in gigabytes.
    pub total_ram_gb: f32,
    /// Logical CPU cores (includes hyperthreads).
    #[specta(type = u32)]
    pub cpu_cores: usize,
    /// Free disk space in the home/data drive, gigabytes.
    pub free_disk_gb: f32,
    /// Auto-detected tier based on hardware. User may override in settings.
    pub tier: MlTier,
    /// Platform name (Windows/Linux/macOS/iOS/Android).
    pub platform: String,
}

pub fn detect() -> DeviceProfile {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_list(sysinfo::CpuRefreshKind::everything());

    let total_ram_gb = (sys.total_memory() as f64 / 1_073_741_824.0) as f32; // bytes → GiB
    let cpu_cores = sys.cpus().len();

    // Free disk on the largest mounted disk (reasonable proxy for "where data lives").
    let disks = Disks::new_with_refreshed_list();
    let free_disk_gb = disks
        .iter()
        .map(|d| d.available_space())
        .max()
        .map(|b| (b as f64 / 1_073_741_824.0) as f32)
        .unwrap_or(0.0);

    let tier = classify(total_ram_gb, cpu_cores, free_disk_gb);

    DeviceProfile {
        total_ram_gb,
        cpu_cores,
        free_disk_gb,
        tier,
        platform: std::env::consts::OS.to_string(),
    }
}

fn classify(ram_gb: f32, cores: usize, free_disk_gb: f32) -> MlTier {
    // Not enough room for a 137MB model + embeddings → cloud-only is safer.
    if ram_gb < 3.0 || free_disk_gb < 0.5 {
        return MlTier::Tier0;
    }
    if ram_gb < 6.0 || cores < 4 {
        return MlTier::Tier1;
    }
    if ram_gb < 12.0 {
        return MlTier::Tier2;
    }
    MlTier::Tier3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_ram_is_tier0() {
        assert_eq!(classify(2.0, 8, 100.0), MlTier::Tier0);
    }

    #[test]
    fn low_disk_is_tier0() {
        assert_eq!(classify(16.0, 8, 0.1), MlTier::Tier0);
    }

    #[test]
    fn mid_range_is_tier1() {
        assert_eq!(classify(4.0, 2, 50.0), MlTier::Tier1);
    }

    #[test]
    fn dual_core_is_tier1_even_with_ram() {
        // 2 cores caps out at Tier 1 even if RAM is high — thin laptop class.
        assert_eq!(classify(16.0, 2, 100.0), MlTier::Tier1);
    }

    #[test]
    fn standard_laptop_is_tier2() {
        assert_eq!(classify(8.0, 4, 200.0), MlTier::Tier2);
    }

    #[test]
    fn beefy_machine_is_tier3() {
        assert_eq!(classify(32.0, 12, 500.0), MlTier::Tier3);
    }
}
