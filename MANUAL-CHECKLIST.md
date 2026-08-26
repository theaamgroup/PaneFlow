# Manual checklist

Things a machine could not establish. Each item has exact steps and a
falsifiable expected result. **Do not tick these on the agent's behalf.**

Created by task 16. Add an item the moment a task produces an unverifiable
claim. If an item turns out to be automatable, automate it and delete the
entry.

---

- [ ] **Task 6 — Cmd+Tab next-workspace**
  - Commit: still advertised in `--help` (`Cmd+Tab`) and `defaults.rs` `secondary-tab`
  - Why a machine cannot do this: Cmd+Tab is the chord macOS intercepts for
    the application switcher. A synthetic System Events keystroke does not
    answer whether the action reaches PaneFlow.
  - Steps: focus a PaneFlow window that has at least two workspaces. Press
    Cmd+Tab on the physical keyboard.
  - Expected: either the next workspace becomes active, **or** macOS's app
    switcher appears. Record which. If the switcher appears, the binding
    does not reach the app.
  - Failure implicates: `src-app/src/keybindings/defaults.rs` `secondary-tab`
    and the `--help` copy that advertises it.

- [ ] **Keyboard-driven pane flows**
  - Commit: (standing)
  - Steps: with a PaneFlow window focused, press Cmd+Shift+D (split
    horizontal), Cmd+Shift+E (split vertical), Cmd+Shift+W (close pane),
    Cmd+Shift+T (undo close).
  - Expected: a new pane appears below / beside; the focused pane closes;
    undo restores it.
  - Failure implicates: keybindings + layout mutations.

- [ ] **PATH-shim agent turn**
  - Commit: (standing)
  - Steps: launch a real agent CLI in a pane through the PATH shim. Watch
    the sidebar through one full turn.
  - Expected: the sidebar tracks the agent from start through tool use to
    idle/waiting.
  - Failure implicates: paneflow-shim + ai-hook + sidebar.

- [ ] **Task 12 — bundle-id permission re-grant**
  - Commit: (pending)
  - Steps: after the first signed/notarized build with
    `com.theaamgroup.paneflow`, launch it. macOS will re-prompt for
    Accessibility / Automation / Full Disk Access once.
  - Expected: prompts appear (this is expected, not a regression). After
    granting, the app still works.
  - Failure implicates: the new CFBundleIdentifier / TCC mapping.

- [ ] **Task 11 — no in-app updater (deleted 2026-08-26)**
  - Commit: updater removal on `issue-63` (bucket 3). The feed, minisign
    client, title-bar update pill, and `spawn_check` are gone. There is no
    `src-app/src/update/` left to poll GitHub.
  - Why a machine cannot fully do this: the agent can grep the binary and
    logs; it cannot watch outbound HTTPS (Little Snitch / a proxy).
  - Steps: launch the app. Confirm there is no title-bar "Update available"
    / "vX available" pill, and the log has no `spawn_check` line and no
    `self-update feed disabled` line. Optionally watch outbound HTTPS.
  - Expected: no request to `api.github.com/repos/arthjean/paneflow` or any
    other GitHub releases endpoint in normal use. Help → What’s New is a
    browser link (`RELEASES_URL`), not a feed poll.
  - Failure implicates: leftover updater wiring that should have been
    deleted with bucket 3.

- [ ] **Task 20 — unsigned .dmg first open**
  - Commit: local artifact at `dist/paneflow-0.1.0-aarch64-apple-darwin.dmg` (30M)
  - Agent observed: `scripts/bundle-macos.sh --version 0.1.0 --arch aarch64`
    produced `dist/PaneFlow.app` with `CFBundleIdentifier=com.theaamgroup.paneflow`
    and `--version` `paneflow 0.1.0`. `create-dmg.sh` wrote the .dmg then exited
    1 at `codesign --verify --deep --strict` because the enclosed binary is
    adhoc/linker-signed. That check is for a signed+notarized release; skip it
    for an unsigned smoke. Gatekeeper will still quarantine a copied .dmg.
  - Steps: open the local unsigned .dmg. Gatekeeper will quarantine it.
    Right-click → Open, or `xattr -d com.apple.quarantine` on the .app,
    then launch from /Applications.
  - Expected: the app launches; `PaneFlow.app/Contents/MacOS/paneflow --version`
    prints `0.1.0`; glyphs render (not empty boxes).
  - Failure implicates: bundle script, embed staging, or font-kit feature.

---

## Automated visual smoke (not a checkbox)

Done at the start of this run, 2026-08-25, against HEAD `35af5bb`:

- Debug binary `./target/debug/paneflow` (paneflow 0.8.2) launched with
  `PANEFLOW_ALLOW_MULTIPLE=1` and `PANEFLOW_SOCKET_PATH=/tmp/paneflow-head-smoke.sock`.
- Screenshot: `/tmp/pf-screenshots/head-debug-window.png`.
- Observed: window opens; sidebar lists workspace `paneflow` on `main`;
  tab strip shows `paneflow`; terminal prompt
  `dayers@Davids-MacBook-Pro paneflow %` renders as real glyphs, not
  empty boxes.
- Log: `font: resolved family='JetBrainsMono Nerd Font Mono'`;
  `Terminal backend selected: … effective=alacritty`;
  `Assets::load_fonts: registered 36 embedded font file(s)`.
- Historical (HEAD `35af5bb`, before leftover-removal deleted the
  updater): `update::checker up to date (v0.8.2)`. That feed is gone;
  Task 11 above is the current check.
