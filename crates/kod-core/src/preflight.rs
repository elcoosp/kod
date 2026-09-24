//! Pre-run resource sampling.
//!
//! An overnight run that exhausts a provider quota at hour one, or
//! that starts on a machine already swapping, wastes the night. This
//! samples the machine before the run starts so a caller can refuse
//! or warn — it does not enforce anything itself.
//!
//! The parsing is separated from the reading: `parse_*` functions
//! take the raw text a platform tool produced and are tested against
//! fixtures. The `sample()` function reads whichever source the host
//! has, and reports `None` for what it cannot find rather than
//! guessing — a made-up RAM figure is worse than an absent one.

use serde::{Deserialize, Serialize};

/// What the machine looked like when the run started.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreflightSample {
    /// Total physical memory, bytes.
    pub total_ram_bytes: Option<u64>,
    /// Free memory, bytes.
    pub free_ram_bytes: Option<u64>,
    /// Swap in use, bytes.
    pub swap_used_bytes: Option<u64>,
    /// 1-minute load average.
    pub load_1m: Option<f64>,
    /// Battery percentage, when on battery power.
    pub battery_percent: Option<u8>,
    /// True when the host is running on battery.
    pub on_battery: bool,
}

impl PreflightSample {
    /// Whether the machine is under enough pressure that a long run is
    /// a bad idea: swapping, or a load average above the core count.
    ///
    /// Returns the reasons, so a caller can print them rather than a
    /// bare "no".
    pub fn pressure_reasons(&self, cores: usize) -> Vec<String> {
        let mut out = Vec::new();
        if let (Some(total), Some(free)) = (self.total_ram_bytes, self.free_ram_bytes)
            && total > 0
            && (free as f64 / total as f64) < 0.10
        {
            out.push(format!(
                "less than 10% RAM free ({free} of {total} bytes)",
            ));
        }
        if let Some(swap) = self.swap_used_bytes
            && swap > 2 * 1024 * 1024 * 1024
        {
            out.push(format!("{} GiB swapping", swap / (1024 * 1024 * 1024)));
        }
        if let Some(load) = self.load_1m
            && cores > 0
            && load > cores as f64
        {
            out.push(format!("load {load:.1} exceeds {cores} cores"));
        }
        if self.on_battery {
            out.push(match self.battery_percent {
                Some(p) => format!("on battery at {p}%"),
                None => "on battery".to_string(),
            });
        }
        out
    }
}

/// Parse Linux `/proc/meminfo`. Returns `(total, free, swap_used)`.
pub fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    let total = field("MemTotal:");
    let available = field("MemAvailable:").or_else(|| field("MemFree:"));
    let swap_total = field("SwapTotal:").unwrap_or(0);
    let swap_free = field("SwapFree:").unwrap_or(0);
    let swap_used = swap_total.saturating_sub(swap_free);
    (total, available, Some(swap_used))
}

/// Parse `sysctl -n vm.loadavg` (macOS), which prints `{ 1.5 2.0 3.0 }`.
pub fn parse_loadavg(text: &str) -> Option<f64> {
    text.trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace())
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
}

/// Parse `pmset -g batt` (macOS). Returns `(percent, on_battery)`.
pub fn parse_pmset(text: &str) -> (Option<u8>, bool) {
    let on_battery = text.contains("Battery Power");
    let percent = text
        .split('%')
        .next()
        .and_then(|before| before.split_whitespace().last())
        .and_then(|v| v.parse::<u8>().ok());
    (percent, on_battery)
}

/// Sample the host.
pub fn sample() -> PreflightSample {
    let mut s = PreflightSample::default();
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            let (t, f, sw) = parse_meminfo(&text);
            s.total_ram_bytes = t;
            s.free_ram_bytes = f;
            s.swap_used_bytes = sw;
        }
        if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
            s.load_1m = text.split_whitespace().next().and_then(|v| v.parse().ok());
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        if let Ok(out) = Command::new("sysctl").args(["-n", "hw.memsize"]).output() {
            s.total_ram_bytes = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .ok();
        }
        if let Ok(out) = Command::new("sysctl").args(["-n", "vm.loadavg"]).output() {
            s.load_1m = parse_loadavg(&String::from_utf8_lossy(&out.stdout));
        }
        if let Ok(out) = Command::new("pmset").args(["-g", "batt"]).output() {
            let (p, on_batt) = parse_pmset(&String::from_utf8_lossy(&out.stdout));
            s.battery_percent = p;
            s.on_battery = on_batt;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "\
MemTotal:       16384000 kB
MemFree:         2000000 kB
MemAvailable:    8000000 kB
SwapTotal:       4096000 kB
SwapFree:        4000000 kB
";

    #[test]
    fn meminfo_parses_the_fields_it_needs() {
        let (total, free, swap) = parse_meminfo(MEMINFO);
        assert_eq!(total, Some(16_384_000 * 1024));
        assert_eq!(free, Some(8_000_000 * 1024), "prefers MemAvailable");
        assert_eq!(swap, Some(96_000 * 1024));
    }

    #[test]
    fn meminfo_falls_back_to_memfree_when_available_is_absent() {
        let text = "MemTotal: 1000 kB\nMemFree: 500 kB\n";
        let (_, free, _) = parse_meminfo(text);
        assert_eq!(free, Some(500 * 1024));
    }

    #[test]
    fn meminfo_missing_fields_are_none_not_zero() {
        let (total, free, _) = parse_meminfo("nothing here\n");
        assert!(total.is_none());
        assert!(free.is_none());
    }

    #[test]
    fn loadavg_parses_the_braced_form() {
        assert_eq!(parse_loadavg("{ 1.50 2.00 3.00 }"), Some(1.5));
        assert_eq!(parse_loadavg("2.25 1.0 0.5"), Some(2.25));
    }

    #[test]
    fn pmset_reads_percent_and_power_source() {
        let on_batt = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=123) 43%; discharging;";
        assert_eq!(parse_pmset(on_batt), (Some(43), true));

        let on_ac = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=123) 100%; charged;";
        assert_eq!(parse_pmset(on_ac), (Some(100), false));
    }

    #[test]
    fn low_ram_is_a_pressure_reason() {
        let s = PreflightSample {
            total_ram_bytes: Some(1000),
            free_ram_bytes: Some(50),
            ..Default::default()
        };
        let r = s.pressure_reasons(8);
        assert!(r.iter().any(|m| m.contains("RAM")), "got: {r:?}");
    }

    #[test]
    fn heavy_swap_is_a_pressure_reason() {
        let s = PreflightSample {
            swap_used_bytes: Some(4 * 1024 * 1024 * 1024),
            ..Default::default()
        };
        assert!(s.pressure_reasons(8).iter().any(|m| m.contains("swap")));
    }

    #[test]
    fn high_load_is_a_pressure_reason() {
        let s = PreflightSample {
            load_1m: Some(16.0),
            ..Default::default()
        };
        assert!(s.pressure_reasons(8).iter().any(|m| m.contains("cores")));
    }

    #[test]
    fn a_healthy_machine_has_no_reasons() {
        let s = PreflightSample {
            total_ram_bytes: Some(16_000_000_000),
            free_ram_bytes: Some(8_000_000_000),
            swap_used_bytes: Some(0),
            load_1m: Some(0.5),
            on_battery: false,
            ..Default::default()
        };
        assert!(s.pressure_reasons(8).is_empty());
    }
}
