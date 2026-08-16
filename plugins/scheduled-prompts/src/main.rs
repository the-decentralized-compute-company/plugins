//! `scheduled-prompts` — let a node do useful work on a schedule instead of
//! only when someone is watching.
//!
//! The operator declares jobs in a file: a cron expression, a prompt, a model,
//! and where the answer goes. This process wakes on a timer, decides which jobs
//! are due, runs at most `max_concurrent_runs` of them at once, never overlaps
//! a job with itself, and writes a bounded record of what happened.
//!
//! **A model cannot create a job.** There is no tool that adds, edits, or
//! deletes one, and `resume` cannot start a job the file disabled. README.md
//! says why at length; the short version is that a model able to schedule its
//! own future execution has arranged to run again, with a prompt it wrote,
//! with nobody present, on hardware somebody else paid for.
//!
//! Run it the way the host does (no arguments beyond `[[plugin]].args`): the
//! runtime connects to `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and
//! serves the manifest. Run it with `--print-package-manifest` to emit the
//! `plugin-manifest.json` that would go in a release archive — for this plugin
//! that is `{}`, because it declares neither a config schema nor a web UI, so
//! the file may be left out entirely.
//!
//! Layout:
//!
//! * `config`    — process settings from `[[plugin]].args` and the environment
//! * `jobs`      — the jobs file: parse, validate, refuse
//! * `cron`      — the schedule expression and its next occurrence
//! * `clock`     — zones, windows, and the one naive-to-instant mapping
//! * `decide`    — whether a job runs now, as a pure function
//! * `history`   — bounded, rolled-up run history on disk
//! * `openai`    — the one completion request
//! * `sink`      — where the answer goes, and the confinement around it
//! * `scheduler` — the tick loop, the concurrency cap, and the tool answers
//! * `manifest`  — what the host projects

mod clock;
mod config;
mod cron;
mod decide;
#[cfg(test)]
mod end_to_end;
mod history;
mod jobs;
mod manifest;
mod openai;
mod scheduler;
mod sink;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::clock::now_ms;
use crate::config::{Command, Config, EnvMap};
use crate::history::Store;
use crate::jobs::{JobsFile, parse_jobs};
use crate::scheduler::{JobsSource, Scheduler};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_process()
        .map_err(|error| anyhow::anyhow!("{error}\n\n{}", config::USAGE))
        .context("scheduled-prompts configuration")?;

    match config.command {
        Command::Help => {
            print!("{}", config::USAGE);
            Ok(())
        }
        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so packaged metadata cannot drift
        // from the running manifest. It deliberately does not read the jobs
        // file — packaging a plugin must not depend on a node's schedule.
        Command::PrintPackageManifest => {
            let scheduler = Scheduler::new(
                config,
                JobsSource {
                    file: JobsFile::empty(),
                    error: None,
                    present: false,
                },
                Store::new(
                    std::env::temp_dir()
                        .join(format!("scheduled-prompts-manifest-{}", std::process::id())),
                ),
            )?;
            let plugin = manifest::scheduled_prompts_plugin(Arc::new(scheduler));
            let rendered = plugin.manifest().context("scheduled-prompts manifest")?;
            println!("{}", package_manifest_json(&rendered)?);
            Ok(())
        }
        // Runtime path. Startup lines go to stderr, where the host's log picks
        // them up: an operator has to be able to tell "jobs loaded" from "jobs
        // did not load" without calling a tool.
        Command::Run => {
            let (source, messages) = load_jobs(&config.jobs_path);
            for message in messages {
                eprintln!("scheduled-prompts: {message}");
            }

            let store = Store::new(config.state_dir.clone());
            let scheduler = Arc::new(Scheduler::new(config, source, store)?);
            PluginRuntime::run(manifest::scheduled_prompts_plugin(scheduler)).await
        }
    }
}

