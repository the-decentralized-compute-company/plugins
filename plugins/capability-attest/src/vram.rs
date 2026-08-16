//! Usable VRAM, measured where that is possible and labelled where it is not.
//!
//! # Blast radius
//!
//! This is the only part of the plugin that starts a process. It runs exactly
//! one command, `nvidia-smi`, with a fixed argument list that no configuration
//! value and no request argument can influence, through
//! [`tokio::process::Command`] — no shell, so no quoting or injection path
//! exists. It is disabled entirely with `--vram-probe off`.
//!
//! Because the command is resolved through `PATH`, a `PATH` an attacker
//! controls means an attacker-chosen `nvidia-smi`. That is worth writing down
//! even though a plugin already runs as the user: it is one more reason to run
//! `tdcc` with a `PATH` you own.
//!
//! # What is *not* here
//!
//! No ROCm, Metal, or Level Zero probe. Their output formats are not something
//! this plugin can claim to parse correctly without being able to test against
//! them, and a wrong VRAM number in a signed record is worse than an absent
//! one. Those platforms use `--vram-total-mib`, which is recorded as
//! `source: "operator-declared"` so a verifier can weigh it accordingly.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::VramProbeKind;

/// How long `nvidia-smi` gets before the probe gives up on it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct VramDevice {
    pub index: u32,
    pub total_mib: u64,
    pub free_mib: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct VramReading {
    /// `"nvidia-smi"`, `"operator-declared"`, or `"unavailable"`. A verifier
    /// that treats these three the same is not verifying anything.
    pub source: String,
    pub total_mib: Option<u64>,
    /// Free VRAM at the moment of the probe. Only a measurement source can
    /// supply this; an operator-declared total leaves it `null`.
    pub free_mib: Option<u64>,
    pub devices: Vec<VramDevice>,
    /// Human-readable note: which probe ran, or why none did.
    pub detail: String,
}

impl VramReading {
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            source: "unavailable".to_string(),
            total_mib: None,
            free_mib: None,
            devices: Vec::new(),
            detail: detail.into(),
        }
    }

    fn operator_declared(total_mib: u64, why_no_probe: &str) -> Self {
        Self {
            source: "operator-declared".to_string(),
            total_mib: Some(total_mib),
            free_mib: None,
            devices: Vec::new(),
            detail: format!(
                "{total_mib} MiB declared with --vram-total-mib; not measured ({why_no_probe})"
            ),
        }
    }

    fn measured(devices: Vec<VramDevice>) -> Self {
        let total_mib = devices.iter().map(|device| device.total_mib).sum();
        let free_mib = devices.iter().map(|device| device.free_mib).sum();
        Self {
            source: "nvidia-smi".to_string(),
            total_mib: Some(total_mib),
            free_mib: Some(free_mib),
            detail: format!("{} device(s) reported by nvidia-smi", devices.len()),
            devices,
        }
    }
}

/// Measure VRAM, falling back to an operator declaration and then to nothing.
///
/// A measurement always beats a declaration: `--vram-total-mib` is a statement
/// of intent, and if the probe can see the hardware, the hardware wins.
pub async fn probe(kind: VramProbeKind, operator_total_mib: Option<u64>) -> VramReading {
    let probe_failure = match kind {
        VramProbeKind::Off => "--vram-probe off".to_string(),
        VramProbeKind::NvidiaSmi => match run_nvidia_smi().await {
            Ok(devices) if !devices.is_empty() => return VramReading::measured(devices),
            Ok(_) => "nvidia-smi reported no devices".to_string(),
            Err(error) => error,
        },
    };

    match operator_total_mib {
        Some(total_mib) => VramReading::operator_declared(total_mib, &probe_failure),
        None => VramReading::unavailable(format!(
            "no VRAM reading: {probe_failure}. Set --vram-total-mib to declare one, \
             knowing it will be recorded as operator-declared"
        )),
    }
}

/// Fixed argument list. Nothing configurable reaches it.
const NVIDIA_SMI_ARGS: &[&str] = &[
    "--query-gpu=memory.total,memory.free",
    "--format=csv,noheader,nounits",
];

