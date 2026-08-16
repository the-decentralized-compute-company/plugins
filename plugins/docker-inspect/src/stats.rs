//! Turning one `/stats` sample into numbers a person recognises.
//!
//! The stats endpoint reports counters, not rates: CPU is cumulative nanoseconds
//! and only becomes a percentage when compared against the previous sample and
//! against how much CPU time the whole machine used in the same interval. The
//! daemon includes that previous sample as `precpu_stats` when the request is
//! made with `stream=false`, which is why this plugin asks for it that way and
//! why the call takes about a second.
//!
//! Everything here is arithmetic on a parsed payload, so all of it is tested
//! directly against captured shapes from both cgroup versions and from Windows,
//! where the fields are genuinely different rather than merely absent.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::format_bytes;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ContainerStats {
    /// RFC 3339 timestamp of this sample.
    #[serde(default)]
    pub read: String,
    #[serde(default)]
    pub cpu_stats: CpuStats,
    #[serde(default)]
    pub precpu_stats: CpuStats,
    #[serde(default)]
    pub memory_stats: MemoryStats,
    #[serde(default)]
    pub pids_stats: PidsStats,
    #[serde(default)]
    pub networks: BTreeMap<String, NetworkStats>,
    #[serde(default)]
    pub blkio_stats: BlkioStats,
    /// Windows only; Linux reports processes through `pids_stats`.
    #[serde(default)]
    pub num_procs: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CpuStats {
    #[serde(default)]
    pub cpu_usage: CpuUsage,
    /// Absent on Windows, where there is no equivalent counter.
    #[serde(default)]
    pub system_cpu_usage: Option<u64>,
    #[serde(default)]
    pub online_cpus: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CpuUsage {
    #[serde(default)]
    pub total_usage: u64,
    /// Removed in newer API versions; used only to count CPUs when
    /// `online_cpus` is missing.
    #[serde(default)]
    pub percpu_usage: Vec<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MemoryStats {
    #[serde(default)]
    pub usage: u64,
    #[serde(default)]
    pub limit: u64,
    /// cgroup counters. `inactive_file` on cgroup v2, `cache` on v1.
    #[serde(default)]
    pub stats: BTreeMap<String, u64>,
    /// Windows only.
    #[serde(default)]
    pub privateworkingset: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PidsStats {
    #[serde(default)]
    pub current: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NetworkStats {
    #[serde(default)]
    pub rx_bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BlkioStats {
    #[serde(default)]
    pub io_service_bytes_recursive: Option<Vec<BlkioEntry>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BlkioEntry {
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub value: u64,
}

/// CPU use as a percentage of one whole CPU multiplied by the number of CPUs —
/// the same number `docker stats` prints, so 200% means two cores saturated.
///
/// Returns `None` when it cannot be computed rather than `0.0`: a Windows
/// daemon reports no system-wide counter, and a container that has just started
/// has no previous sample. Zero and "unknown" are different answers and a model
/// will report whichever it is given.
pub fn cpu_percent(stats: &ContainerStats) -> Option<f64> {
    let system = stats.cpu_stats.system_cpu_usage?;
    let previous_system = stats.precpu_stats.system_cpu_usage?;

    let cpu_delta = stats
        .cpu_stats
        .cpu_usage
        .total_usage
        .checked_sub(stats.precpu_stats.cpu_usage.total_usage)?;
    let system_delta = system.checked_sub(previous_system)?;
    if system_delta == 0 {
        return None;
    }

    let cpus = online_cpus(stats)?;
    Some(cpu_delta as f64 / system_delta as f64 * cpus as f64 * 100.0)
}

/// How many CPUs the sample covers, from whichever field the daemon populated.
pub fn online_cpus(stats: &ContainerStats) -> Option<u64> {
    stats
        .cpu_stats
        .online_cpus
        .filter(|count| *count > 0)
        .or_else(|| {
            let counted = stats.cpu_stats.cpu_usage.percpu_usage.len() as u64;
            (counted > 0).then_some(counted)
        })
}

/// Memory actually held by the container, with page cache discounted the way
/// `docker stats` discounts it.
///
/// cgroup v2 reports `inactive_file`, cgroup v1 reports `cache`, and Windows
/// reports neither but has `privateworkingset` instead. Getting this wrong
/// overstates a container's memory by however large its page cache is, which on
/// a database is most of it.
pub fn memory_usage_bytes(stats: &ContainerStats) -> u64 {
    if stats.memory_stats.usage == 0 && stats.memory_stats.privateworkingset > 0 {
        return stats.memory_stats.privateworkingset;
    }
    let cache = stats
        .memory_stats
        .stats
        .get("inactive_file")
        .or_else(|| stats.memory_stats.stats.get("cache"))
        .copied()
        .unwrap_or(0);
    stats.memory_stats.usage.saturating_sub(cache)
}

/// Total received and transmitted bytes across every interface in the sample.
pub fn network_totals(stats: &ContainerStats) -> (u64, u64) {
    stats.networks.values().fold((0, 0), |(rx, tx), interface| {
        (
            rx.saturating_add(interface.rx_bytes),
            tx.saturating_add(interface.tx_bytes),
        )
    })
}

/// Total bytes read from and written to block devices.
///
/// The `op` field is `read`/`write` on current daemons and `Read`/`Write` on
/// older ones, so it is compared case-insensitively; anything else (`sync`,
/// `async`, `total`) is a different breakdown of the same bytes and would
/// double-count.
pub fn block_io_totals(stats: &ContainerStats) -> (u64, u64) {
    let Some(entries) = &stats.blkio_stats.io_service_bytes_recursive else {
        return (0, 0);
    };
    entries.iter().fold((0, 0), |(read, write), entry| {
        if entry.op.eq_ignore_ascii_case("read") {
            (read.saturating_add(entry.value), write)
        } else if entry.op.eq_ignore_ascii_case("write") {
            (read, write.saturating_add(entry.value))
        } else {
            (read, write)
        }
    })
}

/// A process limit large enough that it is not a limit.
///
/// cgroups spell "no limit" as a sentinel rather than as an absence: cgroup v2
/// reports `u64::MAX` and cgroup v1 reports `0x7FFF_FFFF_FFFF_F000`. Reported
/// verbatim, that becomes "18446744073709551615 processes" in a tool result,
/// which a model will read as a real number and repeat.
const PID_LIMIT_CEILING: u64 = 1 << 40;

/// The same sentinel problem for memory. Eight petabytes is far above any real
/// machine and far below either sentinel.
const MEMORY_LIMIT_CEILING: u64 = 1 << 53;

/// `None` for an absent limit and for a sentinel standing in for "unlimited".
fn real_limit(value: Option<u64>, ceiling: u64) -> Option<u64> {
    value.filter(|limit| *limit > 0 && *limit < ceiling)
}

/// The whole sample, as one response body.
pub fn to_json(stats: &ContainerStats) -> Value {
    let usage = memory_usage_bytes(stats);
    let limit = real_limit(Some(stats.memory_stats.limit), MEMORY_LIMIT_CEILING);
    let (rx, tx) = network_totals(stats);
    let (read, write) = block_io_totals(stats);
    let percent = cpu_percent(stats);

    json!({
        "sampled_at": if stats.read.is_empty() { None } else { Some(stats.read.clone()) },
        "cpu": {
            "percent": percent.map(|value| (value * 100.0).round() / 100.0),
            "online_cpus": online_cpus(stats),
            "note": percent.is_none().then_some(
                "The daemon did not report the system-wide CPU counter this calculation needs. \
                 Windows daemons never do; on Linux this usually means the container had only \
                 just started when the sample was taken."
            ),
        },
        "memory": {
            "usage_bytes": usage,
            "usage": format_bytes(usage),
            "limit_bytes": limit,
            "limit": limit.map(format_bytes),
            "percent": limit
                .map(|limit| (usage as f64 / limit as f64 * 10_000.0).round() / 100.0),
        },
        "network": {
            "rx_bytes": rx,
            "tx_bytes": tx,
            "interfaces": stats.networks.len(),
        },
        "block_io": {
            "read_bytes": read,
            "write_bytes": write,
        },
        "processes": {
            "current": stats.pids_stats.current.or((stats.num_procs > 0).then_some(stats.num_procs)),
            "limit": real_limit(stats.pids_stats.limit, PID_LIMIT_CEILING),
        },
        "note": "One sample, taken now. These are instantaneous rates for CPU and cumulative \
                 totals since container start for network and block IO.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cgroup v2 sample, trimmed to the fields this module reads.
    fn linux_sample() -> ContainerStats {
        serde_json::from_str(
            r#"{
                "read": "2024-05-01T10:00:00.000000000Z",
                "pids_stats": {"current": 12, "limit": 4096},
                "networks": {
                    "eth0": {"rx_bytes": 1000, "tx_bytes": 2000},
                    "eth1": {"rx_bytes": 500, "tx_bytes": 100}
                },
                "memory_stats": {
                    "usage": 209715200,
                    "limit": 1073741824,
                    "stats": {"inactive_file": 104857600, "anon": 104857600}
                },
                "cpu_stats": {
                    "cpu_usage": {"total_usage": 2000000000},
                    "system_cpu_usage": 40000000000,
                    "online_cpus": 4
                },
                "precpu_stats": {
                    "cpu_usage": {"total_usage": 1000000000},
                    "system_cpu_usage": 20000000000,
                    "online_cpus": 4
                },
                "blkio_stats": {
                    "io_service_bytes_recursive": [
                        {"op": "read", "value": 4096},
                        {"op": "write", "value": 8192},
                        {"op": "total", "value": 12288}
                    ]
                }
            }"#,
        )
        .expect("the captured sample parses")
    }

    #[test]
    fn cpu_percent_matches_the_docker_stats_calculation() {
        // 1e9 ns of container time against 2e10 ns of system time on 4 CPUs.
        assert_eq!(cpu_percent(&linux_sample()), Some(20.0));
    }

    #[test]
    fn cpu_percent_is_unknown_rather_than_zero_without_a_system_counter() {
        let windows: ContainerStats = serde_json::from_str(
            r#"{
                "num_procs": 9,
                "memory_stats": {"privateworkingset": 52428800},
                "cpu_stats": {"cpu_usage": {"total_usage": 1000}},
                "precpu_stats": {"cpu_usage": {"total_usage": 500}}
            }"#,
        )
        .expect("parses");

        assert_eq!(cpu_percent(&windows), None);
        assert_eq!(memory_usage_bytes(&windows), 52_428_800);

        let rendered = to_json(&windows);
        assert_eq!(rendered["cpu"]["percent"], json!(null));
        assert!(
            rendered["cpu"]["note"]
                .as_str()
                .expect("a note explains the missing number")
                .contains("Windows")
        );
        assert_eq!(rendered["processes"]["current"], json!(9));
    }

    #[test]
    fn a_zero_system_delta_does_not_divide_by_zero() {
        let mut stats = linux_sample();
        stats.precpu_stats.system_cpu_usage = stats.cpu_stats.system_cpu_usage;
        assert_eq!(cpu_percent(&stats), None);
    }

    #[test]
    fn a_counter_that_went_backwards_is_reported_as_unknown() {
        let mut stats = linux_sample();
        stats.precpu_stats.cpu_usage.total_usage = stats.cpu_stats.cpu_usage.total_usage + 1;
        assert_eq!(cpu_percent(&stats), None);
    }

    #[test]
    fn cpus_fall_back_to_the_per_cpu_array_when_online_cpus_is_absent() {
        let mut stats = linux_sample();
        stats.cpu_stats.online_cpus = None;
        stats.cpu_stats.cpu_usage.percpu_usage = vec![1, 2, 3, 4];
        assert_eq!(online_cpus(&stats), Some(4));
        assert_eq!(cpu_percent(&stats), Some(20.0));
    }

    #[test]
    fn page_cache_is_discounted_from_memory_on_both_cgroup_versions() {
        // v2: `inactive_file`.
        assert_eq!(memory_usage_bytes(&linux_sample()), 104_857_600);

        // v1: `cache`.
        let mut v1 = linux_sample();
        v1.memory_stats.stats = BTreeMap::from([("cache".to_string(), 52_428_800)]);
        assert_eq!(memory_usage_bytes(&v1), 157_286_400);

        // Neither: report the raw usage rather than nothing.
        let mut bare = linux_sample();
        bare.memory_stats.stats.clear();
        assert_eq!(memory_usage_bytes(&bare), 209_715_200);
    }

    #[test]
    fn network_and_block_io_are_summed_without_double_counting() {
        let stats = linux_sample();
        assert_eq!(network_totals(&stats), (1500, 2100));
        assert_eq!(block_io_totals(&stats), (4096, 8192));
    }

    #[test]
    fn older_daemons_capitalised_block_io_operations() {
        let mut stats = linux_sample();
        stats.blkio_stats.io_service_bytes_recursive = Some(vec![
            BlkioEntry {
                op: "Read".into(),
                value: 10,
            },
            BlkioEntry {
                op: "Write".into(),
                value: 20,
            },
        ]);
        assert_eq!(block_io_totals(&stats), (10, 20));
    }

    #[test]
    fn a_missing_block_io_section_is_zero_rather_than_an_error() {
        let mut stats = linux_sample();
        stats.blkio_stats.io_service_bytes_recursive = None;
        assert_eq!(block_io_totals(&stats), (0, 0));
    }

    #[test]
    fn an_unlimited_cgroup_sentinel_is_reported_as_no_limit() {
        let mut stats = linux_sample();
        // cgroup v2 spells "max" as u64::MAX; cgroup v1 uses this value.
        stats.pids_stats.limit = Some(u64::MAX);
        stats.memory_stats.limit = 0x7FFF_FFFF_FFFF_F000;

        let rendered = to_json(&stats);

        assert_eq!(rendered["processes"]["limit"], json!(null));
        assert_eq!(rendered["memory"]["limit"], json!(null));
        assert_eq!(rendered["memory"]["limit_bytes"], json!(null));
        assert_eq!(rendered["memory"]["percent"], json!(null));
        // A real limit still comes through.
        assert_eq!(to_json(&linux_sample())["processes"]["limit"], json!(4096));
    }

    #[test]
    fn the_rendered_sample_carries_readable_numbers_and_the_limit() {
        let rendered = to_json(&linux_sample());

        assert_eq!(rendered["cpu"]["percent"], json!(20.0));
        assert_eq!(rendered["cpu"]["note"], json!(null));
        assert_eq!(rendered["memory"]["usage"], json!("100.0 MiB"));
        assert_eq!(rendered["memory"]["limit"], json!("1.0 GiB"));
        assert_eq!(rendered["memory"]["percent"], json!(9.77));
        assert_eq!(rendered["network"]["rx_bytes"], json!(1500));
        assert_eq!(rendered["processes"]["current"], json!(12));
        assert_eq!(
            rendered["sampled_at"],
            json!("2024-05-01T10:00:00.000000000Z")
        );
    }
}
