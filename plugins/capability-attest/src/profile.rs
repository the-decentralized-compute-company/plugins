//! The pinned inputs of a benchmark.
//!
//! An unqualified "42 tok/s" is not a measurement: it does not say which model,
//! how much context, or what sampling settings produced it. Everything that can
//! change the number lives in [`BenchmarkProfile`], the profile is embedded in
//! every signed record, and [`BenchmarkProfile::fingerprint`] gives verifiers a
//! single value to compare before they put two nodes on the same scale.
//!
//! The prompt is not stored as free text and then hoped over — it is *derived*
//! from the profile by [`BenchmarkProfile::prompt`], deterministically, and its
//! SHA-256 is pinned in the profile. Anyone can rebuild the exact bytes that
//! were sent.
//!
//! # Why the sampling settings are integers
//!
//! `temperature` and `top_p` are stored in thousandths, not as `f64`. Anything
//! inside a signed record has to survive a JSON round trip *bit for bit*, or a
//! verifier recomputes different bytes and reports a bad signature for a record
//! that was signed correctly. `serde_json` does not guarantee that for `f64` —
//! it is measurably not true for values like `31.165399999999998` — so no
//! floating-point number appears anywhere in a claim. `record.rs` has a test
//! that keeps it that way.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The sentence repeated to reach the requested context size.
///
/// Deliberately dull and self-describing: it must not be interesting enough to
/// change how a model behaves between versions, and a reader who finds it in a
/// server log should be able to tell what it is.
pub const DEFAULT_FILLER_SENTENCE: &str =
    "This paragraph is deterministic benchmark filler used to reach a fixed context size.";

/// Appended to every prompt so the model has an actual task.
const PROMPT_SUFFIX: &str = "\n\nContinue the passage above.";

/// Characters per token, in thousandths, assumed when turning a token budget
/// into a prompt length.
///
/// A plugin cannot tokenize for an arbitrary remote model, so this is an
/// estimate. It is recorded as an estimate, and the *measured* prompt token
/// count from the server's `usage` block is recorded next to it whenever the
/// server reports one.
pub const CHARS_PER_TOKEN_ESTIMATE_MILLI: u32 = 4_000;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BenchmarkProfile {
    /// Model id sent to the endpoint. Two records for different models are not
    /// comparable, however similar the names look.
    pub model: String,
    /// Requested prompt length in tokens. Approximate by construction; see
    /// `chars_per_token_estimate_milli`.
    pub context_tokens: u32,
    /// `max_tokens` for the request. Also the ceiling on the generation window
    /// the throughput number is divided by.
    pub max_output_tokens: u32,
    /// Sampling temperature in thousandths: 200 means 0.2.
    pub temperature_milli: u32,
    /// Sampling `top_p` in thousandths: 900 means 0.9.
    pub top_p_milli: u32,
    /// Sampling seed sent to the endpoint. Servers that ignore it still produce
    /// comparable timings; the field records what was asked for.
    pub seed: u64,
    /// Runs performed and discarded before measuring, to pay for cold caches.
    pub warmup_runs: u32,
    /// Runs whose timings go into the record.
    pub measured_runs: u32,
    pub chars_per_token_estimate_milli: u32,
    pub filler_sentence: String,
    /// Length of the built prompt in `char`s.
    pub prompt_chars: u32,
    /// SHA-256 of the built prompt, lowercase hex. The prompt itself is not
    /// stored — it is reproducible from the fields above, and this is what
    /// proves a rebuild matched.
    pub prompt_sha256: String,
}

