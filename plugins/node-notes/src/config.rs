//! Where `node-notes` gets its settings, and why none of them are
//! `[plugin.settings]`.
//!
//! `[plugin.settings]` never reaches a plugin process. The host stores those
//! values and the console renders them, but there is no settings field in the
//! launch contract or the initialize handshake — only a web UI bundle can read
//! them back. Every limit here has to be enforced *inside* this process (a
//! sharing switch the process cannot see is a console control that promises
//! privacy and delivers none), so every limit here arrives as
//! `[[plugin]].args` or as an environment variable of the `tdcc` process.
//!
//! Precedence is uniform: **flag beats environment beats built-in default.**
//! `[[plugin]].url` is not read at all — this plugin has no backend to point at.
//!
//! An unknown flag or an out-of-range value is a startup error rather than a
//! warning. A typo in `--share` that was quietly ignored would leave an
//! operator believing their notes stay on this machine when they do not.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub const PLUGIN_NAME: &str = "node-notes";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Values read from the process environment, as a map so the parser stays a
/// pure function that tests can drive without touching real environment state.
pub type EnvMap = BTreeMap<String, String>;

/// File the local notes are persisted to, inside the state directory.
pub const NOTES_FILE: &str = "notes.json";
/// Where an unparseable notes file is moved before starting empty.
pub const CORRUPT_FILE: &str = "notes.json.corrupt";

pub const DEFAULT_MAX_NOTES: u64 = 200;
pub const DEFAULT_MAX_NOTE_CHARS: u64 = 500;
pub const DEFAULT_TTL_SECS: u64 = 3_600;
pub const DEFAULT_MAX_TTL_SECS: u64 = 86_400;
pub const DEFAULT_MAX_PEER_NOTES: u64 = 64;
pub const DEFAULT_MAX_PEERS: u64 = 64;
pub const DEFAULT_MAX_SHARES_PER_MINUTE: u64 = 20;
pub const DEFAULT_MAX_PEER_NOTES_PER_MINUTE: u64 = 30;

/// Shortest TTL worth accepting. Anything below this expires before a peer on a
/// slow link has plausibly seen it.
pub const MIN_TTL_SECS: u64 = 60;
/// Longest TTL any configuration may allow. Notes are working memory, not a
/// journal; `contribution-ledger` is the plugin for things you keep.
pub const TTL_CEILING_SECS: u64 = 30 * 86_400;

pub const ENV_STATE_DIR: &str = "TDCC_NODE_NOTES_STATE_DIR";
pub const ENV_PERSIST: &str = "TDCC_NODE_NOTES_PERSIST";
pub const ENV_SHARE: &str = "TDCC_NODE_NOTES_SHARE";
pub const ENV_MAX_NOTES: &str = "TDCC_NODE_NOTES_MAX_NOTES";
pub const ENV_MAX_NOTE_CHARS: &str = "TDCC_NODE_NOTES_MAX_NOTE_CHARS";
pub const ENV_DEFAULT_TTL_SECS: &str = "TDCC_NODE_NOTES_DEFAULT_TTL_SECS";
pub const ENV_MAX_TTL_SECS: &str = "TDCC_NODE_NOTES_MAX_TTL_SECS";
pub const ENV_MAX_PEER_NOTES: &str = "TDCC_NODE_NOTES_MAX_PEER_NOTES";
pub const ENV_MAX_PEERS: &str = "TDCC_NODE_NOTES_MAX_PEERS";
pub const ENV_MAX_SHARES_PER_MINUTE: &str = "TDCC_NODE_NOTES_MAX_SHARES_PER_MINUTE";
pub const ENV_MAX_PEER_NOTES_PER_MINUTE: &str = "TDCC_NODE_NOTES_MAX_PEER_NOTES_PER_MINUTE";

const BOOL_FLAGS: &[&str] = &["--share", "--no-persist"];
const VALUE_FLAGS: &[&str] = &[
    "--default-ttl-secs",
    "--max-note-chars",
    "--max-notes",
    "--max-peer-notes",
    "--max-peer-notes-per-minute",
    "--max-peers",
    "--max-shares-per-minute",
    "--max-ttl-secs",
    "--state-dir",
];

