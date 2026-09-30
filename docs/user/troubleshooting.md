# Troubleshooting

> Diagnose PaneFlow build, launch, rendering, configuration, shortcut, theme, and PATH issues on macOS with the shortest confirmed fix first.

Start with the symptom, confirm it, then apply the matching fix.

| Symptom | Confirm | First fix |
| --- | --- | --- |
| Build fails on a Metal shader | `xcrun metal -c` on a scratch file errors | Install full Xcode, then run `xcodebuild -downloadComponent MetalToolchain`. See below. |
| Text renders as empty boxes | `paneflow self-test glyphs` exits 1 | `gpui_platform` was built without the `font-kit` feature. See below. |
| Config change ignored | Validate `paneflow.json`, and check which build you are running | Fix the path or JSON syntax. Debug builds read `paneflow-dev`, not `paneflow`. |
| Shortcut does nothing | Compare against the keybindings reference | Use a known action name and a parseable key chord. |
| Theme change ignored | Save `paneflow.json` and wait one second | Use a bundled theme name and verify file watching. |
| `paneflow` not found | `paneflow --version` | Symlink the bundled binary into `/usr/local/bin`. |
| macOS blocks the app | Gatekeeper dialog | Open once from Finder or remove the quarantine attribute. |
| An agent still lists a `paneflow` MCP server | Its config still has a `paneflow` entry that runs `paneflow-mcp` | Read the cleanup's log lines, then remove the entry by hand. See below. |

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
noted next to the `gpui_platform` line in `src-app/Cargo.toml`. Check that
the feature is present rather than hunting for a font problem.

Confirm with the glyph self-test, which rasterizes sample text in the
bundled fonts without opening a window. A healthy build prints
`paneflow self-test glyphs: ok (...)` and exits 0; a build that draws
empty glyphs lists each empty glyph and exits 1. CI's render smoke lane
runs the same command on the bundled release binary.

```bash
dist/PaneFlow.app/Contents/MacOS/paneflow self-test glyphs
```

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

`window_backdrop` is read once at startup.
Restart PaneFlow after changing this key.

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
Terminal, Search, and Diff bindings are scoped.

See [keybindings.md](keybindings.md) for the action names.

### Why is my theme not hot-reloading?

PaneFlow ships four presets in two variants each: `"PaneFlow Dark"`,
`"PaneFlow Light"`, `"Vercel Dark"`, `"Vercel Light"`, `"Claude Dark"`,
`"Claude Light"`, `"Cursor Dark"`, and `"Cursor Light"`. Runtime lookup
is case-insensitive, but canonical names keep schema validation clean.
See [themes.md](themes.md) for the bundled set.

Theme and typography changes hot-reload from `paneflow.json`. PaneFlow
watches the config directory and debounces changes for 300 ms. For the
theme, a save that is not valid JSON is ignored until the file parses
again, and if the config watcher cannot start, a hand-edited theme does
not hot-reload until a restart.

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

### Why does an agent still list a `paneflow` MCP server?

PaneFlow no longer ships its MCP bridge. On launch, while
`~/Library/Application Support/paneflow/bin/paneflow-mcp` exists, PaneFlow
removes the `paneflow` MCP entry it wrote to Claude Code, Codex, Gemini CLI,
and opencode. It removes only a `paneflow` entry whose command is a
`paneflow-mcp` binary, at any path; a `paneflow` entry that runs anything else
is yours and stays. In `~/.claude.json`, Gemini's `settings.json`, and
opencode's `.json` or `.jsonc` it removes only that entry's text, so comments,
formatting, number spelling, and key order stay byte-for-byte.

The binary goes in two steps. The launch that removes entries keeps it. The
next launch checks again, which also catches an agent that wrote an old config
back, and deletes the binary only when it finds no entry. So the entries go on
the first launch and `paneflow-mcp` on the one after.

Before changing a file it saves a backup next to it: `<file>.bak` if that name
is free, otherwise `<file>.paneflow-bak`, then `<file>.paneflow-bak.1`, `.2`,
and so on. It never overwrites an existing file, and a write it then refuses
deletes the backup it just made.

