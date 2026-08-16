//! What never enters the index.
//!
//! Three separate jobs, deliberately kept apart:
//!
//! - **Secrets.** Name and content heuristics for credential-shaped files. A
//!   file that trips one of these is not indexed, not searched, and not
//!   readable through this plugin. See the README: this is a heuristic, not a
//!   guarantee.
//! - **Vendored and generated trees.** `node_modules`, `target`, `dist`,
//!   minified bundles. Skipping them is about the model's context window, not
//!   about safety.
//! - **Unreadable content.** Binary files and files whose lines are so long
//!   they are obviously machine-written.
//!
//! `.gitignore` itself is handled by the `ignore` crate during the walk, which
//! is the same implementation ripgrep uses; this module only covers what
//! `.gitignore` does not.

/// Version-control metadata. Skipped unconditionally — `--include-vendored`
/// does not bring these back, because nothing in them is source a model should
/// be reading.
pub const VERSION_CONTROL_DIRECTORIES: &[&str] = &[".git", ".hg", ".svn", ".jj", ".bzr"];

/// Directories that habitually hold credentials. Skipped unconditionally.
pub const SECRET_DIRECTORIES: &[&str] = &[
    ".ssh", ".gnupg", ".gpg", ".aws", ".azure", ".kube", ".docker",
];

/// Dependency, build-output, and tool-cache directories. Skipped unless
/// `--include-vendored` is set.
pub const VENDORED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "bower_components",
    "vendor",
    "third_party",
    "thirdparty",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    "__pycache__",
    ".venv",
    "venv",
    "site-packages",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".idea",
    ".vs",
    "Pods",
    "DerivedData",
    ".terraform",
    ".cargo",
    "coverage",
];

/// Exact file names that are credential-shaped.
const SECRET_FILE_NAMES: &[&str] = &[
    ".env",
    ".envrc",
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".htpasswd",
    ".pgpass",
    ".git-credentials",
    ".dockercfg",
    "credentials",
    "credentials.json",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
    "kubeconfig",
    "terraform.tfvars",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Extensions that carry keys, certificates, or password databases.
const SECRET_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "asc", "gpg", "pgp", "ppk", "kdbx", "der",
    "crt", "cer",
];

/// Private-key file name conventions (`deploy_rsa`, `service_ed25519`, ...).
const SECRET_NAME_SUFFIXES: &[&str] = &["_rsa", "_dsa", "_ecdsa", "_ed25519"];

/// Machine-generated artefacts that happen to be text.
const GENERATED_NAME_SUFFIXES: &[&str] = &[
    ".min.js",
    ".min.css",
    ".min.mjs",
    ".map",
    ".bundle.js",
    ".pack.js",
];

/// Bytes sampled from the head of a file when deciding whether it is binary.
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// A file with any line longer than this is treated as generated. This is the
/// minified-bundle guard: one 2 MB line is not something a model can use.
pub const MAX_LINE_BYTES: usize = 2000;

pub fn is_version_control_directory(name: &str) -> bool {
    VERSION_CONTROL_DIRECTORIES.contains(&name)
}

pub fn is_secret_directory(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    SECRET_DIRECTORIES.contains(&lowered.as_str())
}

pub fn is_vendored_directory(name: &str) -> bool {
    VENDORED_DIRECTORIES.contains(&name)
}

/// True when any segment of a `/`-separated relative path is credential-shaped,
/// or its final segment is a credential-shaped file name.
pub fn is_secret_path(relative: &str) -> bool {
    let segments: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    let Some((name, directories)) = segments.split_last() else {
        return false;
    };
    directories
        .iter()
        .any(|segment| is_secret_directory(segment))
        || is_secret_file_name(name)
}

pub fn is_secret_file_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();

    // `.env`, and every `.env.<anything>` — including `.env.example`. Blocking
    // the example file too costs a little convenience and removes the judgement
    // call about which suffixes are safe.
    if lowered == ".env" || lowered.starts_with(".env.") {
        return true;
    }
    if SECRET_FILE_NAMES.contains(&lowered.as_str()) {
        return true;
    }
    if SECRET_NAME_SUFFIXES
        .iter()
        .any(|suffix| lowered.ends_with(suffix))
    {
        return true;
    }
    // `id_rsa.pub` is not secret, but it sits next to one and costs nothing.
    if let Some(stem) = lowered.strip_suffix(".pub")
        && (SECRET_FILE_NAMES.contains(&stem)
            || SECRET_NAME_SUFFIXES
                .iter()
                .any(|suffix| stem.ends_with(suffix)))
    {
        return true;
    }
    match lowered.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => SECRET_EXTENSIONS.contains(&extension),
        _ => false,
    }
}

