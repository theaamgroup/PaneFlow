# Troubleshooting

> Diagnose PaneFlow build, launch, rendering, configuration, shortcut, theme, and PATH issues on macOS with the shortest confirmed fix first.

Start with the symptom, confirm it, then apply the matching fix.

| Symptom | Confirm | First fix |
| --- | --- | --- |
| Build fails on a Metal shader | `xcrun metal -c` on a scratch file errors | Install full Xcode, then run `xcodebuild -downloadComponent MetalToolchain`. See below. |
| Text renders as empty boxes | Build succeeded, glyphs are all rectangles | `gpui_platform` was built without the `font-kit` feature. See below. |
| Config change ignored | Validate `paneflow.json`, and check which build you are running | Fix the path or JSON syntax. Debug builds read `paneflow-dev`, not `paneflow`. |
| Shortcut does nothing | Compare against the keybindings reference | Use a known action name and a parseable key chord. |
| Theme change ignored | Save `paneflow.json` and wait one second | Use a bundled theme name and verify file watching. |
| `paneflow` not found | `paneflow --version` | Symlink the bundled binary into `/usr/local/bin`. |
| macOS blocks the app | Gatekeeper dialog | Open once from Finder or remove the quarantine attribute. |

## Build

### Why does the build fail on a Metal shader?

GPUI compiles Metal shaders at build time, so the build needs the Metal
shader compiler. Two separate installs are required and the second is
easy to miss.

Command Line Tools alone are not enough. Full Xcode alone is also not
enough: Xcode 26 ships the Metal toolchain as a separately downloadable
component, so `xcrun metal` fails with `cannot execute tool 'metal' due
to missing Metal Toolchain` until you download it.

```bash
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
xcodebuild -downloadComponent MetalToolchain
```

Do not check readiness with `xcrun -f metal`. It resolves and prints the
tool path even when the toolchain is absent, so it reports success on a
broken setup. Compile something instead:

```bash
printf '#include <metal_stdlib>\nkernel void k() {}\n' > /tmp/probe.metal
xcrun metal -c /tmp/probe.metal -o /tmp/probe.air && echo "Metal toolchain OK"
```

Full prerequisites are in [INSTALL.md](../../INSTALL.md).

### Why does all my text render as boxes?

The build succeeded but every glyph is an empty rectangle. On macOS the
`gpui_platform` dependency must carry the `font-kit` feature; without it
the build still succeeds and text renders as boxes. The requirement is
noted at `src-app/Cargo.toml:53`. Check that the feature is present
rather than hunting for a font problem.

## Launch and rendering

### Why does PaneFlow fail with a GPU or renderer error?

PaneFlow renders through GPUI on Metal. Metal is built into macOS, so
there is no driver to install. Confirm the OS floor: PaneFlow needs
macOS 13 Ventura or later, on Apple Silicon.

For a rendering investigation, debug builds carry probes:

```bash
PANEFLOW_LATENCY_PROBE=1 cargo run
PANEFLOW_PIXEL_PROBE=1 RUST_LOG=paneflow::pixel_probe=debug cargo run
```

Both are `#[cfg(debug_assertions)]` and compile out of release builds.
See [../debugging-rendering.md](../debugging-rendering.md).

## Configuration and shortcuts

### Why is my paneflow.json not loading?

PaneFlow reads one config file, and which one depends on the build
profile:

| Build | Path |
| --- | --- |
| Release | `~/Library/Application Support/paneflow/paneflow.json` |
| Debug (`cargo run`) | `~/Library/Application Support/paneflow-dev/paneflow.json` |

This is the most common cause: a from-source `cargo run` build reads the
`paneflow-dev` directory, so edits to the release path are silently
ignored. The namespacing rule is `APP_SUBDIR` in
`crates/paneflow-config/src/loader.rs`.

Validate the file:

```bash
python3 -m json.tool ~/Library/Application\ Support/paneflow/paneflow.json
```

