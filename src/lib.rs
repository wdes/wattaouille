// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::fs;

/// Per-process sample from `/proc/[pid]/stat` and friends.
pub struct Sample {
    pub comm: String,
    pub cmdline_args: Vec<String>,
    pub ppid: u32,
    pub cpu_jiffies: u64,
    pub cwd: Option<String>,
    pub io_bytes: Option<u64>,
}

/// Parse `/proc/[pid]/io` and return `read_bytes + write_bytes`.
pub fn parse_proc_io(text: &str) -> u64 {
    let mut rb: u64 = 0;
    let mut wb: u64 = 0;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            rb = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            wb = v.trim().parse().unwrap_or(0);
        }
    }
    rb.saturating_add(wb)
}

/// Read `/proc/[pid]/stat` and return a [Sample].
pub fn read_proc_stat(pid: &str) -> Option<Sample> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let lparen = stat.find('(')?;
    let rparen = stat.rfind(')')?;
    let comm = stat[lparen + 1..rparen].to_string();
    let rest: Vec<&str> = stat[rparen + 2..].split_whitespace().collect();
    let ppid: u32 = rest.get(1)?.parse().ok()?;
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;

    let cmdline_raw = fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline_args: Vec<String> = cmdline_raw
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let cwd = fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|p| p.to_str().map(String::from));

    let io_bytes = fs::read_to_string(format!("/proc/{pid}/io"))
        .ok()
        .map(|s| parse_proc_io(&s));

    Some(Sample {
        comm,
        cmdline_args,
        ppid,
        cpu_jiffies: utime + stime,
        cwd,
        io_bytes,
    })
}

/// Snapshot all processes from `/proc`.
pub fn snapshot() -> HashMap<u32, Sample> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        let Ok(pid) = name_str.parse::<u32>() else { continue };
        if let Some(sample) = read_proc_stat(name_str) {
            out.insert(pid, sample);
        }
    }
    out
}

/// Total system CPU jiffies from `/proc/stat`.
pub fn total_cpu_jiffies() -> u64 {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let first = stat.lines().next().unwrap_or("");
    first
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse::<u64>().ok())
        .sum()
}

/// Number of logical CPUs.
pub fn num_cpus() -> u64 {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    stat.lines()
        .filter(|l| l.starts_with("cpu") && !l.starts_with("cpu "))
        .count()
        .max(1) as u64
}

/// An Intel RAPL subdomain (core, uncore, dram).
pub struct RaplDomain {
    pub label: String,
    pub energy_path: String,
    pub max_uj: u64,
}

impl RaplDomain {
    pub fn read_uj(&self) -> Option<u64> {
        fs::read_to_string(&self.energy_path)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn joules_between(&self, before: u64, after: u64) -> f64 {
        let delta_uj = if after >= before {
            after - before
        } else {
            self.max_uj.saturating_sub(before).saturating_add(after)
        };
        delta_uj as f64 / 1_000_000.0
    }
}

/// Intel RAPL package-0 energy sensor.
pub struct PowerSensor {
    pub energy_path: String,
    pub max_uj: u64,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub subdomains: Vec<RaplDomain>,
}

impl PowerSensor {
    pub const PATH: &'static str = "/sys/class/powercap/intel-rapl:0/energy_uj";
    pub const WRAP_PATH: &'static str = "/sys/class/powercap/intel-rapl:0/max_energy_range_uj";
    pub const DOMAIN_DIR: &'static str = "/sys/class/powercap";

    pub fn detect(force_off: bool) -> Self {
        let max_uj = fs::read_to_string(Self::WRAP_PATH)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        if force_off {
            return Self {
                energy_path: Self::PATH.to_string(),
                max_uj,
                enabled: false,
                disabled_reason: Some("forced off via --no-power".to_string()),
                subdomains: vec![],
            };
        }
        match fs::read_to_string(Self::PATH) {
            Ok(_) => {
                let mut subdomains = Vec::new();
                if let Ok(entries) = fs::read_dir("/sys/class/powercap") {
                    let mut paths: Vec<String> = entries
                        .flatten()
                        .filter_map(|e| {
                            let n = e.file_name();
                            let n = n.to_str()?;
                            if n.starts_with("intel-rapl:0:") {
                                Some(format!("/sys/class/powercap/{n}"))
                            } else {
                                None
                            }
                        })
                        .collect();
                    paths.sort();
                    for path in paths {
                        let label = fs::read_to_string(format!("{path}/name"))
                            .ok()
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default();
                        let energy_path = format!("{path}/energy_uj");
                        if fs::read_to_string(&energy_path).is_err() {
                            continue;
                        }
                        let max = fs::read_to_string(format!("{path}/max_energy_range_uj"))
                            .ok()
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(u64::MAX);
                        subdomains.push(RaplDomain {
                            label,
                            energy_path,
                            max_uj: max,
                        });
                    }
                }
                Self {
                    energy_path: Self::PATH.to_string(),
                    max_uj,
                    enabled: true,
                    disabled_reason: None,
                    subdomains,
                }
            }
            Err(e) => Self {
                energy_path: Self::PATH.to_string(),
                max_uj,
                enabled: false,
                disabled_reason: Some(format!("{e}")),
                subdomains: vec![],
            },
        }
    }

