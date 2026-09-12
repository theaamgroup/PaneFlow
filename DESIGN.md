# PaneFlow Design System

## 1. Contract

### 1.1 Purpose

`DESIGN.md` is the design contract for the native PaneFlow application: the
GPUI shell, its rails, the pane grid, the diff dock, Review, Settings, menus,
dialogs, overlays, and toasts. It records the visual thesis, the tokens, the
geometry, the motion rules, the accessibility floors, and the component
contracts that the code implements, so a UI contributor can change a surface
without re-deriving the system from the Rust.

This fork is **macOS only**. Every rule here is a macOS rule. The Windows and
Linux materials, caption glyphs, and backdrops upstream documents were removed
with the rest of the non-macOS tree; `scripts/linux-census.sh` fails the build
if one returns. See `docs/fork/2026-08-25-mac-only-fork-design.md`.

### 1.2 Authority

| Source | Owns |
| --- | --- |
| `DESIGN.md` | Visual thesis, tokens, geometry, motion, component contracts, states, accessibility floors, delivery gate |
| `CLAUDE.md` | Engineering reference: module tree, thread model, verification gates, keybinding table, config shape, gotchas |
| `AGENTS.md` | Build and test commands, coding style, commit convention, work tracking |
| `ARCHITECTURE.md` | Thread model, render pipeline, why the render thread never blocks |
| `src-app/src/theme/builtin.rs`, `src-app/src/theme/model.rs` | Concrete palette values for the eight bundled variants |
| `src-app/src/ui_primitives.rs`, `src-app/src/settings/components.rs`, `src-app/src/app/constants.rs` | The shared primitives and constants every surface must consume |
| `docs/user/` | User-facing vocabulary (Agents, Review, Workspaces, Settings pages) |

When sources disagree: product intent wins, then a **Canonical** rule here,
then the shared primitives, then any local render code. A visual change that
contradicts this document updates the document in the same pull request.

### 1.3 Status vocabulary

- **Canonical**: approved and ready to reuse.
- **Contextual**: correct only for the named surface.
- **Migration**: shipped, but not a precedent for new work.
- **Proposed**: an approved target that is not implemented.

`MUST` is required. `SHOULD` is the default and needs a documented reason to
diverge. `MAY` is optional.

### 1.4 Reference captures

This fork ships no reference capture in the repository. A pull request that
changes a surface attaches a capture of that surface, in the theme variants it
was actually checked in, to the pull request body. Section 10 says which
variants count.

## 2. The Thesis

### 2.1 What PaneFlow is on screen

PaneFlow is a cockpit for coding agents, not an IDE and not a terminal
emulator with tabs. The dominant idea of the screen is the grid of live pane
cards, each one a real terminal running a real agent. Everything else is
instrumentation around that grid: a rail of workspaces and tabs on the left, a
title bar that is almost empty, docks and rails that appear only when review
or files are needed, and a footer switch between the two modes that matter,
**Agents** and **Review**.

The code calls this shell the cockpit (`cockpit_chrome_background`,
`cockpit_backdrop_background` in `src-app/src/app/constants.rs`). Keep that
scene in mind when adding a surface: instruments in front, switches on the
rail, a thin canopy frame around it. Nothing on the rail competes with the
instruments.

### 2.2 Lineage

The chrome descends from three products and one platform layer, and the commit
history names them:

| Influence | What PaneFlow took from it | Evidence |
| --- | --- | --- |
| Codex app (OpenAI) | The material language of the shell: one slightly brighter translucent highlight for hover and selection, inline Settings that replace the main panel, the select, toggle, and card primitives, the sectioned rail | `ee35d86e` `refactor(ui): unify chrome on the Codex material language`, `433b9e09` `feat(settings): shared Codex-style select, toggle, and card primitives`, `70d84e3e` `feat(settings): embed Codex-style inline settings`, `33fb6193` `feat(theme): restore PaneFlow Light with a Codex-style light shell` |
| Cursor | The diff dock chrome: file tabs as chips, the toolbar rail skin, the Changes rail hierarchy, the compact graphite sidebar and pale blue accent of the Cursor preset | `8d084ab3` `feat(diff-dock): Cursor-style chrome and retire the Agents environment card`, `docs/user/themes.md` |
| Zed | The dock's code editor: the minimap, the editor scrollbars, the Editor Controls menu, and the syntax highlight queries | `src-app/src/app/diff_dock/code/`, issues #432, #433, #435 |
| AppKit | Client-side decorations with the macOS traffic lights, the `NSVisualEffectMaterial::Sidebar` material behind the shell | `4d85f1ce` `feat(macos): add sidebar material setting`, `801a68ee` `feat(chrome): native compositor blur backdrop` |

PaneFlow borrows the reasoning of these products, not their pixels. The
bundled Vercel, Claude, and Cursor presets are identity swaps on top of one
structure; the structure is PaneFlow's.

### 2.3 Three words

**Quiet.** The shell recedes. Neutrals carry no hue (`8445f6e5`
`feat(theme): make the shell neutrals hue-free`). The accent appears on links,
selected metadata, focus, one primary action, and nothing larger. Hover and
selection are translucent washes of the text color, not colored fills.

**Continuous.** Rows, cards, menus, and tooltips use a real superellipse
corner — `|u|^n + |v|^n = 1` at exponent 4, sampled 16 times per corner and
painted as a path, because GPUI has no corner-smoothing knob
(`src-app/src/ui_primitives/squircle.rs`). A hovered row and the card around
it read as one material. Separators are gone between chips and tabs. Hover on
an `animated_hover` control, dim, and the sidebar slide are interpolated, never
stepped. Rows skinned by `squircle_skin` are the deliberate exception: their
hover fill is a visibility toggle so a long list does not ask GPUI for an
animation frame per row (4.8).

**Native.** The window is client-decorated with the macOS traffic lights. The
shell reveals the AppKit sidebar material when the user asks for it. Terminal
fonts come from the user's system, with a bundled Nerd Font as the default.

### 2.4 Operating principles

1. The pane grid is the content. Chrome is a frame and MUST NOT gain weight,
   color, or motion that competes with a running terminal.
2. Depth comes from the surface ramp (`base`, `surface`, `overlay`, `subtle`)
   and from inset cards with masked corners. Chrome, rows, chips, tabs, menus,
   and toasts carry no shadow; a floating overlay or dialog takes `shadow_lg()`
   (section 9).
3. One highlight material. Hovered, active, and selected states are alpha
   tints of one color per theme lightness, never a per-component fill.
4. Every rounded surface takes its radius from section 4.4, which is the
   closed set. New radii are not introduced.
5. Motion explains state: hover, focus dim, the sidebar slide, the menu
   reveal, the toast lifecycle. Nothing animates for decoration except the startup splash
   shimmer and the status spinners. Any new animation MUST read
   `reduce_motion`; section 4.8 lists which existing ones do.
6. Color carries meaning first: added, modified, deleted, conflict, error,
   stalled, and the eight broadcast groups keep their hues across presets.
7. Density over decoration. Body text is 12 px, labels are 11 px, micro chips
   are 9 to 10 px. Whitespace is spent on the grid, not on padding.
8. Every surface holds with the AppKit material on and off, in light and dark,
   and at the 800 by 500 minimum window.
9. Every control MUST carry an accessible name and an announced state.
   Section 7 is normative, not advisory.

## 3. Anatomy

### 3.1 The shell

```text
┌ title bar: max(1.75rem, 36px), full width, drag region, traffic lights ─────────────┐
│ ▤                                                                   [ ● ● ● macOS ] │
├──────────────┬───────────────────────────────────────────────────┬──────────────────┤
│ primary      │ main panel: inset card, 4px inset, 10px radius,   │ right rail       │
│ sidebar      │ corner masks painted in the shell color           │ sessions or      │
│ 300px        │ ┌ pane card, 20px squircle ─┐ ┌ pane card ─────┐  │ files, 300px     │
│ (520 in      │ │ header 34px: title, tools │ │                │  │ ── or ──         │
│  Review)     │ │ terminal, inset 3 / 0     │ │                │  │ diff dock        │
│              │ └───────────────────────────┘ └────────────────┘  │ min(880, room)   │
│ Workspaces   │              8px gutter, 80px min pane            │                  │
│ folder rows  │                                                   │                  │
│ tab rows     │                                                   │                  │
│ IPC banner   │                                                   │                  │
│ Agents|Review│                                                   │                  │
└──────────────┴───────────────────────────────────────────────────┴──────────────────┘
```