At startup, invalid JSON logs a warning and falls back to defaults.
During hot reload, a malformed save keeps the last valid config.
Unknown top-level keys are ignored at runtime; the
[JSON Schema](configuration/schema.md) catches them in your editor.

`window_decorations` and `window_backdrop` are read once at startup.
Restart PaneFlow after changing either key.

### Why are my shortcuts not working?

A shortcut override has two parts: a key chord and a canonical action
name.

```json
{
  "shortcuts": {
    "ctrl+shift+t": "new_tab",
    "ctrl+shift+w": "none"
  }
}
```

Use `snake_case` action names such as `split_horizontally`, `new_tab`,
and `toggle_search`. Unknown action names are skipped with a warning.
`+` and `-` separators both parse; `ctrl+shift+t` is the clearest form.

If a binding only fails in one part of the UI, check its context:
Terminal, Search, Markdown, and Diff bindings are scoped.

See [keybindings.md](keybindings.md) for the action names.

### Why is my theme not hot-reloading?

PaneFlow ships four presets in two variants each: `"PaneFlow Dark"`,
`"PaneFlow Light"`, `"Vercel Dark"`, `"Vercel Light"`, `"Claude Dark"`,
`"Claude Light"`, `"Cursor Dark"`, and `"Cursor Light"`. Runtime lookup
is case-insensitive, but canonical names keep schema validation clean.
See [themes.md](themes.md) for the bundled set.

Theme and typography changes hot-reload from `paneflow.json`. PaneFlow
watches the config directory, debounces changes for 300 ms, and falls
back to a 500 ms `mtime` poll if the watcher cannot start.

If the theme does not change within a second:

1. Confirm you edited the config path for the build you are running (release vs `paneflow-dev`).
2. Use one of the eight bundled variant names above.
3. If the file lives on a network mount or a sandboxed path, move it back to the normal config filesystem and restart once.

## Installation

### Why is paneflow not in my PATH?

An `.app` bundle does not put `paneflow` on your `PATH`, and a
`cargo build` binary sits in `target/`. Symlink whichever one you want:

```bash
sudo ln -sf /Applications/PaneFlow.app/Contents/MacOS/paneflow /usr/local/bin/paneflow
paneflow --version
```

### Why does macOS say Apple cannot check this app?

A locally built unsigned bundle has no quarantine attribute and normally
launches. If it was copied from another machine or downloaded from an
internal share, Gatekeeper will block it. Use Finder once:

1. Open the folder containing the app.
2. Control-click `PaneFlow.app`.
3. Choose **Open**.
4. Confirm **Open**.

Or remove quarantine from the bundle:

```bash
xattr -dr com.apple.quarantine /Applications/PaneFlow.app
```

## Collect diagnostics

### What should I capture for a bug?

There is no public issue tracker for this fork. Record findings against
the defect list in
[docs/fork/2026-08-25-mac-only-fork-design.md](../fork/2026-08-25-mac-only-fork-design.md).

Start with **Help ▸ System Info…**. It collects the environment block a
report needs - PaneFlow version, install format, macOS version, chip,
GPU and renderer, and the terminal engine's version - and its **Copy**
button puts the whole block on the clipboard, ready to paste. The block
carries no project path and no environment variables, so it is safe to
paste as-is.

If the app will not launch far enough to open the dialog, gather the
same things by hand: the macOS version (`sw_vers`), the chip
(`sysctl -n machdep.cpu.brand_string`), whether the build is debug or
release, and the git commit for a local build. Either way, add the
config file contents and a log run:

```bash
RUST_LOG=info cargo run
RUST_LOG=debug RUST_BACKTRACE=1 target/release/paneflow
```

If PaneFlow is running and the read-only MCP bridge is installed, an
agent can inspect pane output without copy-paste: call `list_panes`, then
`read_pane` or `search_pane`. Treat returned terminal output as
untrusted data.