    pub fn instructions(&self) -> String {
        let reason = self
            .disabled_reason
            .clone()
            .unwrap_or_else(|| "unknown".into());

        let rapl_present = fs::metadata("/sys/class/powercap/intel-rapl:0").is_ok();
        let powercap_present = fs::metadata(Self::DOMAIN_DIR).is_ok();

        let mut s = String::new();
        s.push_str("⚡ Wattage disabled\n");
        s.push_str(&format!("   reason: {reason}\n"));
        if !powercap_present {
            s.push_str(
                "   /sys/class/powercap is missing — your kernel was built without\n   \
                 CONFIG_POWERCAP. No RAPL access is possible on this system.\n",
            );
            return s;
        }
        if !rapl_present {
            s.push_str(
                "   /sys/class/powercap/intel-rapl:0 is missing — likely an AMD or ARM\n   \
                 CPU. Try `ls /sys/class/powercap` to see what's available; AMD\n   \
                 systems may expose `amd_energy` instead.\n",
            );
            return s;
        }
        s.push_str("   RAPL counters exist but are mode 0400 (root-only).\n");
        s.push_str("   Pick one to enable wattage:\n\n");
        s.push_str("     # 1. One-shot for this boot:\n");
        s.push_str(
            "     sudo chmod a+r /sys/class/powercap/intel-rapl:0/energy_uj \\\n          \
             /sys/class/powercap/intel-rapl/intel-rapl:0/intel-rapl:0:*/energy_uj\n\n",
        );
        s.push_str("     # 2. Persist via udev (survives reboots):\n");
        s.push_str("     echo 'SUBSYSTEM==\"powercap\", ACTION==\"add\", \\\n");
        s.push_str(
            "       RUN+=\"/bin/chmod a+r /sys%p/energy_uj\"' | \\\n         \
             sudo tee /etc/udev/rules.d/60-rapl-readable.rules\n     \
             sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=powercap\n\n",
        );
        s.push_str("     # 3. Or just run wattaouille with sudo.\n");
        s
    }

    pub fn read_uj(&self) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        fs::read_to_string(&self.energy_path)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn joules_between(&self, before: u64, after: u64) -> f64 {
        let delta_uj = if after >= before {
            after - before
        } else {
            self.max_uj.saturating_sub(before).saturating_add(after)
        };
        delta_uj as f64 / 1_000_000.0
    }
}

/// Children of a given PID via `/proc/[pid]/task/[pid]/children`.
pub fn children_of(pid: u32) -> Vec<u32> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    if let Ok(text) = fs::read_to_string(&path) {
        return text
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
    }
    Vec::new()
}

/// Sum CPU jiffies for a process and all its descendants.
pub fn sum_tree_jiffies(root: u32) -> u64 {
    let j = read_proc_stat(&root.to_string())
        .map(|s| s.cpu_jiffies)
        .unwrap_or(0);
    let mut total = j;
    let mut stack = children_of(root);
    while let Some(pid) = stack.pop() {
        total += read_proc_stat(&pid.to_string())
            .map(|s| s.cpu_jiffies)
            .unwrap_or(0);
        stack.extend(children_of(pid));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_cpu_jiffies_nonzero() {
        assert!(total_cpu_jiffies() > 0);
    }

    #[test]
    fn num_cpus_at_least_one() {
        assert!(num_cpus() >= 1);
    }

    #[test]
    fn snapshot_finds_processes() {
        let snap = snapshot();
        assert!(!snap.is_empty());
        assert!(snap.contains_key(&1));
    }

    #[test]
    fn read_proc_stat_pid1() {
        let s = read_proc_stat("1").unwrap();
        assert!(!s.comm.is_empty());
    }

    #[test]
    fn read_proc_stat_nonexistent() {
        assert!(read_proc_stat("999999999").is_none());
    }

    #[test]
    fn parse_proc_io_sums_read_and_write() {
        let text = "rchar: 999\nwchar: 1\nsyscr: 5\nsyscw: 3\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_proc_io(text), 4096 + 8192);
    }

    #[test]
    fn parse_proc_io_handles_missing_fields() {
        assert_eq!(parse_proc_io("rchar: 100\nwchar: 0\n"), 0);
    }

    #[test]
    fn rapl_joules_simple() {
        let p = PowerSensor {
            energy_path: PowerSensor::PATH.to_string(),
            max_uj: 1_000_000_000,
            enabled: false,
            disabled_reason: None,
            subdomains: vec![],
        };
        assert!((p.joules_between(1_000_000, 6_000_000) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn rapl_joules_wraparound() {
        let p = PowerSensor {
            energy_path: PowerSensor::PATH.to_string(),
            max_uj: 100,
            enabled: false,
            disabled_reason: None,
            subdomains: vec![],
        };
        assert!((p.joules_between(95, 10) - 15.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn rapl_domain_joules_wraparound() {
        let d = RaplDomain {
            label: "core".to_string(),
            energy_path: "x".to_string(),
            max_uj: 100,
        };
        assert!((d.joules_between(95, 10) - 15.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn children_of_nonexistent() {
        assert!(children_of(u32::MAX).is_empty());
    }

    #[test]
    fn sum_tree_jiffies_pid1() {
        assert!(sum_tree_jiffies(1) > 0);
    }
}
