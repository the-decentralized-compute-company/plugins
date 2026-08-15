# Contributing

This repository holds the plugin author guide and its runnable examples. Fixes
and clarifications are welcome.

## What belongs here

- Corrections to `README.md` when it disagrees with the code.
- Improvements to the examples in `examples/`.
- A new example, if it demonstrates a surface the existing two do not.

## What belongs elsewhere

- A bug in a first-party plugin — open it in that plugin's own repository
  (`blackboard`, `openai-endpoint`, `flash-moe`, `metrics`, `agents`).
- A bug in the SDK, installer, host projection, or console — open it against
  the main TDCC repository.
- A new plugin of your own — publish it in your own repository and add a
  catalog entry. This repository is not a plugin registry.

## Ground rules

**Accuracy over completeness.** Everything in `README.md` is meant to be
verifiable against the `tdcc-plugin` SDK, the plugin manager, or the host
runtime. If you cannot point at the code or the CLI that makes a statement
true, do not add it. Do not invent version numbers, benchmarks, or planned
features.

**Examples must build and install.** Before opening a pull request:

```bash
cd examples/<example>
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

Then package the example and install it through the real validation boundary,
with an isolated store so you do not disturb your own installation:

```bash
TDCC_PLUGIN_DIR=/tmp/plugin-store tdcc plugins install \
  --archive ./target/<example>-<version>-local.tar.gz \
  --name <example> --version <version>
TDCC_PLUGIN_DIR=/tmp/plugin-store tdcc plugins info <example>
```

An example that declares a web UI must report
`"validation": { "status": "valid" }` in its stored install record.

**Match the surrounding style.** Comment the non-obvious decision, not the
obvious line. Keep tests beside the code they cover.

## Pull requests

One topic per pull request. In the description, say what you verified and paste
the real output — including exit codes — rather than describing what should
happen. If something is untested on your platform, say so.

By contributing you agree that your contribution is licensed under
[Apache-2.0](LICENSE).