/// Whether local notes are written to disk, and where.
///
/// The `Disabled` variant carries the reason so `status` can say *why* nothing
/// is being persisted instead of leaving an operator to guess.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Persistence {
    Directory(PathBuf),
    Disabled(String),
}

impl Persistence {
    pub fn directory(&self) -> Option<&PathBuf> {
        match self {
            Self::Directory(path) => Some(path),
            Self::Disabled(_) => None,
        }
    }

    pub fn notes_path(&self) -> Option<PathBuf> {
        self.directory().map(|dir| dir.join(NOTES_FILE))
    }
}

/// Whether this node puts notes on the mesh at all.
///
/// Off unless the operator passed `--share`. Sharing publishes operator- and
/// model-written text to every directly connected peer, which is a disclosure
/// decision only the machine's owner can make — so the state you get by doing
/// nothing is the private one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Sharing {
    Enabled,
    Disabled,
}

impl Sharing {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// Every bound this process enforces. All of them are hard caps, not hints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Local notes retained. The oldest-expiring note is dropped to make room.
    pub max_notes: usize,
    /// Characters kept from one note's text. Longer text is truncated and the
    /// note is flagged `truncated`.
    pub max_note_chars: usize,
    /// TTL applied when a caller does not name one.
    pub default_ttl_secs: u64,
    /// Longest TTL this node accepts, for its own notes and for a peer's.
    pub max_ttl_secs: u64,
    /// Notes retained per peer. One peer cannot crowd out another.
    pub max_peer_notes: usize,
    /// Peers tracked at once. The peer heard from longest ago is evicted.
    pub max_peers: usize,
    /// Notes this node will publish per minute.
    pub max_shares_per_minute: u32,
    /// Notes this node will accept from one peer per minute.
    pub max_peer_notes_per_minute: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub persistence: Persistence,
    pub sharing: Sharing,
    pub limits: Limits,
}

impl Config {
    /// Parse `[[plugin]].args` and the process environment into a config.
    pub fn parse(args: &[String], env: &EnvMap) -> Result<Self, String> {
        let flags = parse_flags(args)?;

        let max_ttl_secs = number(
            &flags,
            env,
            "--max-ttl-secs",
            ENV_MAX_TTL_SECS,
            DEFAULT_MAX_TTL_SECS,
            MIN_TTL_SECS,
            TTL_CEILING_SECS,
        )?;
        let default_ttl_secs = number(
            &flags,
            env,
            "--default-ttl-secs",
            ENV_DEFAULT_TTL_SECS,
            DEFAULT_TTL_SECS.min(max_ttl_secs),
            MIN_TTL_SECS,
            TTL_CEILING_SECS,
        )?;
        if default_ttl_secs > max_ttl_secs {
            return Err(format!(
                "the default TTL ({default_ttl_secs}s) cannot exceed the maximum TTL \
                 ({max_ttl_secs}s); raise `--max-ttl-secs` or lower `--default-ttl-secs`"
            ));
        }

        Ok(Self {
            persistence: resolve_persistence(&flags, env)?,
            sharing: if toggle(&flags, env, "--share", ENV_SHARE, true)? {
                Sharing::Enabled
            } else {
                Sharing::Disabled
            },
            limits: Limits {
                max_notes: number(
                    &flags,
                    env,
                    "--max-notes",
                    ENV_MAX_NOTES,
                    DEFAULT_MAX_NOTES,
                    1,
                    10_000,
                )? as usize,
                max_note_chars: number(
                    &flags,
                    env,
                    "--max-note-chars",
                    ENV_MAX_NOTE_CHARS,
                    DEFAULT_MAX_NOTE_CHARS,
                    40,
                    4_000,
                )? as usize,
                default_ttl_secs,
                max_ttl_secs,
                max_peer_notes: number(
                    &flags,
                    env,
                    "--max-peer-notes",
                    ENV_MAX_PEER_NOTES,
                    DEFAULT_MAX_PEER_NOTES,
                    1,
                    1_000,
                )? as usize,
                max_peers: number(
                    &flags,
                    env,
                    "--max-peers",
                    ENV_MAX_PEERS,
                    DEFAULT_MAX_PEERS,
                    1,
                    1_000,
                )? as usize,
                max_shares_per_minute: number(
                    &flags,
                    env,
                    "--max-shares-per-minute",
                    ENV_MAX_SHARES_PER_MINUTE,
                    DEFAULT_MAX_SHARES_PER_MINUTE,
                    1,
                    600,
                )? as u32,
                max_peer_notes_per_minute: number(
                    &flags,
                    env,
                    "--max-peer-notes-per-minute",
                    ENV_MAX_PEER_NOTES_PER_MINUTE,
                    DEFAULT_MAX_PEER_NOTES_PER_MINUTE,
                    1,
                    600,
                )? as u32,
            },
        })
    }