It always checks each agent's default location (`~/.claude.json`,
`~/.codex/config.toml`, `~/.gemini/settings.json`, and `opencode.json` or
`opencode.jsonc` in `~/.config/opencode/`). It also checks the locations named
by the environment PaneFlow was launched with and by the `CLAUDE_CONFIG_DIR`,
`CODEX_HOME`, `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`, and `XDG_CONFIG_HOME`
values your login shell sets, so an override never hides the default file.
PaneFlow reads those login-shell values only when it is launched from the Dock
or Finder with launchd's default `PATH`, the same capture that imports `PATH`.
After `open --env PATH=…` or `launchctl setenv PATH …` it does not read them, a
launch from a terminal already inherits them, and a relative value is ignored.
An entry in a config the pass cannot see stays, and can end up pointing at a
deleted binary; remove it by hand (below).

If it cannot remove an entry, it keeps the binary so that entry keeps working,
and the next launch retries. That happens when the file does not parse, has a
duplicate key, or is larger than 64 MiB; when Codex holds the entry in an
inline `mcp_servers = {…}` table; or when the agent rewrote the file during the
pass. Debug builds (unless `PANEFLOW_ALLOW_DEBUG_MCP_INSTALL=1`) and runs with
`PANEFLOW_HOME` set never edit agent configs, so they keep the binary too.

To see why an entry stayed, quit PaneFlow and start it from Terminal:

```bash
RUST_LOG=warn,paneflow_mcp_install=info /Applications/PaneFlow.app/Contents/MacOS/paneflow
```

The default log filter shows warnings only, and the cleanup logs its changes
at `info`, so the second directive is what shows them. The cleanup logs:

| Level | Line |
| --- | --- |
| info | `mcp bridge cleanup: removed the paneflow entry from {agent} ({file}; backup {backup})` |
| warn | `mcp bridge cleanup: kept the {agent} entry: {reason}` |
| info | `mcp bridge cleanup: kept {binary} until a later launch finds no entry` |
| info | `mcp bridge cleanup: kept {binary} because an entry may still point at it` |
| info | `mcp bridge cleanup: deleted {binary}` |
| warn | `mcp bridge cleanup: could not delete {binary} ({error}); the next launch retries` |
| warn | `mcp bridge cleanup: could not remove the unused backup {backup} ({error})` |
| warn | `paneflow: could not start the MCP bridge cleanup: {e}` |

`{agent}` is `Claude Code`, `Codex`, `Gemini CLI`, or `opencode`. Typical
`{reason}` values:

- `debug builds do not edit agent configs without PANEFLOW_ALLOW_DEBUG_MCP_INSTALL=1`
- `PANEFLOW_HOME is set, so this run does not edit agent configs under the real home`
- `{file} changed while it was being edited; left it as it is (the next launch retries)`
- `{file} is not valid JSON or JSONC - refusing to overwrite it; fix or remove it, then re-run`
  (or `… not valid TOML …`)
- `` `mcp_servers` is not a TOML table - refusing to overwrite ``
- `read {file} failed: {file} is too large (N bytes; maximum 67108864)`

To remove a leftover entry by hand:

| Agent | Remove it with |
| --- | --- |
| Claude Code | `claude mcp remove -s user paneflow` |
| Codex | `codex mcp remove paneflow`, or delete the `[mcp_servers.paneflow]` table from `~/.codex/config.toml` (`$CODEX_HOME/config.toml` when `CODEX_HOME` is set) |
| Gemini CLI | Delete `mcpServers.paneflow` from `~/.gemini/settings.json` |
| opencode | Delete `mcp.paneflow` from `opencode.jsonc` or `opencode.json` in `~/.config/opencode/` (`$XDG_CONFIG_HOME/opencode/` when `XDG_CONFIG_HOME` is set), and from the file `OPENCODE_CONFIG` names or the one in `OPENCODE_CONFIG_DIR` if you set either |

Once no entry is left, the next launch deletes the binary.

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
