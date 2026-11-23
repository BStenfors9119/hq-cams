//! Device health for the status panel: temperature, throttling, load, memory,
//! WiFi and whether the camera is producing frames.
//!
//! Everything is read from `/proc`, `/sys` or a quick tool call (`vcgencmd`,
//! `nmcli`) on each request — cheap enough for a page polling every few seconds,
//! and it means there's no background thread. A source that can't be read
//! (e.g. running off-Pi) comes back as `null` rather than failing the request.

use std::fs;
use std::process::Command;

use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub hostname: String,
    pub version: &'static str,
    pub uptime_secs: Option<u64>,
    pub temp_c: Option<f32>,
    pub throttle: Option<Throttle>,
    /// 1/5/15-minute load averages.
    pub load: Option<[f32; 3]>,
    pub cpus: usize,
    pub mem_total_mb: Option<u64>,
    pub mem_available_mb: Option<u64>,
    pub wifi: Option<Wifi>,
}

/// `vcgencmd get_throttled`, decoded. "Now" flags are current; "since boot"
/// flags mean it happened at some point since the Pi started.
#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Throttle {
    pub raw: String,
    pub under_voltage_now: bool,
    pub freq_capped_now: bool,
    pub throttled_now: bool,
    pub soft_temp_limit_now: bool,
    pub under_voltage_since_boot: bool,
    pub freq_capped_since_boot: bool,
    pub throttled_since_boot: bool,
    pub soft_temp_limit_since_boot: bool,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Wifi {
    pub ssid: String,
    /// NetworkManager's 0–100 signal quality.
    pub signal: u8,
}

pub fn health() -> Health {
    Health {
        hostname: read("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: read("/proc/uptime").and_then(|s| parse_uptime(&s)),
        temp_c: read("/sys/class/thermal/thermal_zone0/temp").and_then(|s| parse_millideg(&s)),
        throttle: run("vcgencmd", &["get_throttled"]).and_then(|s| parse_throttled(&s)),
        load: read("/proc/loadavg").and_then(|s| parse_loadavg(&s)),
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        mem_total_mb: read("/proc/meminfo").and_then(|s| meminfo_mb(&s, "MemTotal")),
        mem_available_mb: read("/proc/meminfo").and_then(|s| meminfo_mb(&s, "MemAvailable")),
        // --rescan no: report the cached scan; never make the radio rescan.
        wifi: run("nmcli", &["-t", "-f", "ACTIVE,SSID,SIGNAL", "device", "wifi", "list", "--rescan", "no"])
            .and_then(|s| parse_wifi(&s)),
    }
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_uptime(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse::<f64>().ok().map(|f| f as u64)
}

fn parse_millideg(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().map(|m| (m / 100.0).round() / 10.0)
}

fn parse_loadavg(s: &str) -> Option<[f32; 3]> {
    let mut it = s.split_whitespace().map(|v| v.parse::<f32>().ok());
    Some([it.next()??, it.next()??, it.next()??])
}

fn meminfo_mb(s: &str, key: &str) -> Option<u64> {
    let line = s.lines().find(|l| l.split(':').next() == Some(key))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

/// `throttled=0x50005` → the bit flags (see the Raspberry Pi `vcgencmd` docs).
fn parse_throttled(s: &str) -> Option<Throttle> {
    let hex = s.trim().strip_prefix("throttled=")?.trim_start_matches("0x");
    let v = u32::from_str_radix(hex, 16).ok()?;
    let bit = |n: u32| v & (1 << n) != 0;
    Some(Throttle {
        raw: format!("0x{v:x}"),
        under_voltage_now: bit(0),
        freq_capped_now: bit(1),
        throttled_now: bit(2),
        soft_temp_limit_now: bit(3),
        under_voltage_since_boot: bit(16),
        freq_capped_since_boot: bit(17),
        throttled_since_boot: bit(18),
        soft_temp_limit_since_boot: bit(19),
    })
}

/// The active network from `nmcli -t -f ACTIVE,SSID,SIGNAL device wifi list`.
/// Terse mode escapes `:` inside an SSID as `\:`, so split from the right.
fn parse_wifi(s: &str) -> Option<Wifi> {
    s.lines().find_map(|line| {
        let mut parts = line.rsplitn(2, ':');
        let signal = parts.next()?.parse().ok()?;
        let rest = parts.next()?;
        let ssid = rest.strip_prefix("yes:")?;
        Some(Wifi { ssid: ssid.replace("\\:", ":"), signal })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_throttle_bits() {
        let t = parse_throttled("throttled=0x20000\n").unwrap();
        assert_eq!(t.raw, "0x20000");
        assert!(t.freq_capped_since_boot);
        assert!(!t.freq_capped_now && !t.under_voltage_now && !t.throttled_since_boot);
        let t = parse_throttled("throttled=0x50005").unwrap();
        assert!(t.under_voltage_now && t.throttled_now && t.under_voltage_since_boot && t.throttled_since_boot);
        assert!(parse_throttled("garbage").is_none());
    }

    #[test]
    fn parses_proc_files() {
        assert_eq!(parse_millideg("70908\n"), Some(70.9));
        assert_eq!(parse_uptime("2405.17 9001.22\n"), Some(2405));
        assert_eq!(parse_loadavg("2.07 2.04 1.89 3/139 755\n"), Some([2.07, 2.04, 1.89]));
        let mem = "MemTotal:         436332 kB\nMemFree:  103321 kB\nMemAvailable:     249856 kB\n";
        assert_eq!(meminfo_mb(mem, "MemTotal"), Some(426));
        assert_eq!(meminfo_mb(mem, "MemAvailable"), Some(244));
        assert_eq!(meminfo_mb(mem, "Swap"), None);
    }

    #[test]
    fn finds_the_active_wifi() {
        let out = "no:Neighbours:40\nyes:Sodapop\\: HQ:72\nno::15\n";
        assert_eq!(parse_wifi(out), Some(Wifi { ssid: "Sodapop: HQ".into(), signal: 72 }));
        assert_eq!(parse_wifi("no:Other:30\n"), None);
    }
}