    /// Read the real process arguments and environment.
    pub fn from_process() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let env: EnvMap = std::env::vars().collect();
        Self::parse(&args, &env)
    }
}

impl Default for Config {
    /// The configuration a bare `[[plugin]] name = "node-notes"` produces on a
    /// machine with no home directory: local-only, in memory, default limits.
    fn default() -> Self {
        Self::parse(&[], &EnvMap::new()).expect("built-in defaults are valid")
    }
}

/// Decide where — or whether — local notes are written.
///
/// A relative `--state-dir` is refused rather than resolved: a plugin's working
/// directory is the host's, not the operator's, so a relative path would land
/// somewhere nobody chose.
fn resolve_persistence(flags: &Flags, env: &EnvMap) -> Result<Persistence, String> {
    if toggle(flags, env, "--no-persist", ENV_PERSIST, false)? {
        return Ok(Persistence::Disabled(
            "`--no-persist` was passed, so notes live in memory and are lost on restart".into(),
        ));
    }

    if let Some((raw, source)) = value(flags, env, "--state-dir", ENV_STATE_DIR) {
        let path = PathBuf::from(raw.trim());
        if !path.is_absolute() {
            return Err(format!(
                "{source} must be an absolute path, got `{}`. A plugin inherits the host's \
                 working directory, so a relative path would land somewhere nobody chose.",
                path.display()
            ));
        }
        return Ok(Persistence::Directory(path));
    }

    // `HOME` on Unix, `USERPROFILE` on Windows.
    match env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .map(|home| home.trim())
        .filter(|home| !home.is_empty())
    {
        Some(home) => Ok(Persistence::Directory(
            PathBuf::from(home).join(".tdcc").join(PLUGIN_NAME),
        )),
        None => Ok(Persistence::Disabled(format!(
            "neither HOME nor USERPROFILE is set and no `--state-dir` was given, so notes live \
             in memory and are lost on restart. Set `--state-dir <absolute path>` or \
             {ENV_STATE_DIR} to keep them."
        ))),
    }
}

type Flags = BTreeMap<String, String>;

/// Accepts `--flag value`, `--flag=value`, and bare boolean flags.
fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg, None),
        };

        if BOOL_FLAGS.contains(&name) {
            let value = match inline {
                Some(value) => parse_bool(&value)
                    .ok_or_else(|| format!("`{name}` expects true or false, got `{value}`"))?,
                None => true,
            };
            flags.insert(name.to_string(), value.to_string());
            index += 1;
        } else if VALUE_FLAGS.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| format!("`{name}` expects a value"))?
                }
            };
            flags.insert(name.to_string(), value);
            index += 1;
        } else {
            return Err(format!(
                "unknown option `{arg}`. Supported: {}, {}.",
                VALUE_FLAGS.join(", "),
                BOOL_FLAGS.join(", ")
            ));
        }
    }
    Ok(flags)
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_value(env: &EnvMap, name: &str) -> Option<(String, String)> {
    env.get(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| (value, name.to_string()))
}

