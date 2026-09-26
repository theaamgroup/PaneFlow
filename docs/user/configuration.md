# Configuration

PaneFlow reads a single JSON file at startup: `paneflow.json`. Every key
is optional and has a sensible default, so an empty `{}` is a valid
config, and having no config file at all is also valid.

| Build | Path |
|---|---|
| Release | `~/Library/Application Support/paneflow/paneflow.json` |
| Debug (`cargo run`) | `~/Library/Application Support/paneflow-dev/paneflow.json` |

Debug builds namespace themselves separately (`APP_SUBDIR` in
`crates/paneflow-config/src/loader.rs`), so a from-source `cargo run`
build does not read the release config. `PANEFLOW_HOME` can relocate app-owned
configuration, data, and cache roots for isolated runs; see the [runtime walkthrough](configuration/runtime.md).

Most keys hot-reload on save. `window_backdrop`
and `crash_reporting` are read once at startup and require a restart. Invalid
JSON at startup logs a warning and falls back to defaults; an invalid save during hot reload
keeps the last valid config.

The full reference, including types, defaults, and per-key notes, is on
the [schema page](configuration/schema.md).

The [runtime walkthrough](configuration/runtime.md) describes hot reload,
key recording, review state, and sidebar metadata.