| Region | Role | Geometry | Source |
| --- | --- | --- | --- |
| Window | Client-side decorations by default; `window_decorations: "server"` opts out | Default 1200 by 800, minimum 800 by 500, corner radius 10, border 1, resize border 10, shadow black 0.4 blurred 5 when floating; a restored size is clamped to 3840 by 2160 when no display size is known | `window_state.rs:10-15`, `app/constants.rs:251-255`, `window_chrome/csd.rs:57,118-122`, `main.rs:3266-3274` |
| Title bar | Drag region, sidebar toggle, caption controls. Nothing else. | Height `max(1.75 rem, 36 px)`; control size 20; edge inset 8; control spacing 12; 80 px of brand padding for the traffic lights, dropping to the 8 px edge inset in fullscreen | `app/constants.rs:17-23`, `window_chrome/title_bar.rs:75,154-160,190` |
| Primary sidebar | Workspaces rail in Agents mode; the Workspaces and Changes rails side by side in Review; navigation in Settings | Width 300, **520 in Review** (220 + 300); slides in 280 ms | `app/constants.rs:15`, `app/review/mod.rs:22-23`, `app/review/mode.rs:90-92`, `main.rs:352`, `settings/chrome.rs:35` |
| Main panel | The inset card that holds the pane grid (Agents or Review) or a Settings page | Inset 4 on right and bottom, and on the left only when the sidebar is hidden; radius 10; four corner masks painted in the shell color. There is no top inset — a spacer the height of the title bar reserves the strip | `app/constants.rs:25-27`, `main.rs:872,1983,2425,2435-2438,2471-2506` |
| Pane grid | **N-ary** `LayoutTree { Leaf, Container }` of pane cards; one grid per workspace tab in Agents, one global grid of diff panes in Review | Gutter 8, divider hit area 7, minimum pane 80; `MAX_PANES` 32, `MAX_WORKSPACES` 20, `MAX_TABS_PER_WORKSPACE` 32, Review caps at `MAX_REVIEW_PANES` 6 | `layout/tree.rs:62-67`, `layout/mod.rs:34,39`, `workspace/mod.rs:53,59`, `app/review/mod.rs:21` |
| Right rail | Sessions rail, or the default Files tree rail. Mutually exclusive in rail placement; Sessions can coexist with a dock tree. | Width 300 each | `app/sessions_sidebar.rs:37`, `app/files_sidebar/mod.rs:52` |
| Diff dock | Side dock attached to a workspace tab, holding Changes plus file, terminal, and Agent setup tabs | Preferred width 880, minimum 360, maximum 1400, fitted to the live remainder and hidden below the floor (5.4); 8 file tabs, a cap that yields to unsaved work (5.4) | `app/diff_dock/model.rs:23-34`, `app/cli_diff_dock.rs:38-64` |
| Footer | IPC offline banner, MCP bridge callout, then the Agents / Review mode strip | Persistent primary navigation. **No Settings gear** (issue #105) and **no update banner** | `app/sidebar_actions_menu.rs:21-60,62-178,254-299` |

The window is 800 by 500 at minimum, and a surface MUST hold there with the
primary sidebar hidden and a right rail or the dock open at the same time.

### 3.2 Modes

PaneFlow has two modes and one takeover surface.

| Mode | Sidebar | Main panel | Entry |
| --- | --- | --- | --- |
| Agents | Workspaces: folder rows, tab rows with the branch and diffstat as meta lines beneath the title, and an agent icon stack | Pane grid | Footer switch, default |
| Review | Workspaces (220): one folder row per open repository, folding its checkouts and git worktrees as child rows; then Changes (300): a `Changes` header with the tree toggle, a base-branch row, an optional filter pill, and file rows | The same pane grid, each pane card holding one unified or split diff with sticky file headers | Footer switch or `secondary-shift-g`, both gated on `review_is_viable` |
| Settings | Back to the app, a search field, three nav groups | One page at a time, centered column, 26 px heading | macOS menu bar, **PaneFlow ▸ Settings…** |

Settings is not a window. It replaces the main panel and reuses the sidebar
width for its navigation, so the shell never changes shape.

`review_enabled` defaults to `true`, but it is only half the gate.
`review_is_viable()` is `review_view_enabled() && (a restored Review layout ||
a default subject)`, so with the switch on and no repository-backed workspace
open the footer still renders the strip while entry is a no-op. Treat the
visible switch as necessary, not sufficient. When the switch is off the strip
is not rendered at all — one reachable mode is not a choice — and
`secondary-shift-g` becomes a silent no-op through `review_is_viable`
(`app/review/mode.rs:55-58`).

### 3.3 Overlays

Every overlay is deferred at an explicit priority, and that ladder is itself
part of the contract: **1** settings selects · **2** toasts · **3** menus
(branch, new tab, dock options, Customize Sidebar, palette branch) · **4**
profile menu, Composer, dock options, the diff feedback flash
(`diff/view/interaction.rs`) · **6** full-surface overlays · **8**
Launch Pad, Custom Buttons and the Review-with-agent popover · **10** dialogs ·
**11** close confirm. A new overlay picks the rung that matches its kind rather
than inventing one.

One live exception: Work Review calls `deferred(...)` with **no** explicit
priority (`app/work_review/mod.rs:488-502`), so it is not on the ladder and
must not be read as sharing the full-surface rung. Give a new overlay an
explicit priority.

| Overlay | Placement | Shell | Source |
| --- | --- | --- | --- |
| Launch Pad | Horizontally centered, **top-anchored at 72**, over a full-window black 0.4 backdrop | Card 520 wide, radius 10; agent list, GitHub issue row, branch field, prompt field, footer hint, one tinted accent button | `app/launch_pad.rs:872-908` |
| Pane palette | Fills an empty tab, titled `New pane` | A centered 260 px column on a 20 px squircle of the terminal background: 13 px Semibold title, an optional branch row 28 tall, preset rows 34 tall with a 14 px agent mark, gap 2, list capped at 420 tall, inline error at 11 px | `app/pane_palette.rs:36-40,654-782,1021-1059` |
| Diff dock surface picker | Fills a fresh dock, under a 40 px header band carrying only the dock close button | **Four** cards 122 by 98, gap 12, radius 10, grid padding 16, icon gap 8; the grid wraps rather than fixing a column count | `app/diff_dock/surface_picker.rs:29-39,62-69,99-126` |
| Composer | Scrim over the whole pane, panel docked at its bottom | Black scrim at 0.25 on the 20 px squircle; panel on `overlay` with margin 8, padding 8, gap 6, 1 px border, radius 8, `shadow_lg`; header chips 10 px; input max height 180 | `pane.rs:690-732` |
| Pane Overview | Horizontally centered, top-anchored at 24 (`OVERVIEW_MARGIN`) | Radius 12, 1 px border, `shadow_lg` on a black 0.4 scrim; 312.5 by 192.5 cards, gap 10, radius 8, grid padding 16 | `app/pane_overview/mod.rs:36-41,486-559` |
| Attention Queue · Fleet Search | Horizontally centered, top-anchored at 96 | 560 wide, radius 8, black 0.4 scrim, `shadow_lg` | `app/attention_queue.rs:227-234`, `app/fleet_search.rs:379-386` |
| Broadcast groups | Horizontally centered, top-anchored at 96 | 420 wide, radius 8, black 0.4 scrim | `app/broadcast.rs:432-439` |
| Theme picker | Horizontally centered, top-anchored at 96 | 520 wide, black 0.4 scrim | `app/theme_picker.rs:343-375` |
| Custom Buttons | Horizontally centered, top-anchored at 72 | 560 wide, radius 10, black 0.45 scrim | `app/custom_buttons_modal.rs:489-543` |
| Close confirm | Centered | 360 wide, radius 10, padding 16, gap 10 | `app/close_confirm.rs:922-931` |
| Menus and selects | Deferred, anchored under the trigger | Squircle 18, list padding 4, item height 28 | `settings/components.rs:482,551,627` |
| Tooltip | After 800 ms | Squircle 14 on the title bar color with a 1 px `border` at full alpha | `ui_primitives.rs:494,524,539-549` |
| Toast | Bottom right: right 18, bottom **20** | Radius 8 on `subtle`, minimum width 220, one single-line row. The element is built at `bottom(18)`, but the animation callback owns the axis from the first frame: it enters 28 → 20, holds at 20, and exits 20 → 28, so 20 is the resting inset and 18 is never observed | `app/notifications.rs:106-122,136-141` |
| System Info dialog | Centered on a black 0.55 backdrop | Squircle 20, 560 wide, padding 20, label column 116, 1 px `border` at 0.6, `shadow_lg` | `app/system_info_dialog.rs:29-44,305-334` |
| About dialog | Centered on the same backdrop | 382 wide, 420 tall body, radius 10 (round, not squircle), 1 px border, `shadow_lg`, 32 px header band; colors derived from `UiColors`; **Migration**, see section 11 | `app/about_dialog.rs:143,354,439-448` |
| Peek badge | Anchored under the pane header, right 2 | Max width 420, `px 2 / py 1`, `text_xs` on `overlay` with a 1 px `vc_conflict` at 0.6 border; the collapsed line caps at 80 characters and hover expands it | `pane.rs:112-119,740-780` |

## 4. Foundations

### 4.1 Color architecture

Color resolves in three layers.

1. **Terminal theme**: 36 `Hsla` slots per variant — 24 ANSI colors, 5 base
   colors (`background`, `foreground`, `bright_foreground`, `dim_foreground`,
   `ansi_background`), `cursor`, `selection`, the derived `selection_foreground`,
   `scrollbar_thumb`, `link_text`, and two title bar colors — plus a 30-slot
   `SyntaxPalette` for the diff and the editor
   (`theme/model.rs:11-64,75-104`).
2. **UI colors**: the 30 semantic roles plus one flag (`use_theme_diff_washes`)
   that the chrome consumes, `UiColors` (`theme/model.rs:522-596`). Vercel,
   Claude, and Cursor each ship their own; PaneFlow Dark and PaneFlow Light
   carry `ui: None` and are derived by lightness.
3. **Local tints**: alpha washes computed at render time from `text`, `muted`,
   or a fixed tint, listed in 4.3.

Components MUST consume `UiColors` through `crate::theme::ui_colors()` (or the
lock-free `ui_colors_with(&theme)`). A hex literal in render code is allowed
only for the fixed and Contextual values listed in 4.3.

**The palette lives in two files, not one.** `theme/builtin.rs` holds the ANSI
and base slots for all eight variants and the `UiColors` of Vercel, Claude, and
Cursor. The PaneFlow Dark and PaneFlow Light `UiColors` are computed in
`theme/model.rs::ui_colors_with` (`:677-778`), and the dark surface constants
`CHROME_BACKGROUND_HEX`, `TERMINAL_BACKGROUND_HEX`, and `BORDER_HEX` are at
`theme/model.rs:458-461`. Cite both files when changing a role.

`UiColors::diff_colors()` (`theme/model.rs:620-640`) is the single source the
diff dock, the Review view, and the Changes rail read; `UiColors::group_color(i)`
(`:643-658`) wraps modulo 8 so no render site indexes the broadcast slots by
hand.

### 4.2 Semantic roles

| Role | Use | PaneFlow Dark | PaneFlow Light |
| --- | --- | --- | --- |
| `base` | Panel and settings background, the work surface | `#181818` | `#ffffff` |
| `surface` | Cards inside a panel, menu surfaces in dark | `#212121` | `#f7f7f7` |
| `overlay` | Shell chrome in dark, popups in light | `#141414` | `#ffffff` |
| `border` | Hairlines, card outlines, pane card border | `#252525` | `#e6e6e6` |
| `subtle` | Pills, inputs, toasts, resting control fill | `#2a2a2a` | `#eeeeee` |
| `muted` | Secondary text, icons at rest, eyebrows | `#a0a0a0` | `#6a6a6a` |
| `text` | Primary text, icons on hover | `#dddddd` | `#262626` |
| `accent` | Links, selected metadata, the one primary action, info callouts | `#57d5c4` | `#4c6fff` |
| `tool_card_header_bg` | Reserved; no surface consumes it today | `#2e2e2e` | `#f1f1f1` |
| `vc_added`, `vc_modified`, `vc_deleted`, `vc_conflict` | Diffstat, status letters, change bars, attention border | `#57d992`, `#ffd166`, `#ff6f6a`, `#ffa657` | `#40a02b`, `#df8e1d`, `#d20f39`, `#fe640b` |
| `vc_*_background` | Row washes in the diff | the matching hue at 0.12 | at 0.16 |
| `vc_word_added`, `vc_word_deleted` | Reserved; word diff was removed (5.4) | the matching hue at 0.40 | at 0.40 |
| `group_1` to `group_8` | Broadcast group stripe and picker | `#7eb6ff`, `#57d992`, `#ffd166`, `#ff6f6a`, `#c79bff`, `#57d5c4`, `#ffa657`, `#9ea7ff` | Catppuccin Latte hues |
| `agent_claude`, `agent_codex` | Identity dots and status glyphs | `#ffa657`, `#7eb6ff` | `#e89271`, `#5b6cff` |
| `agent_error`, `agent_stalled` | Failed and stalled agent states | `#ff6f6a`, `#a0a0a0` | `#d20f39`, `#808080` |

The dark work surface is `#181818` and the dark chrome is `#141414`: the panel
is lighter than the shell around it, which is what makes the inset card read
as a card without a shadow. Light inverts the ramp: pure white work surface,
`#f7f7f7` cards, and a `#f3f4f9` title bar (`theme/builtin.rs:123`).

`apply_surface_overrides` (`theme/model.rs:491-511`) normalizes a dark preset
that ships no `UiColors` — in practice only PaneFlow Dark, since the other six
carry `ui: Some(..)`. It rewrites **nine** terminal slots: both title bar
colors to `#141414`, `background` and `ansi_background` to `#181818`,
`foreground` `#f0f3f7`, `bright_foreground` `#ffffff`, `dim_foreground`
`#9ca7b5`, `selection` `#5aa6ff` at 0.22, `scrollbar_thumb` `#9aa8bd` at 0.30,
and `link_text` `#57d5c4`. It **never sets `border`** — `#252525` comes from
the derived dark `UiColors` arm. The light branch returns early but still
recomputes the selection foreground.

Diff colors on a dark theme fall back to PaneFlow's canonical green and red
with opaque row washes unless the preset sets `use_theme_diff_washes`; **both
Vercel variants do** (`builtin.rs:251,335`), and no other preset does. The
opaque dark fallbacks are `#57d992` / `#ff6f6a` on `#1d3a2b` / `#402425`, with
gutters `#16281f` / `#2c1718` (`theme/model.rs:631-639`).

Status hues are functional and MUST NOT be recolored to match a brand when
doing so weakens the meaning. The terminal selection foreground is never
hand-tuned: it is recomputed at theme load until it clears APCA Lc 45 (7.3).

### 4.3 Tints and fixed values

| Tint | Dark | Light | Where |
| --- | --- | --- | --- |
| Sidebar row active | white at 0.11 | `#262626` at 0.08 | `app/constants.rs:44-48` |
| Sidebar row hover | white at 0.07 | `#262626` at 0.04 | `app/constants.rs:47-49` |
| Tab icon card | title bar color blended with the active tint, then darkened 0.10 | darkened 0.05 | `app/constants.rs:50-55,187-213` |
| Menu item selected | `text` at 0.10 | same | `settings/components.rs:600` |
| Menu item hover | `text` at 0.05 | same | `settings/components.rs:610` |
| Menu divider | `text` at 0.12, deliberately not `border` | same | `settings/components.rs:472-474` |
| Menu surface | `surface` lifted by 0.035 lightness | `overlay` | `settings/components.rs:452-462` |
| Menu border | `border` at 0.6 | same | `settings/components.rs:514-518` |
| Control hover | `subtle` moved 6 percent toward `text` | same | `settings/components.rs:289,405` |
| Select chevron | `muted` at 0.7 | same | `settings/components.rs:387` |
| Hairline | `border` at 0.5 | same | `settings/components.rs:130-132` |
| Unfocused pane dim | terminal background at 0.3 | same | `pane.rs:2116-2122`; `unfocused_pane_opacity` default 0.7, clamped 0.15–1.0 |
| Attention border | `vc_conflict` at 0.7 | same | `pane.rs:2360,2400` |
| Pane swap placeholder | `text` at 0.10 fill, 0.22 border | same | `pane.rs:189-190,2219-2221` |
| Sidebar drop placeholder | `text` at 0.10 fill, 0.22 border | same | `app/sidebar/mod.rs:218-219,2469-2471` |
| Sidebar reorder line | `text` at 0.5, 2 px, `rounded_full` | same | `app/sidebar/mod.rs:241,1332` |
| Changes rail left border | `text` at 0.06 | same | `app/diff_sidebar/mod.rs:44-45` |
| Unfocused code editor wash | one third | same | `app/diff_dock/code/element.rs:311,323` |
| Icon button hover | the caller's hover color from 0 to 1 | same | `ui_primitives.rs:606` |

Fixed values that deliberately do not follow the theme. They read as OS
controls or as system semantics rather than as brand, and are **Contextual**
to the surfaces named:

| Value | Where | Why |
| --- | --- | --- |
| `#339cff` | Toggle track when on (off is `muted` at 0.30) | Platform toggle blue |
| `#ff453a` | Destructive button, hover `l − 0.05` | System red, white label |
| `#007aff` | **The pane split-drop overlay and the PaneFlow terminal cursor only** | System blue for the split affordance |
| `#fbbf24` | Sidebar bell when an agent needs input | Amber request signal, identical in every preset |
| `#83c3ff` | Sidebar dot when an agent finished | Light blue completion signal, identical in every preset |
| `hsl(40 85% 55%)`, `hsl(0 62% 56%)` | Callout warning and error | Severity hues independent of preset |
| `#232323` / `#ffffff` | Settings card fill, keyed on `background.l > 0.5` | Card sits one step above `base` in either lightness |
| `0x2c2c2c` / `0x8b8b8b` / `0xb9b9b9` | Surface picker ink on dark themes | **Contextual**, `app/diff_dock/surface_picker.rs:49-59` |
| `0x2d8c4a` / `0x5cff8a` / `0x021608` (and its inset shadow pair) | About dialog CRT credit plate | **Contextual** period piece, `app/about_dialog.rs:187-258` |
| `0x323232` | Custom Buttons icon picker, selected tile | **Migration**: predates the `UiColors` roles and should move onto one, `app/custom_buttons_modal.rs:895` |
| `0x89b4facc` on `0x1e1e2e` | Terminal copy-mode `COPY` badge | **Migration**: a leftover Catppuccin pair, `terminal/view.rs:1902-1903` |
| `0x383838` | Dark terminal panel ground (`codex_panel_background_for_terminal`) | **Migration**: the light arm already uses `subtle`, `terminal/element/mod.rs:230-236` |
| `0x2fd7f2` | Settings ▸ Terminal, the "uses theme" scheme chip | **Migration**: should be `accent`, `settings/tabs/terminal.rs:593-598` |
| `0xE0_6C_75` | Settings danger text: the injection-fence warning and the MCP failure recap | **Migration**: a fixed One Dark red standing in for a danger role `UiColors` does not have, so it does not follow the theme. `settings/tabs/general.rs:324`, `settings/tabs/mcp.rs:356-360`. Note the literal is written with underscores, so a `0xE06C75` search misses it |

The sidebar's drop affordances are **not** blue. Only the pane split preview
is; the swap preview, the sidebar placeholder, and the reorder line are all
neutral `text` tints.

### 4.4 Geometry

| Element | Radius | Corner | Border |
| --- | --- | --- | --- |
| Window | 10 | round | 1 px `border` on free edges |
| Main panel | 10 | round, masked | none |
| Pane card | 20 | squircle | 1 px `border`, or `vc_conflict` at 0.7 with attention |
| Settings card, pane palette ground, diff dock card | 20 | squircle | none |
| System Info dialog | 20 | squircle | 1 px `border` at 0.6, plus `shadow_lg` |
| Menu, select popup | 18 | squircle | 1 px `border` at 0.6 |
| Sidebar rows, tab icon cards, footer mode buttons, row skin, secondary button, select item, dock tab chip, file tree row, tooltip | 14 (`ROW_RADIUS`) | squircle | tab icon card and tooltip, 1 px `border` |
| Pane Overview panel | 12 | round | 1 px `border`, plus `shadow_lg` |
| Theme tile | 10 | round | 2 px `text` at 0.12, 0.32 on hover, 0.85 when selected |
| About dialog | 10 | round | 1 px, plus `shadow_lg`; **Migration** |
| Filter field, settings control, select trigger, toast, composer, drop overlay, drop placeholder, theme mockup inner frame | 8 | round | drop overlay 2 px blue |
| About close button | 7 | round | none |
| Toolbar pill, sidebar IPC banner, sidebar hover action button, sidebar branch chip, launch pad field, title bar menu trigger, dock tab close slot | 6 | round | IPC banner 1 px `border` |
| Title bar sidebar toggle, launch pad primary button | 5 | round | none |
| Icon button, composer chip, sidebar context menu row | 4 | round | none |
| Scrollbar thumb, header chip, filter clear | 3 | round | none |

Squircle means `squircle_fill` and `squircle_border` from
`ui_primitives/squircle.rs`, applied through `squircle_skin`, `setting_card`,
`menu_surface`, or `tooltip_shell`. Plain `rounded()` is for controls at 10 px
or below, where the superellipse is invisible. The Settings select trigger and
the callout row are the two deliberate 8 px round exceptions inside otherwise
squircle families.

There is **no** `menu_item` primitive. `select_item` rows are `ROW_RADIUS`;
the app's own context menus (`app/sidebar/context_menu.rs`) are plain 4 px and
7 px rounds, and that is **Migration**, not a precedent.

### 4.5 Spacing and sizes

| Measure | Value |
| --- | --- |
| Panel inset | 4 |
| Pane gutter | 8; divider hit band 7; minimum pane 80 |
| Pane content inset | **3 horizontal, 0 vertical** (upstream paints 10 / 6; this fork keeps its 3 px gutter) |
| Pane header | 28 content plus the vertical inset twice, **34 total**; gap 7 |
| Sidebar header row | 36 tall, px 8, label 13 px `muted` with pl 8; two 20 px square buttons at radius 6 with 12 px glyphs, gap 2 |
| Sidebar row | margin 8, padding 8 by 6, gap 4, line height 18, spacing 4; guide-enabled tabs without an agent badge inset their shell 22 |
| Sidebar tab icon stack | 16 px icons, cap 4, overlap 11, 24 by 24 icon card |
| Sidebar action button | 20, gap 4; agent status slot 48 when an agent needs input, 28 when more than one agent, otherwise 20 |
| Sidebar footer | padding 6 top and 8 bottom; mode buttons 30 tall on squircle 14, gap 3, margin 8; IPC banner mx 6 / mb 2 / px 8 / py 6 with no fixed height |
| Review rail row | margin-x 8, padding-x 8, height 30, gap 4, child indent 18, icon 14, subject dot 6 |
| Sessions row | height 30; 5 rows per agent group before **Show more** |
| Files tree | rail 300, optional dock panel 250; rows 28, indent 18, leading slot 14, row gap 12; selection is a `ROW_RADIUS` squircle |
| Settings row | padding 12 by 10, gap 16; section header bottom padding 8 |
| Select trigger | padding 10 by 6, width 190 to 260 |
| Menu | list padding 4, item gap 1, item height 28, width 200 to 280, max height 320 |
| Toggle | track 36 by 22, knob 18 |
| Icon buttons | small 20 outer with 12 icon, medium 24 outer with 13 icon |
| Toolbar pill | height 24, padding 8, gap 5 |
| Filter field | padding 10 by 6, gap 6, 13 px search icon; the clear control is a **24 by 24 hit target** carrying a 10 px glyph, pulled in by −4 so it keeps a 16 px layout footprint (WCAG 2.5.8) |
| Toast | right 18, bottom **20** (the animation owns the vertical axis - see 3.3), padding 12 / 14 by 11, minimum width 220, max width 340 (440 for an error), single line |
| Scrollbar | width 6, gutter 10, minimum thumb 24 |
| Diff | row 18, file header 32, fold row 32, sticky header 24, gutter 36 (a floor, widened per digit count), change bar 4, split divider 3, minimum split column 360, revert chip 56 by 16 inset 10, horizontal track 6 |
| Code editor | 12 px mono, row 18, caret 2, scrollbar track 15, minimum thumb 25 vertical and 28 horizontal; git marker column 6 left of the numbers, bar 4 radius 2 inset 1, deleted dot 8, hover grows 3 to the left |
| Dock | preferred 880, minimum 360, maximum 1400; maximized: panel width minus two 8 px gutters, floor 360; tab strip 40 with 26 px chips, gap 4 |
| Pane Overview | cards 312.5 by 192.5, gap 10, radius 8, grid padding 16, panel margin 24 |

### 4.6 Typography

| Role | Family | Size and weight | Where |
| --- | --- | --- | --- |
| Interface | Geist, bundled, set on the root element (`main.rs:2153`) | 12 Normal for body, Medium for titles in rows | Everything that is not a terminal or code |
| Labels | Geist | 11 Normal muted for eyebrows and descriptions, Semibold for `section_eyebrow` | Settings, rails, pills |
| Micro | Geist | 9 to 10 | Header chips, composer chips, hints |
| Emphasis | Geist | 13 Medium | Row titles that need to outrank body |
| Title | Geist | 14 Semibold | Pane header, empty-state titles, callout titles, System Info title |
| Page heading | Geist | 26 Semibold | Settings page title |
| Dialog title | Geist | 16 | About only — System Info uses `TITLE` (14) |
| Splash | Geist | 34 Medium | Startup wordmark |
| Terminal | User choice among fixed-pitch families; default the bundled JetBrainsMono Nerd Font | 13 pt default (range 8–32); `line_height` and `cell_width` are multipliers of the measured cell, both defaulting to 1.0 (ranges 0.8–2.5 and 0.8–2.0). At 13 pt the cell measures 10 by 23 px | Panes |
| Code and diff | `resolve_font_family(None)`, the terminal default | 12 | Diff dock, editor, theme preview |
| Minimap | `.ZedMono`, which resolves to the bundled Nerd Font | 2 px Black, 1.618 line height | Dock code editor |

The named constants live in `ui_primitives.rs:425-433`: `LABEL_XS` 10,
`LABEL_SM` 11, `BODY` 12, `BODY_EMPHASIS` 13, `TITLE` 14. New interface text
MUST use them. They are not yet universal — a couple of hundred
`text_size(px(N.))` literals remain — so treat an existing literal as debt, not
as licence.

Bundled families (`src-app/assets/fonts/`): Geist, Geist Mono, IBM Plex Mono,
IBM Plex Sans, JetBrainsMono Nerd Font, Lilex, VT323. `Assets::load_fonts`
registers every one with GPUI at boot, but only the first six are selectable:
**VT323 is Contextual to the About dialog's CRT credit plate** and is absent
from `resolve_font_family`'s embedded list, so configuring it as `font_family`
is rejected unless the user has it installed system-wide.

The Nerd Font ships in its non-Mono variant so icon glyphs keep their designed
size; the renderer constrains them to their cells. Aliases that resolve
silently: `JetBrainsMono NF`, `JetBrainsMono Nerd Font Mono`, `JetBrainsMono
NFM`, `.PaneflowMono`, `.PaneflowSans`, `.ZedMono`, and Zed's `.ZedSans` to
Geist.

Sentence case everywhere. Titles truncate with a tooltip past 13 characters
and cap at 24 in the pane header; dock tab labels cap at 22 and file headers
at 64.

### 4.7 Iconography

Chrome icons are single-color stroke SVGs in `src-app/assets/icons/` (75 SVGs
plus three PNGs and a `languages/` subdirectory of 19), painted with
`text_color` so they follow `muted` at rest and `text` on hover. **Language
icons are the exception**: `icons/languages/` assets carry their own `fill`
and are drawn with `img()`, never `svg()`, so the `text_color` rule covers
chrome only.

| Size | Use |
| --- | --- |
| 10 | Filter clear glyph, sidebar diff-header glyphs |
| 11 | Sidebar agent state glyphs (bell, error, stalled) and the comet-trail loader |
| 12 | Small icon button, select chevron, drag ghost, Editor Controls trigger |
| 13 | Medium icon button, filter search, menu check mark, dock tab icon, diff file-header file-type icon |
| 14 | Title bar sidebar toggle, editor and preset logos, sidebar folder, sidebar footer banner |
| 15 | Toast icon |
| 16 | Sidebar tab icon, callout icon, dock options trigger |
| 18 | Empty-state glyph |

The pane card's close chip is the one glyph below the table: `CLOSE_GLYPH_SIZE`
is 9 (`pane.rs:150`), sized to sit inside a 15 px chip (5.3). Apart from it no
glyph outside 10–16 and 18 exists, and every larger `size(px(N.))` is an icon
button *box*, not a glyph.

The fork ships 16 agent launchers (`agent_launcher.rs:23-40`). Marks live in
`src-app/assets/agents/` for the eleven secondary agents and in `icons/` for
Claude, Codex, OpenCode, Pi, and Hermes (`icons/hermesagent.svg`).
`TerminalAgent::icon_multicolor` (`agent_launcher.rs:155-163`) is the authority
on rendering: exactly five — Antigravity, CodeBuddy, Gemini, Kiro, Openclaw —
render through `img()`. `TerminalAgent::accent()` is a separate and narrower
authority on tint: it returns a brand color for **only three** agents — Claude
`#d97757`, Amp `#F34E3F`, Qoder `#2ADB5C` — and `None` for every other, whose
monochrome mark deliberately takes the theme's text color. Do not invent a
brand tint for a mark that returns `None`.

### 4.8 Motion

| Motion | Duration | Easing | Notes |
| --- | --- | --- | --- |
| Hover on an `animated_hover` control | 120 ms, scaled by the distance left to travel | ease-out quint | Retargets mid-flight, pauses during a drag. **Rows are the deliberate exception**: `squircle_skin` (and `select_item` through it) toggles visibility on `group_hover` instead, so a long list does not request an animation frame per row. Do not "fix" a snapping row into an animated one |
| Pane header buttons | 120 ms | ease-out quint | The action-button tint and the close glyph's 0.16 → 0.92 ramp |
| Unfocused pane dim, drop overlay glide | 130 ms, scaled by distance | ease-out quint | Cross-fade dropped below 0.002; the overlay lerps its absolute rect between regions |
| Primary sidebar slide | 280 ms | cubic ease-out `1 − (1 − p)³` | Panel inset and gutter follow the width |
| Menu reveal | 140 ms | cubic ease-out `1 − (1 − p)³` (`ui_primitives::ease_out_cubic`, shared with the sidebar slide) | `menu_reveal`: every menu, select popup, context menu, and submenu fades in from 0 while dropping 4 px into place. No exit animation: GPUI drops the element when its state flips |
| Diff dock open and maximize slides | 280 ms | cubic ease-out `1 − (1 − p)³` | `SidebarWidthAnimation` reused: the dock column grows from the right edge on open; on maximize the pane grid is clipped from its measured width to 0 (never resized) while the dock's left gutter grows with it. A session-switch restore skips the open slide |
| Toast | 180 ms in, **1440 ms default** hold, 180 ms out | ease-in-out | 8 px lift on entry, 8 px drop on exit. `hold_ms` is carried per `Toast`: the Composer recap and queued-prompt toasts hold 4000 ms, and a session-save failure holds `TOAST_HOLD_MS * 2` (2880 ms). Longer holds are deliberate, not drift |
| Status spinner | 1 s loop | linear rotate | Empty states while scanning |
| Sidebar comet-trail loader | 720 ms cycle | stepped | 3 by 3 perimeter of 3 px dots, gap 1, trailing opacities 0.81, 0.49, 0.26 over a 0.06 base |
| Startup splash | 2600 ms shimmer, 900 ms minimum on screen | linear | Letters at 0.54 alpha, shimmering to 0.82 |
| Tooltip | 800 ms delay | none | `delayed_tooltip` |

`reduce_motion` is a PaneFlow-owned process-wide `AtomicBool`, not a GPUI
facility: the pinned GPUI predates `App::set_reduce_motion`. It is written at
startup, from the Settings toggle, and on config hot-reload, so it needs no
restart, and it defaults to `false`.

**Five animations honor it today**: `animated_hover` settles instantly
(`ui_primitives.rs:322-336`), the primary sidebar toggles without the slide
(`main.rs:1726`), the diff dock opens and maximizes without its slides
(`app/diff_dock/mod.rs::open_diff_dock_panel`,
`app/cli_diff_dock.rs::toggle_diff_dock_maximize`), `panel_empty_state`'s
scanning spinner does not start (`ui_primitives.rs:859`), and `menu_reveal`
mounts every menu at rest (`ui_primitives.rs::menu_reveal`). Still ignoring it: the pane header button hover, the
drop-overlay glide, toasts, the comet-trail loader, and the splash shimmer.
The config description promises a static frame for decorative animations; that
promise is **Proposed** until the rest read the flag. Feedback is never
removed, only its interpolation.

## 5. Component Contracts

### 5.1 Title bar

Full width, drag region, double-click zooms, right-click shows the native
window menu. **The left rail carries exactly one control**: the sidebar toggle
(20 px, radius 5, resting tint when the sidebar is hidden, a 14 px
`icons/sidebar.svg` in `muted`, a `Role::Button` with an accessible name and a
delayed `Show sidebar` / `Hide sidebar` tooltip).

There are no Files or Help menus in the title bar. A regression test forbids
them end to end across six files
(`title_bar_files_and_help_popovers_are_removed_end_to_end`,
`window_chrome/title_bar.rs:421-477`). Help lives on the native macOS menu bar,
whose four menus are PaneFlow, Edit, Window, and Help.

The workspace-name breadcrumb (a 3 px `muted` dot plus the name at 12 px
Medium) and the IPC pill still exist in `title_bar.rs`, both gated on
`!self.cockpit`, and `main.rs:2142` sets `cockpit = true` unconditionally — so
neither ever renders. That code is **Migration**; the sidebar footer owns the
IPC banner, and there is no update pill anywhere because Sparkle 2 owns
updates.

The traffic lights get 80 px of brand padding, dropping to the 8 px edge inset
in fullscreen. The bar is an absolutely positioned overlay at top 0 above the
rail and the panel, and it draws no bottom hairline inside the cockpit shell —
the panel inset separates it from the content.

### 5.2 Primary sidebar, Agents mode

A 36 px header row reading `Workspaces` at 13 px `muted`, with **two** 20 px
icon buttons on the right: the Customize Sidebar menu behind
`icons/filter-2.svg`, and Pane Overview behind `icons/layout-grid.svg`. There
is deliberately **no new-workspace button** (issue #105); a guard test
(`the_workspaces_header_carries_no_new_workspace_button`) fails if one returns.
New Workspace is `secondary-shift-n`, the Window menu, and the empty state's
`Open folder` button.

A workspace is a folder row; its tabs are child rows with inline rename, hover
actions, and reorder by drag. The branch and the diffstat are **meta lines
beneath** the tab title, not inline in it; a split tab prints one labeled line
per terminal. `branch` defaults on; `diffstat`, `pr`, and `indent_guide`
default off, all four toggled from the Customize Sidebar menu's `Show`
submenu, whose parent also carries `Expand all` and `Collapse all`.

With the indent guide enabled, a tab without an agent badge insets its shell
22 px (the 14 px folder slot plus the 8 px title gap) and omits that blank
slot. Title and branch content widths both shrink by 22 px, keeping their
right edges inside the shell; branch padding subtracts the inset so its icon
still aligns with the title. Agent badge rows deliberately keep their full
shell and badge slot, with the guide interrupted around the badge. This gives
two shell left edges but one title and branch alignment. Turning the guide
off keeps the blank slot and the original full-width shell (#492).

When a session changes its terminal title, a tab containing exactly one pane
follows that pane's resolved name when its stored label was launch-generated.
The label then stays derived so later session renames appear immediately.
Names typed by the user take precedence and survive terminal title changes.
Older saved labels without provenance are treated as manual. Split tabs keep
their stored name, including when one of their panes is zoomed.

Agent status occupies a 48 px slot when an agent needs input, 28 px when a row
carries more than one agent, and 20 px otherwise, with 11 px glyphs: an amber
bell when the agent needs input, a light blue 7 px dot when it finished, a
**comet-trail loader** — a 3 by 3 perimeter of 3 px dots — while it thinks, an
`agent_error` circle-x when it failed, and an `agent_stalled` triangle when it
stalled. The bell and the dot use the fixed colors from 4.3. The agent icon
stack caps at four 16 px marks with an 11 px overlap.

Drop placeholder while dragging: margin 6, radius 8, `text` at 0.10 with a
0.22 border and a 2 px line. The row-reorder insertion line is a separate 2 px
`text` at 0.5 rule.

**The footer stacks, top to bottom**: the IPC offline banner when the socket is
disabled (mx 6, mb 2, px 8, py 6, radius 6, 1 px `border` on `subtle`, a 14 px
alert glyph and `IPC offline` at 12 px Medium); the MCP bridge callout (issue
#443) with its accent `Install MCP bridge` button and a 10 px dismiss; then the
mode row, `Agents` and `Review` as two flexible 30 px squircle-14 buttons at
small text Medium with a 3 px gap. The active segment takes the active row
tint and carries no click handler; the other takes the hover tint.

There is **no Settings gear** and **no update banner**. The whole mode strip is
dropped when `review_enabled` is off, and the footer collapses to an empty div
when banner, callout, and strip are all absent. The Review Workspaces rail
renders the same footer, so the mode switch and the IPC banner exist in both
modes.

### 5.3 Pane card

A 20 px squircle filled with the terminal background, 1 px `border`.

**A pane holds exactly one surface.** The surface-chip tab bar upstream
documents does not exist here; closing a pane removes it from the layout tree,
and there is no intermediate empty-pane state. The only tab strip in the app
is the diff dock's (5.4).

The header is **34 px** (28 content plus the 3 px inset twice), gap 7, padding
3. The surface title sits at 14 px on an 18 px line, centered by three flex
zones, ellipsized, with a tooltip past 13 characters and a hard cap at 24. A
6 px status dot leads (`agent_error` wins over `vc_conflict`). **At most two
adornments** paint, ranked dot, then queued, then progress, then match: 9 px
chips on `subtle` at padding 4 / radius 3 carrying `1 queued`, the OSC 9;4
progress, or an accent `{n} hits` fleet-match badge. There is no worktree chip
— the sidebar row owns the worktree — and there is no identity pill, because
the sidebar owns identity.

On the right, 22 px action buttons at radius 4 stay visible at rest, `muted`
into `text` on hover with 14 px glyphs. A terminal pane shows split-vertical,
split-horizontal, the Agent sessions rail (hidden when no AI agent is enabled),
and the diff dock toggle. A diff pane shows refresh, view mode, collapse, and
Review with agent instead. A zoomed pane gains an 18 px `Z` chip on `accent`.

**The close control is a 15 px round chip in the *leading* corner**, opposite
the constructive cluster, with a 9 px glyph and `shadow_lg`. It is invisible at
rest and revealed by header hover, its glyph ramping 0.16 → 0.92 over 120 ms
with ease-out quint, resuming from an interrupted hover. It is a two-press
confirm: once armed it fills at 0.72 over `vc_deleted`, its tooltip becomes
`Click again to close`, and it stays painted with the hover gate dropped. A
double-click is swallowed so the second click cannot confirm the arm.

State layers, painted in this order: card fill, content (header, review menu,
body, **dim layer**, **drop overlay**), **peek overlay**, broadcast stripe
(3 px of the group color, inset by the radius top and bottom), border,
composer. Unfocused panes in a multi-pane workspace fade under a 0.3 overlay by
default; the dim is a plain compositing quad with no id and no handlers, so it
takes no hit test, it covers the header too, it cross-fades over 130 ms, and it
is suppressed entirely while a Composer is attached.

The split-drop overlay is blue at 0.10 with a 2 px blue border, radius 8,
margin 8, and it glides between regions over 130 ms rather than jumping; the
edge band is 0.20 of the shorter dimension. The swap variant is neutral `text`.
Attention reuses the border slot — 1 px `vc_conflict` at 0.7, no width change,
so the glow paints without reflow. There is no blue focus ring anywhere.

### 5.4 Diff dock and Review view

**The dock** attaches to a workspace tab and opens on a surface picker of
**four** cards — Changes, Terminal, File, Agent setup — 122 by 98 on a wrapping
grid. The choice is parked on the **tab**, not the workspace: dock slots are
keyed by `Tab::id` (`app/cli_diff_dock.rs:18`), so a sibling tab of the same
folder is a new session and shows the picker again.

Its width is a **preference, not a measurement**. `DiffDockState::width`
defaults to 880 and is bounded 360 to 1400, but the rendered width is
`min(stored, available − 104)`, where 104 reserves one minimum pane plus two
gutters for the grid. When that ceiling drops below 360 the dock is **not
rendered at all**: the grid wins, the dock's state (open, tabs, snapshot)
survives, and any in-flight resize or scrollbar drag is cancelled — so a
closing rail or a growing window brings it straight back. The render clamp
never writes back; the resize drag is the only writer, and a drag pinned at
the ceiling leaves a wider stored preference alone
(`app/cli_diff_dock.rs:38-64`, `app/diff_dock/mod.rs:68-86`).

**Maximized**, the dock takes the whole cockpit: `secondary-shift-f` or the
maximize button at the right of the tab strip (`diff-dock-maximize`, labelled
"Maximize dock" / "Restore dock", a `minimize` glyph while maximized) hides
the pane grid so Changes, an editor tab, or a dock terminal gets the full
window width. The grid is **clipped, never resized**: it stays mounted at its
last measured width inside an `overflow_hidden` column that slides to 0, so an
agent running behind the dock never sees a PTY resize. The dock bypasses the
fit (it renders even in a panel too narrow for dock plus grid), flexes to the
container with no resize handle, and paints the grid's left gutter itself as
the grid goes. Maximizing records the focus that was active and moves it onto
the active dock tab's own handle (a File or Terminal tab), or blurs the pane
when the tab has none (Changes), so keystrokes never reach the hidden grid;
restoring, or closing the maximized dock from its strip or a pane header's
dock toggle, hands it back. What comes back is checked against the model, not
against the frame: a maximized dock renders no grid, so maximize records the
pane that owns the focus while the grid is still painted - its surface, an
input the frame rendered inside it (a find bar), or one of the two editors
mounted beside the surface on the pane card (the header rename field, the
Composer prompt). On the way back that input returns when its pane is still a
pane of the visible tab, the pane itself when its surface was swapped
meanwhile, and a handle with no owning pane returns only while the last frame
rendered it under the app root or it belongs to an open pane palette. Anything
else - the handle of a pane closed while the dock was maximized, as with
nothing recorded at all - focuses the workspace's first pane rather than
leaving the keyboard parked on the window with no pane focused. One limit
stands: an input dismissed by another route while the dock was maximized still
gets its handle back and so lands on the window, because a pane does not
publish whether its find bar is still open. A restore that slides hands the
focus back only once the slide settles, so keys typed while the grid is still
clipped away stay off it. Panes behind a maximized dock are not under the
user's eye: their agents' completions and notifications go out as for a
zoomed-away split. The state is per-app: a tab switch parks the dock through
the closer, and a trip through Review or Settings drops it as the dock
unmounts, so the incoming tab, and the user coming back, always see the grid. Both the open slide and the
maximize slide reuse the primary sidebar's 280 ms curve (4.8) and settle
instantly under `reduce_motion`.

The tab strip is 40 px with a bottom hairline, gap 4, px 8. Chips are 26 tall
on a `ROW_RADIUS` squircle, gap 6, px 8, with a 13 px kind icon, the title at
body size Medium, and a 16 px close slot at radius 6 carrying an 11 px glyph.
A modified tab swaps that glyph for a 7 px `vc_modified` dot at rest, and
arming the close paints it `vc_deleted` — the same two-press confirm the pane
uses. **No tab is permanent**: the dock starts with no content tabs, Changes
is created only when its picker card or `+` menu row is chosen (and reused
when it already exists), and every tab carries the close control, Changes and
the first tab included. Closing the last tab returns the dock to the picker,
re-armed. A Changes tab explicitly opened against a non-git or clean-diff
workspace keeps the blank Changes body (#393). `MAX_DIFF_FILE_TABS` is 8 and
counts *file* tabs only; past the cap the leftmost file tab that is neither
modified nor active is evicted. **The cap yields to unsaved work**: when every
file tab is modified or active, `file_tab_eviction` returns `None` and the new
tab is still inserted, so the strip may exceed eight rather than drop an edit
(`app/diff_dock/tabs.rs:329-346`). The `+` trigger is a 28 px `ROW_RADIUS` square opening a
236 px menu of Changes, File (`secondary-g`), Terminal (`secondary-j`), and
Agent setup.

**The Files tree defaults to a separate 300 px right rail** (`files_tree_placement: "rail"`). It is
per workspace tab (`Tab::files_sidebar_open`), mutually exclusive with the
Sessions rail, unmounted in Review and Settings while staying warm, and
toggled by `secondary-alt-f`. Its width is fixed and resizing it is an explicit
non-goal. Rows are 28 tall with 18 px indentation, a 14 px leading slot, a
12 px row gap, and a `ROW_RADIUS` squircle selection; the header is a 36 px
title row and the search field is the shared `filter_pill`.

With `files_tree_placement: "dock"`, the same panel renders at 250 px to the
right of the editor, below a shared 40 px project/file breadcrumb toolbar.
The named folder button and `secondary-alt-f` toggle it. It has the same rows,
filter and context menus, with no duplicate title, close button or material
background. Sessions can coexist. File and PendingFile tabs show it; other
tabs hide it. The chord creates a file-picker tab and opens the dock when
needed, and closing the last file tab closes the tree. Below 450 px of
rendered dock width only the tree hides, preserving a 200 px editor and the
existing 360 px dock floor and stored width preference. Unmounting preserves
the panel and watches. Both layouts keep per-tab open state and source-only
file opening, including Markdown.

**Review** puts the same `DiffElement` inside ordinary pane cards. One pane
shows one worktree against one base branch, and the grid caps at six. The pane
header is the standard 34 px: a 13 px branch icon, the project name in `text`
at medium weight, `· branch` in `muted`, and a tooltip carrying the full label
and the agent attribution. No diffstat — the counts live on the file rows.
Both split buttons are present; what a diff pane suppresses is the dock button
and the sessions button. Its right cluster is **four explicit icon buttons** —
refresh, unified/split, expand/collapse all, and Review with agent — not a
shared options menu. `render_diff_options_menu` is private to the dock.

**Review with agent is a fork feature and stays.** The sparkles button opens a
256 px popover headed `Review in a new agent tab` with one row per **supported**
review CLI — `ReviewCli::all()` is rendered unfiltered, so a CLI that is not
installed still gets a row; confirming opens one ordinary workspace tab per pick at the
checkout's cwd, copies the prompt to the clipboard, and prefills the pane
without ever pressing Enter.

The **Workspaces rail** (220) lists open repositories as folder rows that carry
no resting tint and fold their checkouts and worktrees as child rows; a focused
*child* row does take the active tint. Several stay open at once and the folded
set persists in the session. A click replaces the focused pane's subject, a
drag onto a pane edge splits the grid, a drop on the center replaces that
pane's subject, and an accent 6 px dot marks subjects already in the grid.

The **Changes rail** (300) follows the focused pane. Its 36 px header reads
`Changes` in the same 13 px `muted` style as the Workspaces header with only
the tree toggle on the right; a 32 px second row holds the `vs {base}` chip,
which opens a 268 px filterable branch picker capped at 12 rows; a third row,
the filter pill, appears once the diff has files. `u` switches unified and
split, `[` and `]` walk the hunks, and the diff context menu offers copy hunk,
copy file diff, and the same view switch.

Change bars are 4 px, dashed at a 1 px stroke on a 2 px step for deletions.
File headers are 32 px rows that collapse to a 24 px sticky header while
scrolling. Changed rows paint a single line wash, and which one depends on the
resolver in 4.2 — `UiColors::diff_colors()`, not the raw role. **Light** takes
the theme's `vc_*_background` at 0.16. **Dark** splits: a preset that sets
`use_theme_diff_washes` (both Vercel variants) keeps its own 0.16 alphas, and
every other dark preset takes the opaque fallback `#1d3a2b` / `#402425` with
`#16281f` / `#2c1718` gutters. So no bundled dark preset paints the 0.12 alpha
the role itself carries; do not "restore" it.

**Word-level diff was deliberately removed** (`diff/engine.rs:12-16`): on
rewritten lines it painted a second, louder wash over the row tint and read as
noise rather than precision. `vc_word_added` and `vc_word_deleted` survive as
theme slots consumed by nothing. There is no whitespace comparison mode and no
Highlight or Whitespace menu row; the dock's `Dock options` menu is Layout,
Collapse/Expand all, and Refresh Changes only.

A file tab carries git markers in a 6 px column left of the line numbers,
computed against `HEAD` off the render thread and kept current by a block
tracker that shifts blocks on every keystroke and re-diffs the touched blocks
after a 150 ms pause. Added and modified blocks paint a 4 px bar with radius 2
and a 1 px inset; a deleted block is an 8 px dot centered on the boundary.
Hovering widens a marker 3 px to the left; clicking opens a `menu_surface`
popup anchored to the block's row, 280 to 520 px wide, flipping above when
there is no room below. It names the block, shows the base text in the code
font with syntax runs on the `vc_deleted_background` wash (12 rows visible,
200 lines with an `and N more lines` foot), and offers `Copy` and `Revert`; an
added block offers `Revert` alone. Escape, an outside click, or an agent write
closes it. In the Changes tab, hovering a modified file's block shows a
`Revert` pill, 56 by 16 on the sidebar hover tint, inset 10 from the right.

The file header is a 36 px row, gap 6, px 10, with a bottom border, the
file-type icon at the far left, and `Ln {line}, Col {column}`. Banners stack
under it, each a `text_xs` row with `px 3 / py 1.5`, gap 2, and a bottom
`border`: the read-only notice (on `overlay` in `muted`, flashing to
`vc_conflict` at 0.22 in `text` for 600 ms on a refused keystroke), the
on-disk conflict and deletion notices (`vc_conflict` at 0.16, `text`), the
last failed write (`vc_deleted` at 0.16, `text`), and, when the file's
initial parse ran past 5 s, `This file is too complex to color.` on `overlay`
in `muted`; that file stays plain and fully editable. A file opens as plain
text and colors when its tree lands off the render thread; each frame colors
only the stale rows in view under a 2 ms budget, so a scrolled-to region may
read plain for a frame before it colors. An unfocused editor, or one whose
caret is scrolled out of view, does not repaint for the caret blink. Its right end
carries the **Editor Controls trigger** — a 20 px `icon_button_sm` with a 12 px
`icons/editor-controls.svg` glyph. That menu offers Minimap and Scrollbar
toggles scoped to the open file tab; the minimap starts hidden and scrollbars
start visible. It is **the one contextual exception to the shared menu skin**:
a fixed 200 px width, 6 px corners, 1 px border, 4 px vertical padding, a 14 px
left check slot, `.ZedSans` at 14 px, and a small two-layer shadow. Palette
roles stay theme-aware. Escape, an outside click, or a second trigger click
dismisses it.

Editor scrollbars follow Zed's 15 px tracks with square thumbs, a 25 px
vertical minimum and a 28 px horizontal one, and a 1 px left border on the
vertical track and thumb. Both axes support centered track clicks and dragging,
and git markers occupy the vertical track. Changes and Review diffs keep the
same vertical track permanently enabled in its own gutter, without editor
controls or a minimap. The minimap uses `.ZedMono` at 2 px Black with a 1.618
line height, caps at 15 percent of the text area and 80 columns, hides below 20
columns, truncates rows at 160 characters, and paints its background at 0.7
with the viewport thumb at 0.5 behind an open left border.

### 5.5 Settings

Navigation reuses the sidebar width: a 36 px `Back to the app` row, the shared
`filter_pill` search field, and three groups labeled **Personal** (General,
Appearance, Keyboard Shortcuts), **Terminal** (Terminal, Workspaces), and
**Integrations** (AI Agent, MCP Servers). Group eyebrows are 11 px Semibold;
in-page eyebrows are 11 px Normal `muted`.

Pages are a centered column with a 26 px Semibold heading and cards that share
`PANE_CARD_RADIUS` with the pane card, painted as a `squircle_fill` absolute
child rather than a `bg()`, borderless, with `hairline` row separators. Rows
are `toggle_row` or a `setting_text` plus control: title 12 Medium, description
11 `muted`, control right-aligned. Toggles are 36 by 22 with an 18 px white
knob on the fixed `#339cff` track. Selects open a `select_menu` under the
trigger, whose 8 px corner is round — the one non-squircle in the family.
Destructive actions use the fixed red button on a `ROW_RADIUS` squircle.

Keyboard Shortcuts includes a searchable Fixed shortcuts section documenting
editor, text-field, Composer, copy-mode, sidebar, and overlay controls. Each row
names the context in which it applies and is marked Fixed; clicking it cannot
arm recording or write a binding. Hover reveals a truncated description. Live
alternative chords for the same action are also listed; editing either row
rebinds that action.

Keyboard Shortcuts is the one virtualized page (`gpui::list`, owns its scroll):
roughly eighty rows of eight nodes rebuilt every frame made the whole surface
lag. The Appearance page leads with three theme tiles (System, Light, Dark;
134 tall, radius 10, 2 px border) holding a mockup painted from the preset and
a live split-diff sample; the preset itself is a select, not a tile grid.

Workspaces includes a **New tabs** card with a **Default branch** select
(default `main`) and a select for each open Git workspace. Workspace rows show
the folder name and path; they offer **Use default**, **Workspace checkout**,
and the repository's local branches. The default select lists branches from
open workspaces. Overrides persist by workspace cwd, so identical folder names
do not share settings. New-tab actions resolve the selected checkout before
opening the preset picker; a failure shows a toast and opens no tab. Existing
terminals retain their checkout, and non-repository workspaces use their directory.
Selects use the shared keyboard and accessibility behavior.

### 5.6 Menus, selects, tooltips

Popups share `menu_surface` — squircle 18, a surface lifted 0.035 in dark or
`overlay` in light, and a `border` at 0.6 — except the Zed Editor Controls menu
in 5.4. Items are 28 px `ROW_RADIUS` squircle rows with `text` washes for hover
(0.05) and selection (0.10), 12 px text, and a 12 px chevron on triggers.

The 13 px check mark is **not** part of `select_item`: call sites render it,
and they render a transparent placeholder when unselected so rows never
reflow. Widths run 200 to 280 and the list scrolls past 320 px.
`select_menu` is deliberately two elements — a non-scrolling shell that paints
and clamps, plus an inner `overflow_y_scroll` list — because GPUI applies a
scroll offset to absolute children.

Tooltips are squircle 14 on the title bar color, padding 8 by 6, small text,
shown after 800 ms through `delayed_tooltip`. Their border is `border` at
**full** alpha, unlike the menu's 0.6.

### 5.7 Launch Pad, Composer, palette

**Launch Pad** is 520 wide at radius 10, horizontally centered but anchored
72 px from the top over a full-window black 0.4 backdrop. It stacks an `Agent`
list (max height 180, radius 6, 13 px marks, 12 px rows, disabled rows marked
`not installed` at 10 px), a fork-only GitHub issue row with a `Load issue`
button, a `New branch` field, an optional `Prompt` field (max height 140), the
footer hint `Enter: load or create · Tab: fields · Esc: cancel`, and a confirm
button whose label is tri-state: `Create worktree + launch`, `Creating…`,
`Loading issue…`. That button is **tinted, not filled** — `accent` at 0.15
behind `accent` text. Tab cycles the three text fields; the agent list is
mouse-driven; Escape and outside clicks are refused while a worktree run is in
flight.

**The Composer** dims the whole pane under a 0.25 black scrim and docks a
bordered panel on `overlay` at the bottom: a `Composer` label at 11 px Medium,
then 10 px chips on radius 4 for the broadcast toggle (`Single pane` on
`subtle`, or `Broadcast: {group}` on `accent` at 0.15), `agent generating -
Enter queues` on `vc_modified` at 0.15 while the agent is busy, and a
`{n} queued · cancel` chip whose hover lerps `muted` into `vc_deleted`.
**Enter pre-fills without submitting**; `Cmd+Enter` pre-fills and submits; a
broadcast never submits even explicitly, and the hint line says so. Escape
closes. Input caps at 64 KiB.

**The pane palette** fills an empty tab named `New pane` with a centered 260 px
column: a 13 px Semibold title, an optional 28 px branch row whose select opens
a 260 px menu, one 34 px row per preset with its 14 px agent mark and a `not
installed` marker at 10 px, and an inline error in `vc_deleted` at 11 px.
Escape folds the branch menu before it closes the palette; arrows move and
scroll the selection into view. Split placement renders no branch row and never
opens the sessions rail; Tab placement may, when `new_pane_shows_sessions` is
on.

### 5.8 Feedback

**Toasts** appear bottom right on `subtle` with a 15 px icon and 12.5 px text
on a single ellipsized line. They **do not stack**: one is visible and the rest
queue FIFO. They carry **no action row and no dismiss affordance** — they time
out after 180 ms in, the hold, then 180 ms out, entering on an 8 px lift and
leaving on an 8 px drop. The hold is 1440 ms by default and is carried per
`Toast`; 4.8 names the deliberate longer-lived cases. Error text is detected from twelve substrings and
takes the alert glyph in `agent_error` with a wider 440 px cap.

**Callouts** (`widgets/callout.rs`) are a 16 px icon, a 14 Semibold title, and
a 13 `muted` description, max width 560, on `surface` inside a 1 px border in
the accent hue, using `accent` for info and the fixed warning and error hues.
Their 8 px corner is a plain round — the one piece of chrome that is not a
squircle.

**Empty states** (`panel_empty_state`) center an 18 px `muted` glyph, an
optional 14 Semibold title, and a 12 px `muted` message; the glyph spins while
scanning, and that spinner honors `reduce_motion`.

### 5.9 Pane Overview

Fork-only (issues #339, #353, #389); upstream has no equivalent. `Cmd+Shift+P`,
Window ▸ Show All Panes, or the sidebar header button opens a cross-workspace
grid of every **terminal** pane, grouped workspace then tab. Markdown and diff
panes are omitted and the surface is gated to Agents mode.

The panel is top-anchored at `OVERVIEW_MARGIN` (24), inset 24 on each side,
with radius 12, a 1 px border, and `shadow_lg` on a black 0.4 scrim.
Cards have radius 8, gap 10, grid padding 16, a 30 px title row, a 28 px
status row, and a 26 px footer. Column count is
`floor((width + gap) / (card_w + gap)).max(1)`.

The Show all panes control and its editable Settings row in Panes & splits
share that name; the row also matches “pane overview”. Its tooltip shows the effective shortcut,
omitting the chord when unassigned. The control uses the shared small icon
button and the standard 800 ms tooltip delay.

Overview cards are 312.5 by 192.5 px, 25% larger than the previous 250 by 154.
The terminal preview uses a 6.25 px face, 25% smaller than before, in a 98.5 px
band cropped from the bottom. Terminal content stays read-only. Full-size
12 px status labels sit outside the preview. Unread input, error, and stalled
states carry a bell, an explicit Unread label, and a semantic border; keyboard
selection retains the accent border. Terminal running / Exited is shown
separately from agent status, and exited previews are dimmed. Status text uses
the shared contrast floor. Card accessible names include both states.

Filtering matches **metadata only** — pane, workspace, and tab titles, the
agent name, the cwd basename — because content search belongs to Fleet Search.
Left and right move by one in flat order and never wrap; up and down preserve
the visual column across workspace boundaries. Enter or a click teleports to
the surface, re-resolving it by id so a pane closed since render is a clean
no-op.

Two caps hold the surface honest: at most 24 live thumbnails (the rest render a
static `Preview paused` shell) and 64 characters of any untrusted label. It
refreshes on its own 250 ms timer that no-ops while closed rather than
subscribing to terminal wakeups. **The thumbnail never routes through
`TerminalElement`**, whose `build_layout` would resize the child's PTY; it hangs
off the window-free `layout_from_snapshot`, culls off-screen cards in prepaint
before taking any lock, forces a block cursor at `cursor` 0.5, and paints no
selection, copy-mode, or search highlights.

## 6. Interaction

### 6.1 Keyboard first

`secondary` is **Cmd**. This is a macOS-only fork, so the cross-platform arm of
`keybindings/display.rs` is dead code. Chords render as Apple HIG glyphs with
no separator. Every default-bound action is remappable in Settings ▸ Keyboard
Shortcuts, and every modal answers Escape. Enter confirms wherever a modal has
a single default action — System Info is the standing exception, with two
footer buttons and no default (7.4).

Not every overlay has an opening chord, and the table below is the whole set
that does. Custom Buttons opens only from the workspace context menu; About,
System Info, and Check for Updates are menu-bar only (6.1's list of twelve
unassignable actions); and the theme picker is reachable only through
`render_profile_menu`, which nothing opens (11). A **new** overlay SHOULD take
a chord or a menu item, and MUST NOT rely on a surface that has neither.

| Surface | Default |
| --- | --- |
| Split horizontal, vertical | `secondary-shift-d`, `secondary-shift-e` |
| Close pane, undo close pane | `secondary-shift-w`, `secondary-shift-t` |
| Focus across the grid | `alt-arrow` |
| New tab, close tab | `secondary-alt-t`, `secondary-w` |
| Next, previous tab | `secondary-]`, `secondary-[` |
| New, close workspace | `secondary-shift-n`, `secondary-shift-q` |
| Next workspace | `ctrl-tab` |
| Workspaces 1 to 9 | `secondary-1` to `secondary-9` |
| Layout presets | `secondary-alt-1` to `secondary-alt-4` |
| Zoom, equalize, swap | `secondary-shift-z`, `secondary-shift-=`, `secondary-shift-s` |
| Review | `secondary-shift-g` |
| Pane overview | `secondary-shift-p` |
| Work review | `secondary-shift-u` |
| Primary sidebar, files rail | `secondary-alt-b`, `secondary-alt-f` |
| Maximize / restore the Changes dock | `secondary-shift-f` |
| New file tab, new terminal tab (dock) | `secondary-g`, `secondary-j` |
| Composer, Launch Pad | `secondary-shift-space`, `secondary-shift-l` |
| Attention queue, jump to next waiting agent | `secondary-shift-a`, `secondary-shift-j` |
| Broadcast groups, toggle member | `secondary-shift-m`, `secondary-shift-b` |
| Copy, paste (Terminal) | `cmd-c` / `cmd-v`, plus `ctrl-shift-c` / `ctrl-shift-v` |
| Clear scrollback, reset terminal | `secondary-shift-k` and `cmd-k`; `secondary-shift-r` |
| Prompt marks | `secondary-shift-up`, `secondary-shift-down` |
| Font size up, down, reset | `secondary-=`, `secondary--`, `secondary-0` |
| Copy mode, find in buffer, fleet search | `ctrl-shift-x`, `ctrl-shift-f`, `alt-f` |
| Diff: hunks, view, dismiss | `]`, `[`, `u`, `escape` |
| Quit | `cmd-q` |

Four bans are enforced by tests and MUST hold:

1. **`secondary-tab` is never bound.** macOS reserves Cmd+Tab for the
   application switcher and never delivers it; next-workspace is `ctrl-tab`.
2. **`Cmd+,` is deliberately unbound.** The macOS Preferences chord stays free.
3. **`ctrl-c` never reaches `terminal_copy`**, so SIGINT survives.
4. **No two defaults claim one chord in one context.**

Twelve actions carry no registry row and are permanently unassignable —
`TerminalSelectAll`, `About`, `CheckForUpdates`, `Copy`, `Paste`, `SelectAll`,
`OpenHelp`, `OpenSettings`, `MinimizeWindow`, `ZoomWindow`, `ShowSystemInfo`,
`ReportIssue`. That is deliberate: they are menu-bar items, and listing them
would grow Settings ▸ Keyboard Shortcuts a column of permanently `Unassigned`
rows. A new menu-only action follows the same precedent.

### 6.2 Pointer

The shell cursor is Arrow. `PointingHand` appears only on rows and buttons that
act; text fields show the text cursor; dividers show column or row resize.
Hover reveals destructive or secondary controls (the pane close chip, sidebar
row actions) rather than showing them at rest; the primary pane actions stay
visible.

Five drag payloads exist: `PaneDrag` (pane header into a pane edge, or into the
sidebar to become a new tab), `SessionDrag` (sessions rail into a pane),
`ReviewSubjectDrag` (fork-only, the Review rail into a pane edge or center),
`TabDrag` (between workspaces), and `WorkspaceDrag` (rail reorder, suppressed
under auto-sort). Finder folders arrive as `ExternalPaths` on the sidebar and
on a terminal.

**Only the pane split preview is blue.** The swap preview, the sidebar
placeholder, and the reorder line are neutral `text` tints (4.3). The drag
ghost is a 6 px chip with 13 px Medium text, a 12 px icon, and `shadow_lg`; the
workspace ghost is a wider variant at `SIDEBAR_WIDTH − 16`.

### 6.3 Focus and attention

Focus is shown by absence of dim: the focused pane stays at full contrast while
its siblings fade. There is no focus ring (7.5). An agent that needs the user
gets the `vc_conflict` border at 0.7 and a sidebar bell; clicking anywhere in
the panel acknowledges visible completions. The attention queue lists those
panes and `secondary-shift-j` jumps through them. A native notification is
dropped only when its pane is under the user's eye: the window is focused and
the pane's workspace and tab are on screen (the same test as the completion
dot, #408 / #422). A pane in another workspace, a background tab, a zoomed-away
split, or an unmounted dock notifies even while the window is focused.

## 7. Accessibility

This section has no upstream counterpart. It records what issues #275, #316,
#317, #321, #340, and #361 landed, and it is **normative**: a new surface that
does not meet it is not finished.

### 7.1 Every icon-only control is named

`ui_primitives.rs:585`'s private `icon_button` takes `label` as a **required
positional argument** and applies three things together, so an icon button
cannot be constructed unnamed:

```rust
.role(Role::Button)
.aria_label(label.clone())
.delayed_tooltip(text_tooltip(label))
```

`icon_button_sm` (20 / 12 px) and `icon_button_md` (24 / 13 px) are the public
wrappers and MUST be preferred over a hand-rolled div. `toolbar_pill` takes an
`Option<SharedString>`, and `None` is the one documented opt-out — a pill whose
visible text already names it — which still carries `Role::Button`.

### 7.2 State is announced, not just painted

| Control | Role | State |
| --- | --- | --- |
| Icon buttons, sidebar actions, menu triggers, nav rows | `Role::Button` | `aria_label`; `a11y_disabled` where a control can be disabled |
| Settings toggles (`toggle_switch`) | `Role::Switch` | `aria_toggled`, `tab_index(0)` |
| Select triggers | `Role::ComboBox` | `aria_expanded`, `tab_index(0)` |
| Select lists and rows | `Role::ListBox` / `ListBoxOption` | `aria_selected` |
| Terminal search status | `Role::Status` | the status string as `aria_label` |

Two rules follow, both learned the hard way and both guarded by tests:

- **Never add a key handler to a switch.** GPUI synthesizes a
  `ClickEvent::Keyboard` from Space and Enter on the focused div; a second
  handler double-toggles.
- **Every select caller MUST chain an `on_click` arm that accepts
  `ClickEvent::Keyboard`**, or VoiceOver's Click never reaches it.
  `settings_selects_are_accessible_comboboxes` walks `src/settings/**` and
  fails with a `file:line` when a trigger lacks one;
  `settings_toggles_are_accessible_switches` does the same for a tab that wraps
  `toggle_pill` directly.

The animated hover wrapper forwards `a11y_role`, `write_a11y_info`, and
`a11y_synthetic_children`, so wrapping an element never drops its semantics.
When an assistive client is attached, the pane hands it the **uncached**
terminal element so the tree rebuilds rather than serving a stale snapshot.

### 7.3 Contrast is computed, not eyeballed

`apca_contrast(text, bg)` (`terminal/element/color.rs:79`) implements APCA
0.0.98G-4g. `ensure_minimum_contrast(fg, bg, min_lc)` (`:123`) is the cached
repair path: it searches lightness, then desaturation, then black or white.

**`MIN_APCA_CONTRAST` is 45.0** (`terminal/element/mod.rs:71`) — the minimum
for large fluent text under ARC Bronze Simple Mode. It is enforced at every
observation point where a color meets a wash the theme did not choose: per-cell
terminal text, the search-hit wash, the selection foreground, the exit banner
and IME preedit, the Settings shortcuts accent label, and the MCP button label
shared with the sidebar callout. The About dialog holds a **higher** floor of
60.0 for body text, with 45.0 for secondary text.

Any new surface that paints text on a computed or themed wash MUST either route
through `ensure_minimum_contrast` or ship a test that walks all eight bundled
variants. Syntax slots are the documented exception: they use a soft `Lc > 5.0`
separation check, because a palette's whole point is closely related hues. Diff
row washes carry no floor today.

### 7.4 Keyboard-operable surfaces

These answer arrows, Enter, and Escape in full: attention queue, Pane Overview
(two-dimensional), sessions rail, files rail, theme picker, pane palette, fleet
search, work review, broadcast groups, and the Editor Controls menu. Launch
Pad, the diff branch menu, close confirm, and About answer Escape and Enter
only; Launch Pad additionally cycles its text fields with Tab, and its agent
list is mouse-driven by design. **System Info is Escape-only** — it carries
two footer buttons, Close and Copy, and neither is the default, so Enter is
deliberately ignored.

A new overlay MUST at minimum dismiss on Escape, and MUST confirm on Enter
when it has a single default action. A new list surface SHOULD be
arrow-navigable.

### 7.5 Known accessibility gaps

These are real and MUST NOT be described as solved:

1. **There is no Tab ring and no visible focus ring anywhere.**
   `window.focus_next` is not bound and GPUI does not auto-bind Tab, so only
   eight `tab_index(0)` call sites exist and focus is conveyed solely by the
   absence of dim.
2. **The workspace and tab rail is not keyboard-navigable.** It handles keys
   only for the inline rename editor.
3. **Settings nav rows carry a role and a name but no `tab_index`**, so they
   are not tab stops.
4. **No repo-wide scan asserts every icon button is named.** The primitive makes
   it hard to get wrong and two scans cover Settings, but coverage elsewhere is
   per-surface.
5. **`reduce_motion` is config-driven, not OS-driven.** It does not read the
   macOS "Reduce Motion" system setting.
6. **The Review-with-agent popover is mouse-only.** It has no focus handle and
   no key handler; its rows are click-only and the sole dismissal is an outside
   mouse press (`pane/review.rs`). It does not meet the Escape floor above, and
   it is the one live overlay that does not.

## 8. Platform Material

macOS only. There is one row, not four, and `scripts/linux-census.sh` keeps it
that way.

| Layer | Behavior |
| --- | --- |
| Window surface | `window_backdrop` defaults to `auto`, which asks GPUI for a transparent window surface. `blurred`, `transparent`, `opaque`, and `off` override it; the legacy `mica` and `acrylic` spellings still load; `PANEFLOW_WINDOW_BACKDROP` overrides for one launch |
| Material | `NSVisualEffectMaterial::Sidebar`, blending `BehindWindow`, state `FollowsWindowActiveState`, installed on the **whole NSWindow content view below GPUI's render view** — so every transparent shell region exposes it: the rail, the title bar, and the pane gutters |
| Toggle | `macos_chrome_material` defaults **on** and hot-reloads. Toggling calls `setHidden:` rather than tearing the view down. Appearance follows theme lightness through `NSAppearanceNameVibrantLight` / `Dark`. If installation fails, the window falls back to `WindowBackgroundAppearance::Blurred` |
| Fullscreen | The material is suppressed: `chrome_material_for_frame(enabled, is_fullscreen) = enabled && !is_fullscreen`. A source-probe test forbids reading the config getter bare |
| Decorations | `window_decorations` is read once at startup. `client` (default) paints the CSD button group outside fullscreen; `server` opts out. An invalid value logs and falls back to `client` |
| Caption | The macOS traffic lights, given 80 px of brand padding that drops to the 8 px edge inset in fullscreen |

Rules that follow:

1. A surface MUST hold with the material off. Never rely on transparency for
   contrast or separation; the corner masks and the surface ramp carry the card
   reading on their own.
2. Anything drawn over the material MUST be `transparent_black` where the
   material should show and the opaque shell color where it should not. The
   helpers in `constants.rs` decide — and they are stronger than they look:
   `cockpit_chrome_background` returns `transparent_black()` unconditionally,
   discarding its arguments, while `cockpit_backdrop_background` returns
   transparent only when the material is active. Do not branch on `target_os`
   in render code.
3. `windows_chrome_material` and `windows_terminal_material` are gone from the
   struct and the published schema. The loader still accepts them as ignored
   no-ops so existing `paneflow.json` files keep loading, and a test pins that.
   Do not reintroduce them as fields.

## 9. What Not To Do

These are the PaneFlow-specific bans, in addition to the generic ones a design
review would raise anywhere.

- **Shadows on chrome.** No shadow on chrome, rows, tabs, menus, or toasts. A
  floating overlay or dialog takes `shadow_lg()` — that is the fork's standard
  skin for them, and the window shell and both drag ghosts keep theirs. The
  **pane close chip** is a named Contextual exception: it lifts off the card on
  `shadow_lg` (`pane.rs:1285-1312`, 5.3) because it floats over live terminal
  output. No other chip takes one.
- Separators between tabs, chips, or toolbar buttons. The floating chip
  language replaced full-height bordered tabs. The dock tab strip's bottom
  hairline is the one deliberate rule, and it separates the strip from the
  content, not the chips from each other.
- Identity pills, badges, or logos in the pane header. The sidebar owns
  identity.
- Accent fills on anything larger than a button — and the Launch Pad's primary
  button is *tinted*, not filled, so it is the ceiling in both senses.
- Hue in a neutral. If a gray reads warm or cool, it is a bug unless the preset
  is Claude, whose paper and graphite are the identity.
- A new radius, a new text size, or a new hover color. Pick from 4.4, 4.6, 4.3.
- A per-component light or dark branch when a `UiColors` role models the state.
  Lightness checks belong in `constants.rs`, `theme/model.rs`, and
  `settings/components.rs`.
- A hex literal in render code outside the fixed values in 4.3.
- Motion that does not explain a state change, and any new animation that keeps
  running under `reduce_motion`.
- A tooltip without the 800 ms delay, or a control without a tooltip when its
  glyph is not self-evident.
- An icon-only control built without `icon_button_sm` / `icon_button_md`, a
  custom control that paints a state assistive technology cannot read, or a
  select trigger with no `ClickEvent::Keyboard` arm. Section 7 is the floor.
- Blocking the render thread for a visual. Snapshots, git, and file walks go
  through `smol::unblock`; see `ARCHITECTURE.md`.
- A `#[cfg(target_os = "linux")]` or `#[cfg(windows)]` branch, a Mica or
  Wayland material path, or a non-macOS caption glyph.

## 10. Delivery Gate

**Visual.** Before a UI change is ready for review, confirm on a real build:

1. PaneFlow Dark and PaneFlow Light, plus one of Vercel, Claude, or Cursor in
   both variants.
2. `macos_chrome_material` on and off.
3. `reduce_motion` on: every animation **you touched** settles without
   interpolation. Do not attest more than that — 4.8 lists five animations
   that still ignore the flag (menus fade in through `menu_reveal` and snap
   under the flag), so "nothing moves" is not yet true of the app.
4. The 800 by 500 minimum window, with the primary sidebar hidden and a right
   rail or the diff dock open at the same time. Check that the dock's render
   floor behaves — below roughly 464 px of remainder it MUST disappear cleanly
   rather than squeeze the grid.
5. Long titles, long paths, and missing optional data: truncation follows 4.6
   and rows never wrap.
6. Hover, active, selected, disabled, loading, empty, error, and attention
   states of every control you touched.
7. Keyboard and assistive technology: reach every new control without the
   mouse, and confirm VoiceOver announces its name and its state (section 7).
8. A capture of the surface in the pull request, and a line listing which
   variants you actually ran.

**Engineering.** The same six gates every change on this fork runs, before and
after, quoting the real output — never a piped exit status:

```bash
cargo build                                     # exit 0
cargo test --workspace                          # diff test names, do not trust the integer
cargo clippy --workspace --all-targets          # exit 0, WARNING COUNT 1 (block v0.1.6)
cargo clippy --workspace --all-targets -- -D warnings   # exit 0; what CI enforces
cargo fmt --check                               # exit 0
./target/debug/paneflow --version               # paneflow 0.5.0
cargo deny check advisories licenses sources    # exit 0
```

Both Clippy forms matter and neither is a superset of the other: the counted
form is the local gate CLAUDE.md prescribes, and the denying form is what the
macOS job runs (`.github/workflows/run_tests.yml`, which pins
`--locked --target aarch64-apple-darwin`). The workflow comment says the two
must not drift; run both before pushing a UI change.

Shared primitives to reach for first, in `src-app/src/ui_primitives.rs`:
`AnimatedHoverExt`, `squircle_skin`, `icon_button_sm`, `icon_button_md`,
`toolbar_pill`, `filter_pill`, `section_eyebrow`, `panel_empty_state`,
`text_tooltip`, `delayed_tooltip`. In `src-app/src/settings/components.rs`:
`setting_card`, `toggle_row`, `toggle_switch`, `setting_text`,
`select_trigger`, `select_menu`, `select_item`, `select_listbox`,
`select_option`, `menu_surface`, `secondary_button`, `destructive_button`,
`hairline`. Add a primitive only when two surfaces already need the same
behavior.

## 11. Known Gaps

- Custom user themes are not loaded. New palettes ship as presets in
  `theme/builtin.rs` with both variants, a `UiColors`, and a syntax palette.
- `window_decorations` and `window_backdrop` are read once at startup.
- `reduce_motion` reaches only four animations (4.8) and does not follow the
  macOS system setting.
- The six accessibility gaps in 7.5, of which the missing focus ring is the
  most consequential.
- The About dialog is **Migration** in shape only: issue #273 moved its colors
  onto `UiColors`, but it still paints a plain 10 px round with a shadow
  instead of the squircle card that System Info already uses. Its CRT credit
  plate keeps its own fixed hexes on purpose and is **Contextual**.
- The title bar still carries dead breadcrumb and IPC-pill code behind
  `!cockpit`, and `render_profile_menu` is unreachable because nothing ever
  sets `profile_menu_open`. All three are **Migration**; delete rather than
  revive.
- The app's own context menus use plain 4 px and 7 px rounds instead of
  `ROW_RADIUS`, and there is no `menu_item` primitive to unify them.
- `tool_card_header_bg`, `vc_word_added`, and `vc_word_deleted` are defined by
  every preset and consumed by nothing.
- The traffic-light brand padding is an inline `px(80.0)` literal rather than a
  named constant.
- Roughly two hundred `text_size(px(N.))` literals remain where a named size
  from 4.6 belongs.
- 4.3's fixed-value table is the set a **new** surface may draw from, and the
  rows marked **Migration** there are existing debt rather than precedent. It
  is kept accurate against render code — `grep -rE 'rgba?\(0x' src-app/src`
  outside `theme/` is the check — but a literal added without a table row is a
  contract violation, not a new fixed value.
