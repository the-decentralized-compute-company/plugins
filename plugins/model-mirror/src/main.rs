//! `model-mirror` — one node holds a model cache and serves it to peers, so a
//! 20 GB artifact crosses the origin's link once instead of once per node.
//!
//! Run it the way the host does (with the plugin's configured `args`): the
//! runtime connects to `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and
//! serves the manifest. Run it with `--print-package-manifest` to emit the
//! `plugin-manifest.json` that would go in a release archive, or with `--help`
//! for the operator limits.

mod announce;
mod artifact;
mod cache;
mod digest;
mod manifest;
mod options;
mod policy;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::cache::MirrorCache;
use crate::manifest::model_mirror_plugin;
use crate::options::{MirrorOptions, Platform, parse_options};

const USAGE: &str = "\
model-mirror — mirror model artifacts to mesh peers, digest-verified.

Configured through [[plugin]].args in ~/.tdcc/config.toml, because host-owned
[plugin.settings] values never reach a plugin process and every limit below has
to be enforced in-process.

  --cache-dir <path>                Where this mirror stores artifacts.
                                    Default: <platform cache dir>/tdcc/model-mirror
  --import-root <path>              Directory `import` may read from. Repeatable.
                                    Default: the Hugging Face hub cache.
  --max-cache-bytes <size>          Disk this node contributes. Default 0, which
                                    means it holds and serves nothing.
  --max-chunk-bytes <size>          Ceiling on one transfer chunk. Defaults to
                                    the 8MiB hard maximum; a caller that names
                                    no length gets 1MiB.
  --serve-bytes-per-minute <size>   Outbound cap. Default 64MiB (~8.9 Mbit/s).
                                    0 means unlimited.
  --reverify-after-secs <n>         Re-digest before serving when the last full
                                    verification is older than this. Default 86400.
  --no-advertise                    Hold and serve, but do not announce holdings
                                    on the mesh.
  --help                            Print this and exit.

Sizes accept plain bytes, binary suffixes (KiB, MiB, GiB, TiB), and decimal
suffixes (KB, MB, GB, TB).
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        // Packaging path: the same `plugin!` declaration the runtime registers
        // also produces `plugin-manifest.json`, so packaged metadata cannot
        // drift from the running manifest. It is built against a throwaway
        // cache directory so packaging never touches an operator's real store.
        Some("--print-package-manifest") => print_package_manifest().await,
        _ => {
            let options = parse_options(&args, |key| std::env::var(key).ok(), Platform::current())
                .context("read model-mirror options from [[plugin]].args")?;
            announce_configuration(&options);
            let cache = MirrorCache::open(options)
                .await
                .context("open the model-mirror cache")?;
            PluginRuntime::run(model_mirror_plugin(cache)).await
        }
    }
}

async fn print_package_manifest() -> Result<()> {
    let scratch = std::env::temp_dir().join("model-mirror-package-manifest");
    let options = MirrorOptions {
        cache_dir: scratch.clone(),
        import_roots: Vec::new(),
        max_cache_bytes: 0,
        max_chunk_bytes: crate::policy::MAX_CHUNK_BYTES_CEILING,
        serve_bytes_per_minute: 0,
        reverify_after_secs: crate::policy::DEFAULT_REVERIFY_AFTER_SECS,
        advertise: false,
    };
    let cache = MirrorCache::open(options)
        .await
        .context("open a scratch cache to render the package manifest")?;
    let plugin = model_mirror_plugin(cache);
    let manifest = plugin.manifest().context("model-mirror manifest")?;
    println!("{}", package_manifest_json(&manifest)?);
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    Ok(())
}

/// Say out loud what this node is about to contribute.
///
/// An operator who forgot `--max-cache-bytes` should learn it from one line in
/// the log, not from a tool call failing an hour later.
fn announce_configuration(options: &MirrorOptions) {
    if options.holds_artifacts() {
        eprintln!(
            "model-mirror: up to {} bytes of cache in {}, serving at up to {} bytes/minute{}",
            options.max_cache_bytes,
            options.cache_dir.display(),
            if options.serve_bytes_per_minute == 0 {
                "unlimited".to_string()
            } else {
                options.serve_bytes_per_minute.to_string()
            },
            if options.advertise {
                ""
            } else {
                ", not advertising"
            }
        );
    } else {
        eprintln!(
            "model-mirror: holding nothing — pass --max-cache-bytes in [[plugin]].args to \
             contribute disk to the mesh"
        );
    }
}