async fn run_nvidia_smi() -> Result<Vec<VramDevice>, String> {
    let mut command = tokio::process::Command::new("nvidia-smi");
    command.args(NVIDIA_SMI_ARGS);
    // If the timeout below drops this future, the child goes with it rather
    // than surviving as an orphan holding the GPU driver open.
    command.kill_on_drop(true);

    let output = match tokio::time::timeout(PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("could not run nvidia-smi: {error}")),
        Err(_) => {
            return Err(format!(
                "nvidia-smi did not finish within {}s",
                PROBE_TIMEOUT.as_secs()
            ));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "nvidia-smi exited with {}: {}",
            output.status,
            first_line(&stderr)
        ));
    }
    parse_nvidia_smi_csv(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the output of
/// `nvidia-smi --query-gpu=memory.total,memory.free --format=csv,noheader,nounits`.
///
/// One line per device, two comma-separated integers in MiB. Anything that does
/// not match that exactly is an error, not a partially trusted reading.
pub fn parse_nvidia_smi_csv(stdout: &str) -> Result<Vec<VramDevice>, String> {
    let mut devices = Vec::new();
    for (index, line) in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let mut fields = line.split(',').map(str::trim);
        let (Some(total), Some(free), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(format!(
                "unexpected nvidia-smi line {:?}: expected \"<total>, <free>\"",
                line
            ));
        };
        let total_mib = total
            .parse::<u64>()
            .map_err(|error| format!("unexpected nvidia-smi total {total:?}: {error}"))?;
        let free_mib = free
            .parse::<u64>()
            .map_err(|error| format!("unexpected nvidia-smi free {free:?}: {error}"))?;
        if free_mib > total_mib {
            return Err(format!(
                "nvidia-smi reported {free_mib} MiB free of {total_mib} MiB total"
            ));
        }
        devices.push(VramDevice {
            index: index as u32,
            total_mib,
            free_mib,
        });
    }
    Ok(devices)
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_gpu_reading_sums_into_one_total() {
        let devices = parse_nvidia_smi_csv("24564, 23990\n24564, 24010\n").unwrap();

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[1].index, 1);

        let reading = VramReading::measured(devices);
        assert_eq!(reading.source, "nvidia-smi");
        assert_eq!(reading.total_mib, Some(49_128));
        assert_eq!(reading.free_mib, Some(48_000));
    }

    #[test]
    fn blank_lines_and_trailing_whitespace_are_tolerated() {
        let devices = parse_nvidia_smi_csv("\n  8192,   7000  \n\n").unwrap();

        assert_eq!(
            devices,
            vec![VramDevice {
                index: 0,
                total_mib: 8192,
                free_mib: 7000
            }]
        );
    }

    #[test]
    fn output_that_is_not_the_expected_shape_is_an_error_not_a_guess() {
        for hostile in [
            "24564",
            "24564, 23990, 100",
            "24564, N/A",
            "[N/A], [N/A]",
            "not,numbers",
        ] {
            assert!(
                parse_nvidia_smi_csv(hostile).is_err(),
                "{hostile:?} should not parse into a VRAM number"
            );
        }
    }

    #[test]
    fn more_free_than_total_is_refused() {
        let error = parse_nvidia_smi_csv("100, 200").unwrap_err();
        assert!(error.contains("free"), "{error}");
    }

    #[test]
    fn no_devices_parses_to_an_empty_list_rather_than_zero_vram() {
        assert_eq!(parse_nvidia_smi_csv("").unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn a_disabled_probe_falls_through_to_the_operator_declaration() {
        let declared = probe(VramProbeKind::Off, Some(24_576)).await;

        assert_eq!(declared.source, "operator-declared");
        assert_eq!(declared.total_mib, Some(24_576));
        assert_eq!(
            declared.free_mib, None,
            "a declaration cannot know how much is free right now"
        );
        assert!(declared.detail.contains("--vram-probe off"));
    }

    #[tokio::test]
    async fn a_disabled_probe_with_no_declaration_reports_unavailable() {
        let reading = probe(VramProbeKind::Off, None).await;

        assert_eq!(reading.source, "unavailable");
        assert_eq!(reading.total_mib, None);
        assert!(reading.detail.contains("--vram-total-mib"));
    }
}