impl BenchmarkProfile {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        model: String,
        context_tokens: u32,
        max_output_tokens: u32,
        temperature: f64,
        top_p: f64,
        seed: u64,
        warmup_runs: u32,
        measured_runs: u32,
        filler_sentence: String,
    ) -> Result<Self> {
        if model.trim().is_empty() {
            bail!("--model must not be empty");
        }
        if !(16..=131_072).contains(&context_tokens) {
            bail!("--context-tokens must be between 16 and 131072, got {context_tokens}");
        }
        if !(8..=8192).contains(&max_output_tokens) {
            bail!("--max-output-tokens must be between 8 and 8192, got {max_output_tokens}");
        }
        let temperature_milli = to_milli("--temperature", temperature, 0, 2_000)?;
        let top_p_milli = to_milli("--top-p", top_p, 1, 1_000)?;
        if warmup_runs > 8 {
            bail!("--warmup-runs must be at most 8, got {warmup_runs}");
        }
        if !(1..=16).contains(&measured_runs) {
            bail!("--measured-runs must be between 1 and 16, got {measured_runs}");
        }
        if filler_sentence.trim().is_empty() {
            bail!("--filler-sentence must not be empty");
        }

        let mut profile = Self {
            model,
            context_tokens,
            max_output_tokens,
            temperature_milli,
            top_p_milli,
            seed,
            warmup_runs,
            measured_runs,
            chars_per_token_estimate_milli: CHARS_PER_TOKEN_ESTIMATE_MILLI,
            filler_sentence,
            // Filled in below, once the prompt those two describe exists.
            prompt_chars: 0,
            prompt_sha256: String::new(),
        };
        let prompt = build_prompt(
            &profile.filler_sentence,
            profile.context_tokens,
            profile.chars_per_token_estimate_milli,
        )?;
        profile.prompt_chars = prompt.chars().count() as u32;
        profile.prompt_sha256 = sha256_hex(prompt.as_bytes());
        Ok(profile)
    }

    /// Sampling temperature as the request body wants it.
    pub fn temperature(&self) -> f64 {
        f64::from(self.temperature_milli) / 1000.0
    }

    /// Sampling `top_p` as the request body wants it.
    pub fn top_p(&self) -> f64 {
        f64::from(self.top_p_milli) / 1000.0
    }

    /// Rebuild the exact prompt this profile describes.
    pub fn prompt(&self) -> Result<String> {
        let prompt = build_prompt(
            &self.filler_sentence,
            self.context_tokens,
            self.chars_per_token_estimate_milli,
        )?;
        // A profile that arrived over the mesh is untrusted input. If its
        // recorded hash does not match what its own fields rebuild, the record
        // is describing a prompt nobody can reproduce.
        let actual = sha256_hex(prompt.as_bytes());
        if actual != self.prompt_sha256 {
            bail!(
                "profile prompt_sha256 does not match the prompt its own fields rebuild \
                 (recorded {}, rebuilt {actual})",
                self.prompt_sha256
            );
        }
        Ok(prompt)
    }

    /// A short digest of every pinned input.
    ///
    /// Two measurements are comparable if and only if these match. Verifiers
    /// should treat a fingerprint mismatch as "different benchmark", not as
    /// "slower node".
    pub fn fingerprint(&self) -> String {
        let canonical =
            serde_json::to_vec(self).expect("BenchmarkProfile contains no non-serialisable values");
        sha256_hex(&canonical)[..32].to_string()
    }

    pub fn comparable_with(&self, other: &Self) -> bool {
        self.fingerprint() == other.fingerprint()
    }
}

/// Convert an operator-supplied decimal into thousandths, rejecting anything
/// outside the allowed range.
fn to_milli(flag: &str, value: f64, low: u32, high: u32) -> Result<u32> {
    if !value.is_finite() {
        bail!("{flag} must be a number, got {value}");
    }
    let scaled = (value * 1000.0).round();
    if scaled < f64::from(low) || scaled > f64::from(high) {
        bail!(
            "{flag} must be between {} and {}, got {value}",
            f64::from(low) / 1000.0,
            f64::from(high) / 1000.0
        );
    }
    Ok(scaled as u32)
}

