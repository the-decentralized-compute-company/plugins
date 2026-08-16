//! Test-only scaffolding: a throwaway directory tree, a directory-link helper,
//! and a hand-built WAV fixture.
//!
//! Hand-rolled rather than pulled from a crate so the plugin's release
//! dependency set stays as small as the thing it does — nothing here is
//! compiled into the shipped binary.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory under the system temp dir that deletes itself on drop.
pub struct TempTree {
    path: PathBuf,
}

impl TempTree {
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "transcribe-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp tree");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The tree root as the plugin would hold it: canonical, so containment
    /// checks compare like with like.
    pub fn canonical_root(&self) -> PathBuf {
        std::fs::canonicalize(&self.path).expect("canonicalize temp tree")
    }

    /// Write a file at a `/`-separated relative path, creating parents.
    pub fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let mut target = self.path.clone();
        for segment in relative.split('/') {
            target.push(segment);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(&target, contents).expect("write temp file");
        target
    }

    pub fn mkdir(&self, relative: &str) -> PathBuf {
        let mut target = self.path.clone();
        for segment in relative.split('/') {
            target.push(segment);
        }
        std::fs::create_dir_all(&target).expect("create directory");
        target
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        // Best effort: a leaked temp directory is a nuisance, a panicking
        // destructor masking a real test failure is worse.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create a directory link at `link` pointing at `target`.
///
/// Unix gets a symlink. Windows tries a real symlink first — which needs
/// Developer Mode or `SeCreateSymbolicLinkPrivilege` — and falls back to a
/// directory junction, which an unprivileged user can create. Returns `Err(())`
/// when the platform allows neither, so a test can skip rather than fail on a
/// locked-down machine.
pub fn link_directory(target: &Path, link: &Path) -> Result<(), ()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map_err(|_| ())
    }

    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok(());
        }
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|_| ())?;
        if status.success() && link.exists() {
            Ok(())
        } else {
            Err(())
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(())
    }
}

/// A canonical 16-bit PCM WAV of `seconds` duration.
///
/// The samples are a slow ramp rather than silence so a slice taken from the
/// middle is distinguishable from a slice taken from the start — which is what
/// the chunking tests need to prove.
pub fn wav_fixture(sample_rate: u32, channels: u16, seconds: f64) -> Vec<u8> {
    let frames = (sample_rate as f64 * seconds).round() as u32;
    let mut samples = Vec::with_capacity(frames as usize * channels as usize * 2);
    for frame in 0..frames {
        for channel in 0..channels {
            let value = (frame as i32 + channel as i32) as i16;
            samples.extend_from_slice(&value.to_le_bytes());
        }
    }
    wav_with_data(sample_rate, channels, 16, &samples)
}

/// Wrap raw sample bytes in a canonical 44-byte-header WAV.
pub fn wav_with_data(
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    data: &[u8],
) -> Vec<u8> {
    let block_align = channels * bits_per_sample / 8;
    let byte_rate = sample_rate * u32::from(block_align);

    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}