/// Read and validate the jobs file, and say plainly what happened.
///
/// Three outcomes, each with a different posture:
///
/// * **No file.** Not an error. Installing an unconfigured plugin must never be
///   the reason a node changes behaviour, so it starts with zero jobs and
///   `status` says where it looked.
/// * **A file that loads.** Its jobs are scheduled.
/// * **A file that does not load.** The scheduler does not start, and every
///   tool reports the error. Running the half of the operator's schedule that
///   happened to parse would be the worst of the three outcomes — this way the
///   failure is loud, and nothing runs that the operator did not fully write.
fn load_jobs(path: &Path) -> (JobsSource, Vec<String>) {
    let mut messages = Vec::new();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            messages.push(format!(
                "no jobs file at {}, so nothing is scheduled. Create it and restart the node; \
                 `status` reports the path it looked at.",
                path.display()
            ));
            return (
                JobsSource {
                    file: JobsFile::empty(),
                    error: None,
                    present: false,
                },
                messages,
            );
        }
        Err(error) => {
            let message = format!("cannot read {}: {error}", path.display());
            messages.push(format!("{message} — no job will run"));
            return (
                JobsSource {
                    file: JobsFile::empty(),
                    error: Some(message),
                    present: true,
                },
                messages,
            );
        }
    };

    let env: EnvMap = std::env::vars().collect();
    match parse_jobs(&text, &env, now_ms()) {
        Ok(file) => {
            let enabled = file.jobs.iter().filter(|job| job.enabled).count();
            messages.push(format!(
                "loaded {} job(s) from {} ({enabled} enabled, timezone {}, up to {} concurrent \
                 run(s))",
                file.jobs.len(),
                path.display(),
                file.zone.as_str(),
                file.max_concurrent_runs
            ));
            (
                JobsSource {
                    file,
                    error: None,
                    present: true,
                },
                messages,
            )
        }
        Err(error) => {
            messages.push(format!(
                "{} did not load, so NO job will run: {error}",
                path.display()
            ));
            (
                JobsSource {
                    file: JobsFile::empty(),
                    error: Some(error),
                    present: true,
                },
                messages,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tdcc-scheduled-prompts-main-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn a_missing_jobs_file_starts_the_plugin_with_nothing_scheduled() {
        let dir = scratch("missing");

        let (source, messages) = load_jobs(&dir.join("nothing-here.toml"));

        assert!(!source.present);
        assert_eq!(source.error, None, "a missing file is not a failure");
        assert!(source.file.jobs.is_empty());
        assert!(messages[0].contains("nothing is scheduled"), "{messages:?}");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_valid_file_is_loaded_and_summarised_on_stderr() {
        let dir = scratch("valid");
        let path = dir.join("jobs.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             timezone = \"utc\"\n\
             [[job]]\n\
             id = \"digest\"\n\
             schedule = \"0 3 * * *\"\n\
             model = \"qwen3:8b\"\n\
             prompt = \"Summarise.\"\n\
             sink = { kind = \"file\", path = \"digest.md\" }\n",
        )
        .expect("write");

        let (source, messages) = load_jobs(&path);

        assert!(source.present);
        assert_eq!(source.error, None);
        assert_eq!(source.file.jobs.len(), 1);
        assert!(messages[0].contains("loaded 1 job(s)"), "{messages:?}");
        assert!(messages[0].contains("1 enabled"), "{messages:?}");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_file_that_does_not_load_schedules_nothing_and_says_so_loudly() {
        let dir = scratch("broken");
        let path = dir.join("jobs.toml");
        std::fs::write(
            &path,
            "version = 1\n\
             [[job]]\n\
             id = \"digest\"\n\
             scheduel = \"0 3 * * *\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"a.md\" }\n",
        )
        .expect("write");

        let (source, messages) = load_jobs(&path);

        assert!(source.present);
        let error = source.error.expect("the parse error is kept");
        assert!(error.contains("scheduel"), "{error}");
        assert!(source.file.jobs.is_empty(), "nothing partial is scheduled");
        assert!(messages[0].contains("NO job will run"), "{messages:?}");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