/// Repeat `filler` until the prompt is exactly the requested number of `char`s,
/// then append the fixed task suffix.
///
/// Exactness matters more than elegance here: the point is that two machines
/// running the same profile send byte-identical prompts. The arithmetic is
/// integer arithmetic for the same reason.
fn build_prompt(filler: &str, context_tokens: u32, chars_per_token_milli: u32) -> Result<String> {
    if chars_per_token_milli == 0 {
        bail!("chars_per_token_estimate_milli must be greater than zero");
    }
    let target_chars =
        (u64::from(context_tokens) * u64::from(chars_per_token_milli)).div_ceil(1000) as usize;
    let suffix_chars = PROMPT_SUFFIX.chars().count();
    if target_chars <= suffix_chars {
        bail!(
            "context_tokens {context_tokens} is too small to build a prompt \
             (needs more than {suffix_chars} characters)"
        );
    }
    let filler_target = target_chars - suffix_chars;

    let unit = format!("{} ", filler.trim());
    let mut body = String::with_capacity(filler_target + unit.len());
    while body.chars().count() < filler_target {
        body.push_str(&unit);
    }
    let body: String = body.chars().take(filler_target).collect();

    Ok(format!("{body}{PROMPT_SUFFIX}"))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> BenchmarkProfile {
        BenchmarkProfile::build(
            "demo-model".into(),
            128,
            64,
            0.0,
            1.0,
            42,
            1,
            3,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .expect("valid profile")
    }

    #[test]
    fn the_prompt_is_exactly_the_requested_length_and_ends_with_the_task() {
        let built = profile();
        let prompt = built.prompt().unwrap();

        assert_eq!(prompt.chars().count(), 128 * 4);
        assert_eq!(prompt.chars().count() as u32, built.prompt_chars);
        assert!(prompt.ends_with("Continue the passage above."));
    }

    #[test]
    fn the_same_profile_rebuilds_byte_identical_prompts() {
        let first = profile().prompt().unwrap();
        let second = profile().prompt().unwrap();

        assert_eq!(first, second);
        assert_eq!(profile().prompt_sha256, sha256_hex(first.as_bytes()));
    }

    #[test]
    fn a_tampered_prompt_hash_is_caught_when_the_prompt_is_rebuilt() {
        // The shape of a record arriving from a peer: fields say one thing, the
        // pinned hash says another.
        let mut tampered = profile();
        tampered.prompt_sha256 = "00".repeat(32);

        let error = tampered.prompt().unwrap_err();

        assert!(error.to_string().contains("prompt_sha256"), "{error}");
    }

    #[test]
    fn sampling_settings_are_pinned_as_exact_thousandths() {
        let built = BenchmarkProfile::build(
            "demo-model".into(),
            128,
            64,
            0.2,
            0.9,
            1,
            0,
            1,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .unwrap();

        assert_eq!(built.temperature_milli, 200);
        assert_eq!(built.top_p_milli, 900);
        assert!((built.temperature() - 0.2).abs() < 1e-12);
        assert!((built.top_p() - 0.9).abs() < 1e-12);
    }

    #[test]
    fn changing_any_pinned_input_changes_the_fingerprint() {
        let base = profile();
        let baseline = base.fingerprint();

        let mut model = base.clone();
        model.model = "other-model".into();
        assert_ne!(model.fingerprint(), baseline);

        let mut context = base.clone();
        context.context_tokens = 256;
        assert_ne!(context.fingerprint(), baseline);

        let mut temperature = base.clone();
        temperature.temperature_milli = 700;
        assert_ne!(temperature.fingerprint(), baseline);

        let mut seed = base.clone();
        seed.seed = 43;
        assert_ne!(seed.fingerprint(), baseline);

        let mut output = base.clone();
        output.max_output_tokens = 65;
        assert_ne!(output.fingerprint(), baseline);

        assert!(base.comparable_with(&profile()));
        assert!(!base.comparable_with(&model));
    }

    #[test]
    fn a_profile_survives_a_json_round_trip_unchanged() {
        // The property the whole signing scheme rests on.
        let base = profile();
        let encoded = serde_json::to_string(&base).unwrap();
        let decoded: BenchmarkProfile = serde_json::from_str(&encoded).unwrap();

        assert_eq!(base, decoded);
        assert_eq!(encoded, serde_json::to_string(&decoded).unwrap());
    }

    #[test]
    fn a_longer_filler_sentence_still_produces_the_requested_length() {
        // Truncation has to land on a character boundary, not a byte boundary.
        let built = BenchmarkProfile::build(
            "demo-model".into(),
            32,
            8,
            0.0,
            1.0,
            1,
            0,
            1,
            "ünïcödé filler sentence with multi-byte characters".into(),
        )
        .unwrap();

        assert_eq!(built.prompt().unwrap().chars().count(), 32 * 4);
    }

    #[test]
    fn out_of_range_settings_are_refused_with_the_flag_name() {
        let too_small = BenchmarkProfile::build(
            "m".into(),
            4,
            64,
            0.0,
            1.0,
            1,
            0,
            1,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .unwrap_err();
        assert!(too_small.to_string().contains("--context-tokens"));

        let hot = BenchmarkProfile::build(
            "m".into(),
            128,
            64,
            9.0,
            1.0,
            1,
            0,
            1,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .unwrap_err();
        assert!(hot.to_string().contains("--temperature"));

        let zero_top_p = BenchmarkProfile::build(
            "m".into(),
            128,
            64,
            0.0,
            0.0,
            1,
            0,
            1,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .unwrap_err();
        assert!(zero_top_p.to_string().contains("--top-p"));

        let no_runs = BenchmarkProfile::build(
            "m".into(),
            128,
            64,
            0.0,
            1.0,
            1,
            0,
            0,
            DEFAULT_FILLER_SENTENCE.into(),
        )
        .unwrap_err();
        assert!(no_runs.to_string().contains("--measured-runs"));
    }
}