pub fn is_generated_file_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    GENERATED_NAME_SUFFIXES
        .iter()
        .any(|suffix| lowered.ends_with(suffix))
}

/// A NUL byte in the head of the file. Cheap, and the same signal `git` uses.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0)
}

pub fn longest_line_bytes(text: &str) -> usize {
    text.lines().map(str::len).max().unwrap_or(0)
}

pub fn looks_minified(text: &str) -> bool {
    longest_line_bytes(text) > MAX_LINE_BYTES
}

/// A PEM private-key block.
///
/// The marker must start its own line, which is how PEM actually writes it.
/// Matching the bare substring anywhere would mean this very file — which
/// contains the marker as a string literal — could not be indexed by the
/// plugin it belongs to.
pub fn contains_private_key_block(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("-----BEGIN")
            && (trimmed.contains("PRIVATE KEY") || trimmed.contains("OPENSSH PRIVATE KEY"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_file_names_are_recognised() {
        for name in [
            ".env",
            ".ENV",
            ".env.local",
            ".env.production",
            "id_rsa",
            "id_ed25519",
            "deploy_rsa",
            "server.pem",
            "server.KEY",
            "vault.kdbx",
            "credentials.json",
            "terraform.tfvars",
            ".git-credentials",
            "id_rsa.pub",
        ] {
            assert!(is_secret_file_name(name), "expected {name:?} to be secret");
        }
    }

    #[test]
    fn ordinary_source_files_are_not_secrets() {
        for name in [
            "main.rs",
            "environment.ts",
            "keyboard.py",
            "monkey.go",
            "README.md",
            "Cargo.toml",
            "key_bindings.json",
            ".gitignore",
        ] {
            assert!(
                !is_secret_file_name(name),
                "expected {name:?} to be indexable"
            );
        }
    }

    #[test]
    fn a_secret_directory_anywhere_in_the_path_disqualifies_the_file() {
        assert!(is_secret_path("home/.ssh/config"));
        assert!(is_secret_path(".aws/config"));
        assert!(is_secret_path("infra/.kube/settings.yaml"));
        assert!(is_secret_path("src/.env"));
        assert!(!is_secret_path("src/ssh/client.rs"));
        assert!(!is_secret_path("src/main.rs"));
    }

    #[test]
    fn generated_bundles_are_recognised() {
        assert!(is_generated_file_name("app.min.js"));
        assert!(is_generated_file_name("theme.MIN.CSS"));
        assert!(is_generated_file_name("app.js.map"));
        assert!(!is_generated_file_name("minify.rs"));
        assert!(!is_generated_file_name("mapper.go"));
    }

    #[test]
    fn binary_content_is_detected_by_a_nul_byte() {
        assert!(looks_binary(b"MZ\x90\x00\x03"));
        assert!(!looks_binary(b"fn main() {}\n"));
        // A NUL past the sniff window is not this function's problem; the
        // UTF-8 decode that follows catches the rest.
        let mut late = vec![b'a'; BINARY_SNIFF_BYTES + 16];
        late[BINARY_SNIFF_BYTES + 8] = 0;
        assert!(!looks_binary(&late));
    }

    #[test]
    fn minified_files_are_detected_by_line_length() {
        let normal = "fn main() {\n    println!(\"hello\");\n}\n";
        assert!(!looks_minified(normal));
        assert_eq!(longest_line_bytes(normal), 22);

        let minified = format!("var a=1;{}\n", "x".repeat(MAX_LINE_BYTES));
        assert!(looks_minified(&minified));
    }

    #[test]
    fn pem_private_key_blocks_are_detected_at_line_start_only() {
        assert!(contains_private_key_block(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\n-----END RSA PRIVATE KEY-----\n"
        ));
        assert!(contains_private_key_block(
            "  -----BEGIN OPENSSH PRIVATE KEY-----\n"
        ));
        // The case that would otherwise make this module unindexable by its
        // own plugin.
        assert!(!contains_private_key_block(
            "const MARKER: &str = \"-----BEGIN RSA PRIVATE KEY-----\";\n"
        ));
        assert!(!contains_private_key_block(
            "-----BEGIN CERTIFICATE-----\nMIIB==\n"
        ));
    }

    #[test]
    fn vendored_and_version_control_directories_are_distinguished() {
        assert!(is_version_control_directory(".git"));
        assert!(!is_vendored_directory(".git"));
        assert!(is_vendored_directory("node_modules"));
        assert!(is_vendored_directory("target"));
        assert!(!is_vendored_directory("src"));
        assert!(is_secret_directory(".SSH"));
    }
}
