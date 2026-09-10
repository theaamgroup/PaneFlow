# Session-to-tab rename verification

Verified on macOS on 2026-09-10 for issue #496 and PR #498, using the
debug binary built from `d862387e`. These are original window captures,
not mockups. The final captures used a temporary app bundle containing
the same binary so macOS would reliably foreground its window.

The synthetic terminal emitted OSC 2 title changes through a real PTY.
The test instance used its own IPC socket and temporary debug state;
the original debug state was restored and verified byte-for-byte afterward.

## Observed behavior

- A single-pane tab initially displayed its stored label, `Before rename`.
  Changing the session title changed both its tab row and pane header to
  `Renamed session`. A second rename also updated both.
- Renaming either pane in a split left the stored tab label unchanged.
- A long session name truncated rather than wrapping in the tab row and
  pane header at the 800 by 500 minimum content size. The header was also
  checked with the primary sidebar hidden.
- PaneFlow Dark, PaneFlow Light, Cursor Dark, and Cursor Light were checked
  with `macos_chrome_material` both on and off. `reduce_motion` was on
  throughout. This change adds no controls, geometry, or animation.

The zoomed-split case is covered by the GPUI regression test
`session_title_changes_preserve_split_tab_names_even_when_zoomed`.
The live checks above concern the changed title-bearing surfaces; they do
not claim a new VoiceOver, keyboard-navigation, or dock-layout audit.

## Captures

| Scenario | Capture |
| --- | --- |
| Stored name before the session rename | [Before](before.png) |
| PaneFlow Dark, material on / off | [On](paneflow-dark-material-on.png) · [Off](paneflow-dark-material-off.png) |
| PaneFlow Light, material on / off | [On](paneflow-light-material-on.png) · [Off](paneflow-light-material-off.png) |
| Cursor Dark, material on / off | [On](cursor-dark-material-on.png) · [Off](cursor-dark-material-off.png) |
| Cursor Light, material on / off | [On](cursor-light-material-on.png) · [Off](cursor-light-material-off.png) |
| Renamed split pane with unchanged tab | [Split](split-tab.png) |
| Minimum size with a long title | [Sidebar visible](minimum-long-title.png) · [Sidebar hidden](minimum-hidden-sidebar.png) |

## Engineering evidence

[CI run 34541965372](https://github.com/theaamgroup/PaneFlow/actions/runs/34541965372)
passed formatting, all-target Clippy with warnings denied, the full workspace
test suite (including both new regression tests), libghostty PTY smoke,
the release build, the platform census, and the dependency audit.

The earlier local full-suite and all-target lint attempts stalled; the
successful CI run supplies those results. The local build, focused tests,
standard strict Clippy, formatting, and dependency audit also passed.