/// Resolve one setting, returning its value and a label naming *where* it came
/// from so an error message can point at the thing the operator actually wrote.
fn value(flags: &Flags, env: &EnvMap, flag: &str, var: &str) -> Option<(String, String)> {
    flags
        .get(flag)
        .map(|value| (value.clone(), format!("`{flag}`")))
        .or_else(|| env_value(env, var).map(|(value, name)| (value, format!("`{name}`"))))
}

fn number(
    flags: &Flags,
    env: &EnvMap,
    flag: &str,
    var: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let Some((raw, source)) = value(flags, env, flag, var) else {
        return Ok(default);
    };
    let parsed: u64 = raw
        .parse()
        .map_err(|_| format!("{source} must be a whole number, got `{raw}`"))?;
    if parsed < min || parsed > max {
        return Err(format!(
            "{source} must be between {min} and {max}, got {parsed}"
        ));
    }
    Ok(parsed)
}

/// Read a boolean whose flag and environment variable may have opposite
/// polarity. `--no-persist` turns persistence off while
/// `TDCC_NODE_NOTES_PERSIST` turns it on, so `env_means` says which value of
/// the variable corresponds to the flag being present.
fn toggle(
    flags: &Flags,
    env: &EnvMap,
    flag: &str,
    var: &str,
    env_means: bool,
) -> Result<bool, String> {
    if let Some(raw) = flags.get(flag) {
        return parse_bool(raw).ok_or_else(|| format!("`{flag}` expects true or false: {raw}"));
    }
    match env_value(env, var) {
        Some((raw, name)) => {
            let parsed =
                parse_bool(&raw).ok_or_else(|| format!("`{name}` expects true or false: {raw}"))?;
            Ok(parsed == env_means)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn sharing_is_off_and_limits_are_bounded_with_no_configuration() {
        let config = Config::parse(&[], &EnvMap::new()).expect("defaults parse");

        assert_eq!(config.sharing, Sharing::Disabled, "sharing is opt-in");
        assert_eq!(config.limits.max_notes, DEFAULT_MAX_NOTES as usize);
        assert_eq!(config.limits.default_ttl_secs, DEFAULT_TTL_SECS);
        assert_eq!(config.limits.max_ttl_secs, DEFAULT_MAX_TTL_SECS);
    }

    #[test]
    fn a_home_directory_is_where_notes_land_by_default() {
        let config = Config::parse(&[], &env(&[("HOME", "/home/op")])).expect("parses");
        assert_eq!(
            config.persistence,
            Persistence::Directory(PathBuf::from("/home/op").join(".tdcc").join("node-notes"))
        );

        let windows =
            Config::parse(&[], &env(&[("USERPROFILE", "C:\\Users\\op")])).expect("parses");
        assert!(windows.persistence.directory().is_some());
    }

    #[test]
    fn without_a_home_directory_persistence_is_disabled_with_a_reason() {
        let config = Config::parse(&[], &EnvMap::new()).expect("parses");
        let Persistence::Disabled(reason) = &config.persistence else {
            panic!("expected persistence to be disabled");
        };
        assert!(reason.contains("--state-dir"), "{reason}");
        assert!(config.persistence.notes_path().is_none());
    }

    #[test]
    fn a_relative_state_dir_is_refused_rather_than_resolved() {
        let error = Config::parse(&args(&["--state-dir", "notes"]), &EnvMap::new())
            .expect_err("a relative path is refused");
        assert!(error.contains("absolute"), "{error}");
    }

    #[test]
    fn a_flag_beats_the_environment_which_beats_the_default() {
        let from_default = Config::parse(&[], &EnvMap::new()).expect("parses");
        assert_eq!(from_default.limits.max_notes, DEFAULT_MAX_NOTES as usize);

        let from_env = Config::parse(&[], &env(&[(ENV_MAX_NOTES, "50")])).expect("parses");
        assert_eq!(from_env.limits.max_notes, 50);

        let from_flag = Config::parse(
            &args(&["--max-notes", "25"]),
            &env(&[(ENV_MAX_NOTES, "50")]),
        )
        .expect("parses");
        assert_eq!(from_flag.limits.max_notes, 25);
    }

    #[test]
    fn sharing_can_be_turned_on_by_either_channel_and_the_flag_wins() {
        assert!(
            Config::parse(&args(&["--share"]), &EnvMap::new())
                .expect("parses")
                .sharing
                .is_enabled()
        );
        assert!(
            Config::parse(&[], &env(&[(ENV_SHARE, "true")]))
                .expect("parses")
                .sharing
                .is_enabled()
        );
        assert!(
            !Config::parse(&args(&["--share=false"]), &env(&[(ENV_SHARE, "true")]))
                .expect("parses")
                .sharing
                .is_enabled(),
            "an explicit flag value overrides the environment"
        );
    }

    #[test]
    fn persistence_is_disabled_by_the_flag_and_by_the_variable() {
        assert!(matches!(
            Config::parse(&args(&["--no-persist"]), &env(&[("HOME", "/home/op")]))
                .expect("parses")
                .persistence,
            Persistence::Disabled(_)
        ));
        assert!(matches!(
            Config::parse(&[], &env(&[("HOME", "/home/op"), (ENV_PERSIST, "false")]))
                .expect("parses")
                .persistence,
            Persistence::Disabled(_)
        ));
    }

    #[test]
    fn an_unknown_flag_is_a_startup_error_naming_the_supported_set() {
        let error =
            Config::parse(&args(&["--shair"]), &EnvMap::new()).expect_err("a typo is refused");
        assert!(error.contains("unknown option"), "{error}");
        assert!(error.contains("--share"), "{error}");
    }

    #[test]
    fn a_value_flag_without_a_value_is_refused() {
        let error =
            Config::parse(&args(&["--max-notes"]), &EnvMap::new()).expect_err("no value given");
        assert!(error.contains("expects a value"), "{error}");
    }

    #[test]
    fn out_of_range_numbers_are_refused_with_the_bounds() {
        let error = Config::parse(&args(&["--max-ttl-secs", "99999999"]), &EnvMap::new())
            .expect_err("beyond the ceiling");
        assert!(error.contains(&TTL_CEILING_SECS.to_string()), "{error}");

        let error = Config::parse(&args(&["--default-ttl-secs", "10"]), &EnvMap::new())
            .expect_err("below the floor");
        assert!(error.contains(&MIN_TTL_SECS.to_string()), "{error}");
    }

    #[test]
    fn a_default_ttl_above_the_maximum_is_refused_rather_than_clamped() {
        let error = Config::parse(
            &args(&["--default-ttl-secs", "7200", "--max-ttl-secs", "3600"]),
            &EnvMap::new(),
        )
        .expect_err("an impossible pair is refused");
        assert!(error.contains("cannot exceed"), "{error}");
    }

    #[test]
    fn lowering_the_maximum_ttl_lowers_the_default_with_it() {
        // Otherwise `--max-ttl-secs 600` alone would be an error, which is a
        // hostile way to treat the most obvious way to tighten retention.
        let config =
            Config::parse(&args(&["--max-ttl-secs", "600"]), &EnvMap::new()).expect("parses");
        assert_eq!(config.limits.max_ttl_secs, 600);
        assert_eq!(config.limits.default_ttl_secs, 600);
    }

    #[test]
    fn inline_and_separated_flag_values_are_both_accepted() {
        let inline = Config::parse(&args(&["--max-notes=17"]), &EnvMap::new()).expect("parses");
        let separated =
            Config::parse(&args(&["--max-notes", "17"]), &EnvMap::new()).expect("parses");
        assert_eq!(inline.limits.max_notes, separated.limits.max_notes);
    }

    #[test]
    fn a_non_numeric_value_names_the_setting_that_was_wrong() {
        let error = Config::parse(&[], &env(&[(ENV_MAX_PEERS, "lots")])).expect_err("not a number");
        assert!(error.contains(ENV_MAX_PEERS), "{error}");
    }
}
