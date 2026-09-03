//! Sidebar rendering for `PaneFlowApp`: workspace rows, action buttons,
//! notification dropdown, and the context-menu row helpers (in the
//! [`context_menu`] submodule).
//!
//! Extracted from `main.rs` per US-025 of the src-app refactor PRD - pure
//! code-motion, behaviour unchanged. Toast utilities and sidebar-adjacent
//! types (`WorkspaceContextMenu`, `WorkspaceDrag`, `WorkspaceDragPreview`)
//! remain in `main.rs` because they cross module boundaries.

pub(crate) mod context_menu;

use crate::ui_primitives::TooltipDelayExt;
use gpui::{
    Animation, AnimationExt, AnyElement, App, AppContext, ClickEvent, Context, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Role, SharedString,
    Styled, Window, div, prelude::*, px, rgb, svg,
};

use crate::{
    PaneFlowApp, SIDEBAR_WIDTH, TabContextMenu, TabDrag, WorkspaceContextMenu, WorkspaceDrag,
    WorkspaceDragPreview, ai_types,
    app::pane_palette::PalettePlacement,
    app::workspace_ops::WorkspaceFocusTarget,
    pane_drag::PaneDrag,
    ui_primitives::{ROW_RADIUS, squircle, squircle_skin},
    workspace::{Tab, Workspace},
};

/// Memoized rail ordering. Group labels stay hidden, but sibling worktrees
/// remain contiguous as before the visual redesign - under Manual ordering and,
/// since issue #107, inside each Auto bucket too.
///
/// `signature` is the whole invalidation mechanism: nothing clears this cache on
/// mutation, so a field the order depends on that is missing from
/// [`PaneFlowApp::sidebar_order_signature`] can never reorder the rail.
#[derive(Default)]
pub(crate) struct SidebarOrderCache {
    signature: Option<u64>,
    order: Vec<usize>,
}

/// Debug-only render budget guard for the CLI sidebar. Mirrors the Agents
/// sidebar canary so projection or card regressions show up during profiling
/// without adding user-facing log noise.
struct SidebarRenderTimeCanary {
    start: std::time::Instant,
    workspace_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarAgentState {
    NeedsInput,
    Errored,
    Stalled,
    Finished,
    Thinking,
}

/// One row of the rail, in render order: a workspace folder row, or one of
/// its tab rows. The gaps between these rows are what a sidebar drop actually
/// aims at, so the same flattened plan drives both the rows and the dividers
/// between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarRow {
    Folder(usize),
    Tab(usize, usize),
}

/// An insertion point of the rail: the gap between two rows, or either end of
/// the list. Rendered as its own divider element sitting *between* the cards
/// rather than as a border painted on one of them - a card that lights up says
/// "into this one", which no sidebar drop ever means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SidebarDropSlot {
    /// `(workspace index, tab index)` a dropped tab or pane lands at. `None`
    /// for the gap above the very first row: it precedes every workspace, so
    /// it belongs to none of them.
    tab: Option<(usize, usize)>,
    /// Insertion index for a dropped folder, set only on the gaps that
    /// actually separate two folders. A gap inside a tab list is not a
    /// workspace boundary and shows no line for a folder drag.
    workspace: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidebarAgentSummary {
    state: SidebarAgentState,
    count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidebarServiceSummary {
    primary: u16,
    overflow: usize,
}

/// Visual strength of the workspace text that may be quieted when the
/// workspace has no foreground work. Kept separate from the row shell and
/// agent badge so neither can accidentally inherit the idle treatment.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SidebarWorkspaceTone {
    title_opacity: f32,
    meta_opacity: f32,
}

/// Match the Files tree's established quiet step, but apply it only to a
/// workspace's title and service metadata. The row shell, action buttons, and
/// agent badge stay at full strength.
const IDLE_WORKSPACE_TEXT_OPACITY: f32 = 0.55;

fn sidebar_workspace_tone(
    is_selected: bool,
    is_idle: bool,
    is_waiting_for_input: bool,
) -> SidebarWorkspaceTone {
    let opacity = if is_idle && !is_selected && !is_waiting_for_input {
        IDLE_WORKSPACE_TEXT_OPACITY
    } else {
        1.0
    };
    SidebarWorkspaceTone {
        title_opacity: opacity,
        meta_opacity: opacity,
    }
}

/// Quiet step for a tab row's title, on a second axis entirely:
/// [`sidebar_workspace_tone`] asks "is this workspace doing anything", this
/// asks "is this workspace the one on screen". They are independent - a busy
/// background workspace keeps its folder title bright and still recedes as a
/// tree - so the two must not be folded into one predicate.
///
/// Reuses [`IDLE_WORKSPACE_TEXT_OPACITY`] on purpose: the rail has one quiet
/// step, not two, or the two dims would read as a ranking.
fn sidebar_tab_title_opacity(is_active_workspace: bool) -> f32 {
    if is_active_workspace {
        1.0
    } else {
        IDLE_WORKSPACE_TEXT_OPACITY
    }
}

/// Wash standing in for a text selection behind a still-seeded rename buffer.
///
/// `UiColors` carries no selection slot, so `accent` - the "highlighted items"
/// token - is the honest choice. The alpha is deliberately heavier than the
/// 0.15 the app uses for a resting/active row fill: at that strength selected
/// text reads as merely hovered, and the whole point of the seeded state is
/// that the next keystroke destroys the value.
const RENAME_SELECTION_ALPHA: f32 = 0.3;

/// Background and body text of the inline rename editor.
///
/// Seeded (nothing typed yet) the whole value reads as selected - accent wash,
/// and no trailing caret, because there is no insertion point to mark. Once
/// the user has typed, it is the ordinary editor: overlay fill and a caret at
/// the end of the buffer.
///
/// One definition for both row shells. They already share `rename_text`, and a
/// selection only one of them painted would make the same gesture look like
/// two different modes.
fn rename_editor_skin(
    text: &str,
    seeded: bool,
    ui: crate::theme::UiColors,
) -> (gpui::Hsla, String) {
    if seeded {
        (ui.accent.opacity(RENAME_SELECTION_ALPHA), text.to_string())
    } else {
        (ui.overlay, format!("{text}|"))
    }
}

/// Consume a still-seeded rename buffer before the keystroke that lands on it.
///
/// A rename opens with its seeded value selected, so the first printable key
/// replaces the whole value instead of appending to it, and the first
/// backspace clears it instead of shaving one character off a name the user
/// never typed. Returns whether the selection was the thing consumed, which is
/// what tells backspace not to also `pop()`.
fn take_rename_selection(text: &mut String, seeded: &mut bool) -> bool {
    if std::mem::take(seeded) {
        text.clear();
        true
    } else {
        false
    }
}

pub(crate) const SIDEBAR_ROW_MARGIN_X: f32 = 8.0;
pub(crate) const SIDEBAR_ROW_PADDING_X: f32 = 8.0;
pub(crate) const SIDEBAR_ROW_PADDING_Y: f32 = 6.0;
/// Separates a row's title line from its meta line (branch / service).
const SIDEBAR_ROW_GAP: f32 = 4.0;
/// Height of a row's title line, and with it the height of a single-line row:
/// `SIDEBAR_ROW_LINE_HEIGHT + 2 * SIDEBAR_ROW_PADDING_Y`.
///
/// Set explicitly because the default line height is a multiple of the font
/// size, so the rail's row height moved with the font metrics - it measured 23
/// px here and made every row 35 px tall. Pinning it keeps the row at 30 px,
/// the height the rail is designed against, whatever font resolves.
pub(crate) const SIDEBAR_ROW_LINE_HEIGHT: f32 = 18.0;
const SIDEBAR_TITLE_ROW_GAP: f32 = 8.0;
const SIDEBAR_AGENT_STATUS_SLOT_WIDTH: f32 = 48.0;
const SIDEBAR_AGENT_ICON_SLOT_WIDTH: f32 = 20.0;
/// Group the rail's file-manager drop placeholder reads its visibility from.
const SIDEBAR_DROP_GROUP: &str = "sidebar-drop-zone";
/// Gap between the drop placeholder and the rail's edges, so the rounded box
/// floats inside the sidebar instead of tracing its border.
const SIDEBAR_DROP_PLACEHOLDER_MARGIN: f32 = 6.0;
const SIDEBAR_DROP_PLACEHOLDER_RADIUS: f32 = 8.0;
/// Fill / hairline alpha of the drop placeholder, matching the pane-swap
/// placeholder (`pane.rs`) so both neutral drop targets read the same.
const SIDEBAR_DROP_PLACEHOLDER_FILL_ALPHA: f32 = 0.10;
const SIDEBAR_DROP_PLACEHOLDER_BORDER_ALPHA: f32 = 0.22;
/// Side of one square button of a row's hover action cluster.
const SIDEBAR_ACTION_BUTTON_SIZE: f32 = 20.0;
/// Gap between two buttons of the same cluster.
const SIDEBAR_ACTION_BUTTON_GAP: f32 = 4.0;
/// US-010: room the hover action cluster needs in the right corner of a
/// workspace row, plus the gap that keeps it off the trailing agent badge. The
/// folder cluster carries two buttons - "new pane" and "close workspace".
///
/// It is trailing padding on the title row, held at all times rather than
/// opened on hover: reserving it only under the pointer left whatever could
/// not shrink sitting under the button.
const SIDEBAR_ACTION_LANE_WIDTH: f32 = SIDEBAR_TITLE_ROW_GAP
    + SIDEBAR_ACTION_BUTTON_SIZE
    + SIDEBAR_ACTION_BUTTON_GAP
    + SIDEBAR_ACTION_BUTTON_SIZE;
/// Vertical space between two rows of the rail. The divider element *is* that
/// space (the list itself sets no gap), so inserting the drop slots costs no
/// layout: nothing moves when a drag starts.
const SIDEBAR_ROW_SPACING: f32 = 4.0;
/// Thickness of the insertion line. Two pixels, not one: a hairline reads as
/// an artifact of the row above it.
const SIDEBAR_DROP_LINE_PX: f32 = 2.0;
/// How far above and below its gap a divider's invisible drop band reaches -
/// half a single-line row, so the bands of consecutive gaps tile the rail
/// without overlapping. Dropping anywhere over a row therefore aims at the
/// nearest gap, and the line the user sees is the one they get.
const SIDEBAR_DROP_BAND_REACH: f32 = SIDEBAR_ROW_LINE_HEIGHT / 2.0 + SIDEBAR_ROW_PADDING_Y;
const SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH: f32 =
    SIDEBAR_WIDTH - SIDEBAR_ROW_MARGIN_X * 2.0 - SIDEBAR_ROW_PADDING_X * 2.0;
/// US-008: leading affordances of a workspace folder row - the folder icon and
/// the gap before the title. The open/closed folder glyph is the whole
/// disclosure affordance; there is deliberately no chevron next to it.
///
/// A tab row reserves the icon width with an invisible placeholder and nothing
/// more, so every title in the rail - folder and tab alike - starts on the same
/// X. The folder icon is then the only thing that distinguishes a workspace
/// from its tabs.
const SIDEBAR_FOLDER_ICON_WIDTH: f32 = 14.0;
/// US-013: geometry of the per-pane icon cluster carried by a tab row.
///
/// The cap is the visual reference's: past four panes the cluster would eat
/// the title before it reaches the activity badge, so the tail folds into a
/// `+N`. Each slot is fixed and painted whether or not the pane's agent is
/// known yet, so the scan landing later swaps a glyph in place instead of
/// reflowing the row (edge case 15).
const SIDEBAR_TAB_ICON_SIZE: f32 = 16.0;
const SIDEBAR_TAB_ICON_GAP: f32 = 3.0;
const SIDEBAR_TAB_ICON_CAP: usize = 4;
/// One card per pane, not one capsule around the stack: each glyph carries its
/// own rounded fill plus hairline, and the cards slide over each other. The
/// height is the row's line height, so a stack never grows the row; the width
/// and the glyph keep the visual reference's proportions (card slightly taller
/// than wide, glyph about two thirds of the card width).
///
/// The card is a square chip: 24x24 around a 16px glyph, so the padding is
/// exactly 4px on all four sides. Both the difference (24 - 16 = 8) and each
/// side (4) are whole pixels on purpose - an odd gap splits into 4.5px per side
/// and GPUI's rounding then lands the glyph half a pixel off center, which on a
/// mark this small is visible as a lean.
///
/// Centering is left to `items_center` + `justify_center` and nothing else: no
/// per-icon nudge. The glyphs are optically centered *inside their own
/// viewBox*, which is the fix that survives a size change - see `codex.svg`,
/// whose viewBox is widened around the blossom's bounding box so the mark
/// carries the same relative margin as the Lucide-derived icons next to it.
///
/// The card is *taller* than the row's line height (18px). It is laid out
/// absolutely and overhangs into the row's 6px vertical padding rather than
/// pushing the row past 30px, so the height must stay under that 30px total -
/// the row shell clips its overflow, and a card past 30px would paint sliced.
/// `tab_card_fits_inside_a_row` guards both bounds.
///
/// The corner is the rail's own: `ROW_RADIUS`, traced by the same `squircle`
/// primitive the rows use, not GPUI's `rounded()` circular arc. A card sitting
/// inside a row must not answer its container's continuous corner with a
/// different curve. `trace` clamps the radius to half the shorter side, so a
/// 14px corner on a 24px card resolves to the full superellipse - the card is
/// all corner, which is exactly what makes it read as the row's own material.
const SIDEBAR_TAB_CARD_WIDTH: f32 = 24.0;
const SIDEBAR_TAB_CARD_HEIGHT: f32 = 24.0;
const SIDEBAR_TAB_CARD_ICON_SIZE: f32 = 16.0;
/// Stacked-card overlap: past the first slot every card slides back over its
/// predecessor, which is what reads as one cluster rather than a row of loose
/// cards. Children paint in declaration order, so the later pane sits on top -
/// the same direction as the visual reference.
const SIDEBAR_TAB_ICON_OVERLAP: f32 = 11.0;

/// Shared shell of every rail row, folder and tab alike, so a workspace row is
/// exactly as tall as a tab row: same padding, same corner, no minimum height.
/// A workspace only grows past that when it renders a meta line (a detected
/// service), which the `gap` then separates from the title.
fn sidebar_row_shell() -> gpui::Div {
    div()
        .px(px(SIDEBAR_ROW_PADDING_X))
        .py(px(SIDEBAR_ROW_PADDING_Y))
        .flex_none()
        // `relative()` belongs to the shell, not to its callers: the row's
        // fill and its hover action cluster are both absolutely positioned
        // against it.
        .relative()
        .overflow_x_hidden()
        .flex()
        .flex_col()
        .gap(px(SIDEBAR_ROW_GAP))
}

/// A rail row: the shared continuous-corner skin, with `body` on top.
fn squircle_row(
    shell: gpui::Stateful<gpui::Div>,
    group: SharedString,
    resting: Option<gpui::Hsla>,
    hovered: Option<gpui::Hsla>,
    body: impl IntoElement,
) -> gpui::Stateful<gpui::Div> {
    squircle_skin(shell, group, ROW_RADIUS, resting, hovered).child(body)
}

/// US-010: the action cluster a rail row opens under the pointer - absolute,
/// right-aligned, one 20x20 button per action, hidden until `group` is hovered.
///
/// Insets are resolved against this cluster's own parent - the element it is
/// added to - not against the row shell: taffy positions an absolute child
/// relative to its direct parent, it does not walk up to the nearest positioned
/// ancestor the way CSS does. The caller therefore adds it to a box that is
/// already inside the shell's padding, and must not re-apply that padding here.
///
/// The cluster is centered on the title line, not on the row: a row that grew a
/// meta line must not drift its actions down with it. The buttons are 20px on
/// an 18px line, hence the 1px overhang.
///
/// The hover toggle must NOT be `display`. `Div::prepaint` skips its children
/// when the computed style says `display: none` while `Div::paint` paints them,
/// and the two phases can disagree on hover within one frame (the group hitbox
/// is only known after prepaint). Flipping `display` on hover therefore paints
/// never-prepainted children and GPUI panics with "must call prepaint before
/// paint". `visibility` is only consulted in `Interactivity::paint`, so both
/// phases stay consistent - this is Zed's `visible_on_hover` idiom, and it also
/// keeps the hidden cluster from swallowing clicks (mouse listeners are
/// registered after the visibility check).
fn sidebar_hover_actions(group: SharedString) -> gpui::Div {
    div()
        .absolute()
        .top(px(
            (SIDEBAR_ROW_LINE_HEIGHT - SIDEBAR_ACTION_BUTTON_SIZE) / 2.
        ))
        .right(px(0.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(SIDEBAR_ACTION_BUTTON_GAP))
        .invisible()
        .group_hover(group, |style| style.visible())
}

/// One button of a [`sidebar_hover_actions`] cluster. `label` is the button's
/// accessible name and its tooltip (issue #340: one string feeds both, so an
/// action cannot be built without a name). The caller chains its own click
/// handler.
///
/// `svg()` is a mask: it paints nothing without its own `text_color`, and the
/// parent's does NOT cascade - the same trap the sidebar header's `+`
/// documents.
fn sidebar_action_button(
    id: SharedString,
    label: SharedString,
    icon: &'static str,
    icon_size: f32,
    ui: crate::theme::UiColors,
) -> gpui::Stateful<gpui::Div> {
    let tint = ui.muted;
    // The row underneath is already at the hover tint when this button is
    // reachable, so the button hovers into the active tint - one step further,
    // or it would be invisible against its own row.
    let active_bg = crate::app::constants::sidebar_tab_active_background();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .flex_none()
        .size(px(SIDEBAR_ACTION_BUTTON_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_color(tint)
        .hover(move |style| style.bg(active_bg))
        .delayed_tooltip(move |_w, cx| {
            cx.new(|_| SidebarTooltip {
                label: label.clone(),
            })
            .into()
        })
        .child(
            svg()
                .size(px(icon_size))
                .flex_none()
                .path(icon)
                .text_color(tint),
        )
}

impl SidebarAgentSummary {
    fn slot_width(self) -> f32 {
        if self.state == SidebarAgentState::NeedsInput {
            SIDEBAR_AGENT_STATUS_SLOT_WIDTH
        } else if self.count > 1 {
            28.0
        } else {
            SIDEBAR_AGENT_ICON_SLOT_WIDTH
        }
    }

    fn tooltip_state(self) -> String {
        match self.state {
            SidebarAgentState::NeedsInput => {
                agent_status_sentence(self.count, "needs input", "need input")
            }
            SidebarAgentState::Errored => agent_status_sentence(self.count, "errored", "errored"),
            SidebarAgentState::Stalled => agent_status_sentence(self.count, "stalled", "stalled"),
            SidebarAgentState::Thinking => {
                agent_status_sentence(self.count, "thinking", "thinking")
            }
            SidebarAgentState::Finished => {
                "Agent finished · Click workspace or pane to dismiss".to_string()
            }
        }
    }
}

fn agent_status_sentence(count: usize, singular_state: &str, plural_state: &str) -> String {
    if count == 1 {
        format!("1 agent {singular_state}")
    } else {
        format!("{count} agents {plural_state}")
    }
}

impl SidebarRenderTimeCanary {
    fn new(workspace_count: usize) -> Self {
        Self {
            start: std::time::Instant::now(),
            workspace_count,
        }
    }
}

impl Drop for SidebarRenderTimeCanary {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        if elapsed > std::time::Duration::from_millis(16) {
            tracing::debug!(
                target: "paneflow_app::sidebar",
                "render_sidebar exceeded 16ms frame budget: {:.2}ms across {} workspaces",
                elapsed.as_secs_f64() * 1000.0,
                self.workspace_count
            );
        }
    }
}

fn visible_service_ports(
    active_ports: &[u16],
    service_labels: &std::collections::HashMap<u16, crate::terminal::ServiceInfo>,
) -> Vec<u16> {
    active_ports
        .iter()
        .copied()
        .filter(|port| service_labels.contains_key(port))
        .collect()
}

fn sidebar_service_summary(
    active_ports: &[u16],
    service_labels: &std::collections::HashMap<u16, crate::terminal::ServiceInfo>,
) -> Option<SidebarServiceSummary> {
    let visible = visible_service_ports(active_ports, service_labels);
    let primary = visible
        .iter()
        .copied()
        .find(|port| {
            service_labels
                .get(port)
                .is_some_and(|info| info.is_frontend)
        })
        .or_else(|| visible.first().copied())?;
    Some(SidebarServiceSummary {
        primary,
        overflow: visible.len().saturating_sub(1),
    })
}

fn sidebar_agent_summary<'a, I>(sessions: I, completion_unread: bool) -> Option<SidebarAgentSummary>
where
    I: IntoIterator<Item = &'a ai_types::AgentSession>,
{
    let mut counts = [0usize; 4];
    for session in sessions {
        let index = match session.state {
            ai_types::AgentState::WaitingForInput => 0,
            ai_types::AgentState::Errored => 1,
            ai_types::AgentState::Stalled => 2,
            ai_types::AgentState::Thinking => 3,
            ai_types::AgentState::Finished => continue,
        };
        counts[index] += 1;
    }

    let priority = [
        SidebarAgentState::NeedsInput,
        SidebarAgentState::Errored,
        SidebarAgentState::Stalled,
    ];
    for (state, count) in priority.into_iter().zip(counts[..3].iter().copied()) {
        if count > 0 {
            return Some(SidebarAgentSummary { state, count });
        }
    }

    if completion_unread {
        return Some(SidebarAgentSummary {
            state: SidebarAgentState::Finished,
            count: 1,
        });
    }

    (counts[3] > 0).then_some(SidebarAgentSummary {
        state: SidebarAgentState::Thinking,
        count: counts[3],
    })
}

/// US-012: the sessions a workspace *folder* row speaks for.
///
/// Expanded, every session whose `surface_id` resolved is already spoken for
/// by its own tab row, so the folder keeps only the residue: sessions still at
/// `surface_id: None` - old shims, ancestor walks that never landed. That
/// residue is exactly FR-04, an unattributed session belongs to the project
/// and never to an arbitrary tab.
///
/// Collapsed, the tab rows are off screen, so the folder re-aggregates every
/// session again: the fold must hide no state (FR-05).
///
/// The completion notification is deliberately NOT filtered here - it is
/// workspace-scoped and carries no surface, so it stays on the folder row in
/// both states, for the same reason.
fn folder_row_sessions<'a, I>(
    sessions: I,
    expanded: bool,
) -> impl Iterator<Item = &'a ai_types::AgentSession>
where
    I: IntoIterator<Item = &'a ai_types::AgentSession>,
    I::IntoIter: 'a,
{
    sessions
        .into_iter()
        .filter(move |session| !expanded || session.surface_id.is_none())
}

/// US-012: the sessions one tab row speaks for - those whose `surface_id` is a
/// terminal of that tab's pane tree.
///
/// This filter and [`folder_row_sessions`] partition on `surface_id`, so a
/// session that resolves late simply migrates from the folder to its owning
/// tab on the next frame, counted once on either side and never on both.
fn tab_row_sessions<'a, I>(
    sessions: I,
    surfaces: &'a std::collections::HashSet<u64>,
) -> impl Iterator<Item = &'a ai_types::AgentSession>
where
    I: IntoIterator<Item = &'a ai_types::AgentSession>,
    I::IntoIter: 'a,
{
    sessions
        .into_iter()
        .filter(move |session| session.surface_id.is_some_and(|id| surfaces.contains(&id)))
}

/// US-013: how many pane icons a tab row paints, and how many fold into the
/// trailing `+N`.
fn tab_icon_cluster_split(pane_count: usize) -> (usize, usize) {
    let shown = pane_count.min(SIDEBAR_TAB_ICON_CAP);
    (shown, pane_count - shown)
}

/// US-013: what one pane contributes to its tab row's icon cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabPaneIcon {
    path: &'static str,
    label: &'static str,
}

/// US-013: read a pane's icon straight from `terminal.detected_agent`, the
/// PID-authoritative scan result `apply_pane_scan` already deposited. No scan
/// is started here: a pane whose agent is not known yet keeps the generic
/// surface icon and swaps glyph in place once the 500 ms debounce lands.
fn tab_pane_icon(pane: &crate::pane::Pane, cx: &gpui::App) -> TabPaneIcon {
    let agent = pane
        .surface
        .as_terminal()
        .and_then(|terminal| terminal.read(cx).terminal.detected_agent);
    match agent {
        Some(agent) => TabPaneIcon {
            path: agent.icon_path(),
            label: agent.display_name(),
        },
        None => TabPaneIcon {
            path: pane.surface.kind_icon(),
            label: pane.surface.kind_label(),
        },
    }
}

/// US-009: label of a tab row. An unnamed tab (the default until it is renamed
/// or created from a named preset) falls back to its 1-based position, so the
/// list never shows a blank row.
pub(crate) fn tab_display_title(tab: &Tab, tab_idx: usize) -> String {
    if tab.title.trim().is_empty() {
        format!("Tab {}", tab_idx + 1)
    } else {
        tab.title.clone()
    }
}

/// The label a tab row actually paints, derived at render time.
///
/// Precedence: a manual rename wins; failing that the tab borrows its FIRST
/// pane's resolved title; failing that the positional fallback above.
///
/// The middle branch is the whole point. Agents rewrite their terminal title
/// through OSC 0/2 as they work, that lands in `terminal.title`, and
/// `Pane::surface_title` is the one resolver that already ranks a pane's
/// custom name over that OSC title over the agent's display name. Reusing it
/// here is what stops the rail from growing a second, drifting definition of
/// "what is running in there".
///
/// Derived, never stored: nothing is written to `Tab::title` and nothing is
/// saved. Persisting an agent-owned string as if the user had typed it would
/// freeze the label at whatever the agent happened to be doing, and would
/// dirty the session file on every OSC.
pub(crate) fn tab_row_title(tab: &Tab, tab_idx: usize, cx: &gpui::App) -> String {
    if !tab.title.trim().is_empty() {
        return tab.title.clone();
    }
    // `root` and not `saved_layout`: under zoom `root` holds exactly the pane
    // on screen, which is the one whose title the row should be speaking for.
    let pane_title = tab
        .root
        .as_ref()
        .and_then(|root| root.first_leaf())
        .map(|pane| crate::pane::Pane::surface_title(&pane.read(cx).surface, cx))
        .unwrap_or_default();
    if pane_title.trim().is_empty() {
        tab_display_title(tab, tab_idx)
    } else {
        pane_title
    }
}

/// Index a remove-then-insert reorder must target so the moved item lands in
/// the gap at `slot`. Both [`crate::workspace::Workspace::reorder_tab`] and
/// `reorder_workspace` remove first, which slides everything after the source
/// up by one - so a move down aims one index lower than the gap it points at.
fn reorder_target(from: usize, slot: usize) -> usize {
    if from < slot { slot - 1 } else { slot }
}

/// Per-workspace inputs to the sidebar's ordering, snapshotted once per frame.
/// Pure data so both order functions and the cache signature can be unit-tested
/// without a `Workspace` (whose constructors read `.git` off disk).
pub(crate) struct WorkspaceOrderKey {
    /// The user pinned this workspace to the top of the rail.
    pub pinned: bool,
    /// Something is running in it - the inverse of
    /// [`crate::workspace::Workspace::is_idle`].
    pub active: bool,
    /// Lowercased title, so the alphabetical tie-break is case-insensitive.
    pub title_lower: String,
    /// Shared repo root; sibling worktrees carry an identical value and are the
    /// only reason a workspace is not its own group.
    pub repo_root: Option<std::path::PathBuf>,
}

/// Partition storage indices into the blocks the rail moves as a unit: sibling
/// worktrees of one repo (2+ workspaces sharing a `repo_root`) form one group in
/// storage order, everything else is a singleton. A group is emitted at the
/// position of its FIRST member, so the manual order is storage order with the
/// siblings pulled together.
///
/// One definition, shared by Manual ([`PaneFlowApp::compute_display_order`]) and
/// Auto ([`compute_auto_order`]) - the two must agree on what a group is, or a
/// mode switch would regroup the rail as well as reorder it.
fn worktree_groups<'a>(
    roots: impl Iterator<Item = Option<&'a std::path::Path>>,
) -> Vec<Vec<usize>> {
    let roots: Vec<Option<&std::path::Path>> = roots.collect();
    let mut repo_members: std::collections::HashMap<&std::path::Path, Vec<usize>> =
        std::collections::HashMap::new();
    for (index, root) in roots.iter().enumerate() {
        if let Some(root) = root {
            repo_members.entry(root).or_default().push(index);
        }
    }

    let mut groups: Vec<Vec<usize>> = Vec::with_capacity(roots.len());
    let mut placed = vec![false; roots.len()];
    for (index, root) in roots.iter().enumerate() {
        if placed[index] {
            continue;
        }
        if let Some(root) = root
            && let Some(members) = repo_members.get(root)
            && members.len() >= 2
        {
            for &member in members {
                placed[member] = true;
            }
            groups.push(members.clone());
            continue;
        }
        placed[index] = true;
        groups.push(vec![index]);
    }
    groups
}

/// Issue #107, the Auto ordering: pinned first, then active, then inactive,
/// alphabetical within each bucket.
///
/// A sibling-worktree group is sorted as one row (R-D2): it is pinned if ANY
/// member is pinned and active if ANY member is active, and it sorts under the
/// title of its FIRST member in storage order. Bucketing a group by its
/// strongest member is what keeps pinning one worktree from dragging its
/// siblings to the bottom of the rail. Members keep storage order inside their
/// group, and the sort is stable, so equal keys keep storage order too.
fn compute_auto_order(keys: &[WorkspaceOrderKey]) -> Vec<usize> {
    let mut groups = worktree_groups(keys.iter().map(|key| key.repo_root.as_deref()));
    groups.sort_by(|a, b| {
        let rank = |group: &Vec<usize>| {
            (
                group.iter().any(|&i| keys[i].pinned),
                group.iter().any(|&i| keys[i].active),
            )
        };
        let (a_pinned, a_active) = rank(a);
        let (b_pinned, b_active) = rank(b);
        // `false < true`, so descending on the two flags is `b.cmp(a)`.
        b_pinned
            .cmp(&a_pinned)
            .then_with(|| b_active.cmp(&a_active))
            .then_with(|| keys[a[0]].title_lower.cmp(&keys[b[0]].title_lower))
    });
    groups.into_iter().flatten().collect()
}

/// The insertion points of a rendered rail: one before every row, one after
/// the last, so `rows.len() + 1` in all.
///
/// A gap inherits its tab target from the row *above* it, which is the only
/// reading that matches what the eye sees: the line above a folder row cannot
/// mean "first tab of that folder" (its tabs render below it), it means "last
/// tab of the workspace the line is under".
fn sidebar_drop_slots(
    rows: &[SidebarRow],
    workspace_count: usize,
    auto_sort: bool,
) -> Vec<SidebarDropSlot> {
    (0..=rows.len())
        .map(|k| SidebarDropSlot {
            tab: match k.checked_sub(1).map(|above| rows[above]) {
                Some(SidebarRow::Folder(ws)) => Some((ws, 0)),
                Some(SidebarRow::Tab(ws, tab)) => Some((ws, tab + 1)),
                None => None,
            },
            // Issue #107: no folder drop target under Auto. `workspace` is a
            // STORAGE index fed straight to a remove-then-insert reorder, which
            // only means what it says while display order == storage order.
            // Under Auto it would move a different workspace than the one the
            // line sits beside, and the sort would undo the move anyway.
            workspace: match rows.get(k) {
                _ if auto_sort => None,
                Some(SidebarRow::Folder(ws)) => Some(*ws),
                // Past the last row: a folder dropped here lands at the end.
                None => Some(workspace_count),
                Some(SidebarRow::Tab(..)) => None,
            },
        })
        .collect()
}

/// What one keystroke means to the sidebar's inline rename editor.
///
/// Issue #79: both row shells - workspace folders and their tab children -
/// used to inline the same `match` over `keystroke.key`, and neither was
/// reachable. Factoring the decision out gives the two handlers one shared
/// definition and makes it testable without a live window; each handler still
/// owns its own mutation, so this stays pure over its inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenameKey {
    /// `enter`: settle the edit through `commit_rename`.
    Commit,
    /// `escape`: drop the edit and leave the title as it was.
    Cancel,
    /// `backspace`: delete the last character of `rename_text`.
    Backspace,
    /// A printable character to append to `rename_text`.
    Insert(String),
    /// Nothing the editor acts on - let it through untouched.
    Ignore,
}

/// Map a keystroke onto the editor's response.
///
/// `alt` suppresses insertion alongside `control` and `platform` (mirroring
/// the broadcast picker): on macOS Option+key composes a dead key, so without
/// this guard Option+E types a combining acute into the name instead of being
/// ignored. `shift` is deliberately not in that set - it is how a capital
/// arrives.
pub(crate) fn rename_key_action(
    key: &str,
    key_char: Option<&str>,
    mods: gpui::Modifiers,
) -> RenameKey {
    match key {
        "enter" => RenameKey::Commit,
        "escape" => RenameKey::Cancel,
        "backspace" => RenameKey::Backspace,
        _ => match key_char {
            Some(ch) if !ch.is_empty() && !mods.control && !mods.platform && !mods.alt => {
                RenameKey::Insert(ch.to_string())
            }
            _ => RenameKey::Ignore,
        },
    }
}

impl PaneFlowApp {
    /// Hand focus back when an inline rename ends.
    ///
    /// Issue #79 meets issue #108: the renamed row is the only element that
    /// tracks `sidebar_rename_focus`, and it stops tracking it the instant the
    /// rename state clears. Without this the window ends every rename with
    /// nothing focused - the dispatch path collapses to the tree root, and
    /// every global `context: None` binding matches but finds no handler.
    /// Mirrors `close_attention_queue_and_restore_focus`.
    pub(crate) fn restore_focus_after_rename(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = match self.workspaces.get(self.active_idx) {
            Some(ws) => ws.focus_first(window, cx),
            None => false,
        };
        if !focused {
            window.focus(&self.empty_workspace_focus, cx);
        }
    }

    /// Commit a live inline rename and hand focus back, in one step.
    ///
    /// `commit_rename` on its own became a focus-stranding path the moment a
    /// row started tracking focus (issue #79): it clears `renaming_idx` /
    /// `renaming_tab`, the renamed row stops tracking `sidebar_rename_focus`
    /// on the very next frame, and unless the caller focuses something else
    /// the window is left with nothing focused (issue #108). Callers that go
    /// straight on to focus a pane themselves - `select_workspace_tab`,
    /// `activate_workspace_at` - do not need this. The ones that open a
    /// context menu do: `select_menu` tracks no focus handle, so the menu
    /// cannot take the focus the row just gave up.
    ///
    /// A no-op when no rename is live, so an ordinary right-click does not
    /// move focus.
    fn commit_inline_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_renaming = self.renaming_idx.is_some() || self.renaming_tab.is_some();
        self.commit_rename(cx);
        if was_renaming {
            self.restore_focus_after_rename(window, cx);
        }
    }

    /// Start the inline rename of a workspace folder row.
    ///
    /// Issue #79: this takes a `Window` so it can focus. Setting `renaming_idx`
    /// only draws the editor; the row's `on_key_down` still receives nothing
    /// until the row is on the dispatch path, which it is only while it tracks
    /// `sidebar_rename_focus`. The focus claim has to happen here, at the end,
    /// rather than in the caller: the first click of a double-click already ran
    /// the single-click branch, which selected the workspace and focused one of
    /// its terminals. Claiming focus last is what survives that.
    fn begin_workspace_rename(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_rename(cx);
        if let Some(title) = self
            .workspaces
            .get(index)
            .map(|workspace| workspace.title.clone())
        {
            self.rename_text = title;
            // Seeded, so the editor opens with the whole existing name
            // selected. Set after `commit_rename`, which clears the flag.
            self.rename_seeded = true;
            self.renaming_idx = Some(index);
            self.sidebar_rename_focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Cache key for the memoized rail order. It has to fold EVERY input the
    /// two order functions read, or the rail paints a stale order forever: the
    /// cache is only ever consulted, never invalidated by a mutation.
    ///
    /// `Workspace::id` is deliberately absent. Neither
    /// [`Self::compute_display_order`] nor [`compute_auto_order`] reads it, and
    /// the hash is position-sensitive, so two workspaces that swap places
    /// either differ in a key field (new hash) or are interchangeable to both
    /// functions (same order either way).
    fn sidebar_order_signature(keys: &[WorkspaceOrderKey], auto_sort: bool) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        auto_sort.hash(&mut hasher);
        keys.len().hash(&mut hasher);
        for key in keys {
            key.pinned.hash(&mut hasher);
            key.active.hash(&mut hasher);
            key.title_lower.hash(&mut hasher);
            match &key.repo_root {
                Some(root) => root.hash(&mut hasher),
                None => 0u8.hash(&mut hasher),
            }
        }
        hasher.finish()
    }

    /// Manual ordering: storage order, with sibling worktrees pulled
    /// contiguous. Unchanged - Auto is opt-in and this is what the rail does
    /// without it.
    fn compute_display_order(workspaces: &[Workspace]) -> Vec<usize> {
        worktree_groups(workspaces.iter().map(|ws| ws.repo_root.as_deref()))
            .into_iter()
            .flatten()
            .collect()
    }

    /// Snapshot the per-workspace inputs both order functions and the cache
    /// signature read. One walk per frame; every field is already resident
    /// (`is_idle` reads the pane scanner's cache), so this does no I/O.
    fn workspace_order_keys(&self, cx: &App) -> Vec<WorkspaceOrderKey> {
        self.workspaces
            .iter()
            .map(|ws| WorkspaceOrderKey {
                pinned: ws.pinned,
                active: !ws.is_idle(cx),
                title_lower: ws.title.to_lowercase(),
                repo_root: ws.repo_root.clone(),
            })
            .collect()
    }

    /// Storage indices in the same order as the workspace folder rows in the
    /// rail. Keyboard navigation uses this projection too, so "workspace N"
    /// has one meaning even when grouping or Auto sorting diverges from
    /// storage order.
    pub(crate) fn workspace_display_order(&self, cx: &App) -> Vec<usize> {
        let auto_sort = self.cached_config.workspace_auto_sort_enabled();
        let keys = self.workspace_order_keys(cx);
        let signature = Self::sidebar_order_signature(&keys, auto_sort);
        if self.sidebar_order_cache.borrow().signature != Some(signature) {
            let order = if auto_sort {
                compute_auto_order(&keys)
            } else {
                Self::compute_display_order(&self.workspaces)
            };
            let mut cache = self.sidebar_order_cache.borrow_mut();
            cache.order = order;
            cache.signature = Some(signature);
        }
        self.sidebar_order_cache.borrow().order.clone()
    }

    pub(crate) fn render_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let _render_canary = SidebarRenderTimeCanary::new(self.workspaces.len());
        let ui = crate::theme::ui_colors();
        let theme = crate::theme::active_theme();
        let mut sidebar = div()
            .relative()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .h_full()
            // Cockpit rail (#141414). The
            // border-right is gone: the rail and the #181818 content gutter
            // separate by a luminance step, not a drawn divider (the OpenAI
            // surface system - separation by luminance, not borders).
            .bg(crate::app::constants::cockpit_chrome_background(
                theme.title_bar_background,
                window.is_window_active(),
                self.cached_config.cockpit_chrome_material_enabled(),
            ))
            .flex()
            .flex_col();

        sidebar = sidebar.child(
            div()
                // Tight enough that the header reads as the list's first line
                // rather than a floating band: at 48 the header label sat 43 px
                // from the first row's label while consecutive rows sit 34
                // apart.
                .h(px(36.))
                .flex_none()
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .pl(px(8.))
                        .text_size(px(13.))
                        .text_color(ui.muted)
                        .child("Workspaces"),
                ),
        );

        // Workspace list - scrollable area. Wheel-scroll comes from
        // `overflow_y_scroll + track_scroll`; the visible scroll bar
        // is gone, so the list uses the full sidebar width without a
        // trailing gutter.
        let mut list = div()
            .id("workspace-list")
            .flex_1()
            .min_w_0()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .track_scroll(&self.sidebar_scroll)
            .flex()
            .flex_col()
            // No gap and no top padding: the drop dividers are the gaps, and
            // the leading one is the list's top padding.
            .pb(px(4.));

        if self.workspaces.is_empty() {
            list = list.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(ui.muted)
                            .child("Open a project folder"),
                    )
                    .child({
                        let hover_bg = crate::app::constants::sidebar_tab_active_background();
                        div()
                            .id("empty-new-ws")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(6.))
                            .bg(ui.subtle)
                            .text_color(ui.text)
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .hover(move |style| style.bg(hover_bg))
                            .on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                                this.create_workspace_with_picker(w, cx);
                            }))
                            .child(
                                svg()
                                    .size(px(12.))
                                    .flex_none()
                                    .path("icons/folder_open.svg")
                                    .text_color(ui.muted),
                            )
                            .child("Open folder")
                    }),
            );
        }

        list = self.render_workspace_rows(list, ui, cx);
        sidebar = sidebar.child(self.sidebar_list_wrapper(list, cx));
        sidebar = sidebar.child(self.render_sidebar_settings_footer(cx));
        sidebar
    }

    /// Flatten the rail into the rows it renders. Built once per frame because
    /// the drop dividers are defined by the gaps between these rows.
    fn sidebar_rows(&self, cx: &App) -> Vec<SidebarRow> {
        let order = self.workspace_display_order(cx);
        let mut rows = Vec::with_capacity(order.len());
        for i in order {
            rows.push(SidebarRow::Folder(i));
            // US-009: the tabs of an expanded workspace follow their folder row
            // as sibling children of the scrolling list, so a long tab list
            // scrolls with everything else instead of squeezing the rows above
            // it (`sidebar_workspace_rows_keep_height_when_list_overflows`).
            // An empty workspace shows no child row at all: its single tab is
            // the FR-01 placeholder, not something the user created, and
            // `open_tab` fills it in place. The folder simply reads as empty.
            if self.workspaces[i].sidebar_expanded && !self.workspaces[i].is_empty_shell() {
                for tab_idx in 0..self.workspaces[i].tab_count() {
                    rows.push(SidebarRow::Tab(i, tab_idx));
                }
            }
        }
        rows
    }

    fn render_workspace_rows(
        &self,
        mut list: gpui::Stateful<gpui::Div>,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let auto_sort = self.cached_config.workspace_auto_sort_enabled();
        let rows = self.sidebar_rows(cx);
        let slots = sidebar_drop_slots(&rows, self.workspaces.len(), auto_sort);
        for (k, row) in rows.iter().enumerate() {
            list = list.child(self.render_drop_divider(k, slots[k], ui, cx));
            list = list.child(match *row {
                SidebarRow::Folder(i) => self
                    .render_workspace_row(i, ui, auto_sort, cx)
                    .into_any_element(),
                SidebarRow::Tab(i, tab_idx) => {
                    self.render_tab_row(i, tab_idx, ui, cx).into_any_element()
                }
            });
        }
        if let Some(&trailing) = slots.last() {
            list = list.child(self.render_drop_divider(rows.len(), trailing, ui, cx));
        }
        list
    }

    /// The gap between two rows, rendered as a real element: an invisible drop
    /// band spanning half a row on either side, holding the insertion line it
    /// reveals while a matching drag hovers it.
    ///
    /// The band is absolutely positioned, so it reaches over its neighbors
    /// without displacing them, and it carries no click listener - a plain
    /// (`HitboxBehavior::Normal`) hitbox never occludes the rows underneath, so
    /// they stay clickable through it. The line itself is a child styled by
    /// `group_drag_over`, which is what keeps the visible mark 2 px thin while
    /// the target stays a comfortable 34 px tall.
    fn render_drop_divider(
        &self,
        key: usize,
        slot: SidebarDropSlot,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Neutral, like the pane placeholder in the grid: the rail already
        // spends its accent on the selected row, and a colored drop line
        // competed with it.
        let color = ui.text.opacity(0.5);
        let group = SharedString::from(format!("drop-slot-{key}"));
        let mut band = div()
            .id(SharedString::from(format!("drop-band-{key}")))
            .group(group.clone())
            .absolute()
            .top(px(-SIDEBAR_DROP_BAND_REACH))
            // Width, not a `left`/`right` inset pair: an absolutely positioned
            // element sized by its insets alone measures 0 px wide here, which
            // leaves both the line and its hitbox invisible.
            .w_full()
            .px(px(SIDEBAR_ROW_MARGIN_X))
            .h(px(SIDEBAR_ROW_SPACING + SIDEBAR_DROP_BAND_REACH * 2.0))
            .flex()
            .flex_col()
            .justify_center();
        // The line reads the band's hover state through `group_drag_over`, and
        // that lookup only runs for an element owning a hitbox: the whole
        // drag-over branch of GPUI's `compute_style_internal` sits behind
        // `if let Some(hitbox)`, while `should_insert_hitbox` counts
        // `drag_over_styles` but *not* `group_drag_over_styles`. Declaring the
        // line a group of its own is what earns it that hitbox - without it the
        // line is laid out and painted, yet never styled, so nothing shows.
        let mut line = div()
            .group(SharedString::from(format!("drop-line-{key}")))
            .h(px(SIDEBAR_DROP_LINE_PX))
            .w_full()
            .rounded_full();

        if let Some((ws_idx, tab_idx)) = slot.tab {
            let ws_id = self.workspaces.get(ws_idx).map(|ws| ws.id);
            band = band
                // US-011: a tab dropped in a gap of its own workspace reorders;
                // dropped in another workspace's gap it reattaches there,
                // keeping its pane tree and its live terminals.
                .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                    if ws_id == Some(drag.workspace_id) {
                        let Some(from) = this
                            .workspaces
                            .get(ws_idx)
                            .and_then(|ws| ws.tabs().iter().position(|tab| tab.id == drag.tab_id))
                        else {
                            return;
                        };
                        this.reorder_workspace_tab(drag, ws_idx, reorder_target(from, tab_idx), cx);
                    } else {
                        this.move_tab_to_workspace(drag, ws_idx, tab_idx, window, cx);
                    }
                }))
                // A pane dragged out of the grid leaves its current tab and
                // becomes a tab of this workspace, terminal still running.
                // Legal on the pane's *own* workspace too: the gesture is "give
                // this pane a tab of its own", not "reattach elsewhere".
                .on_drop(cx.listener(move |this, drag: &PaneDrag, window, cx| {
                    this.move_pane_to_new_tab(drag.pane_id, ws_idx, tab_idx, window, cx);
                }));
            line = line
                .group_drag_over::<TabDrag>(group.clone(), move |style| style.bg(color))
                .group_drag_over::<PaneDrag>(group.clone(), move |style| style.bg(color));
        }

        if let Some(ws_slot) = slot.workspace {
            band = band.on_drop(cx.listener(move |this, drag: &WorkspaceDrag, _window, cx| {
                // Re-resolve the source by id: the rail re-renders during the
                // drag, so the index captured when it started can be stale.
                let Some(from) = this.workspaces.iter().position(|ws| ws.id == drag.id) else {
                    return;
                };
                this.reorder_workspace(drag.id, reorder_target(from, ws_slot), cx);
            }));
            line =
                line.group_drag_over::<WorkspaceDrag>(group.clone(), move |style| style.bg(color));
        }

        div()
            .h(px(SIDEBAR_ROW_SPACING))
            .flex_none()
            .relative()
            .child(band.child(line))
    }

    fn render_workspace_row(
        &self,
        i: usize,
        ui: crate::theme::UiColors,
        auto_sort: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ws = &self.workspaces[i];

        let title = ws.title.clone();

        let idx = i;
        let ws_id = ws.id;
        let ws_title: SharedString = ws.title.clone().into();
        // Two distinct tints, not one: the row lifts by the hover step, and
        // its own actions hover one step further into the active tint (see
        // `sidebar_action_button`). Sharing a single tint left those actions
        // invisible against the row they sit on.
        let hover_bg = crate::app::constants::sidebar_tab_hover_background();
        // US-008 / US-010: one hover group per folder row - the trailing agent
        // badge fades out and the create-tab action fades in together.
        let group_name = SharedString::from(format!("ws-row-{ws_id}"));
        let is_expanded = ws.sidebar_expanded;

        let row_shell = sidebar_row_shell()
            .id(SharedString::from(format!("ws-{ws_id}")))
            .group(group_name.clone())
            // Issue #107: no folder drag under Auto - the sort owns the order,
            // so a drag could only ever snap back. Tab drags are unaffected.
            .when(!auto_sort, |shell| {
                shell.on_drag(
                    WorkspaceDrag {
                        id: ws_id,
                        title: ws_title.clone(),
                    },
                    |drag, _offset, _window, cx| {
                        cx.new(|_| WorkspaceDragPreview {
                            title: drag.title.clone(),
                        })
                    },
                )
            })
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                if let Some(workspace) = this.workspaces.get_mut(idx) {
                    workspace.agent_completion_notification.acknowledge();
                }
                let is_double = matches!(e, ClickEvent::Mouse(m) if m.down.click_count == 2);
                // The dismiss cannot run before the single-vs-double decision:
                // ahead of the single-click branch it would sit between the
                // `was_renaming` read and the commit, and the click that is
                // meant to end an edit would throw the typed name away. Each
                // branch dismisses at the point that is correct for it.
                // `begin_workspace_rename` commits any rename already live
                // before it seeds its own, so the double-click branch needs no
                // rename handling of its own.
                if is_double {
                    this.dismiss_transient_surfaces();
                    this.begin_workspace_rename(idx, window, cx);
                } else {
                    let was_renaming = this.renaming_idx == Some(idx);
                    this.commit_rename(cx);
                    this.dismiss_transient_surfaces();
                    // Issue #78: a single click on the row is a workspace-level
                    // gesture, so it lands on the pane whose agent is waiting
                    // for input (the one the row's "Input" badge advertises)
                    // rather than the first pane in layout order.
                    this.activate_workspace_at(
                        idx,
                        WorkspaceFocusTarget::WaitingElseFirst,
                        window,
                        cx,
                    );
                    // US-008: the whole card is the disclosure control, not just
                    // the folder icon. Committing a rename is exempt: that click
                    // ends an edit, and folding the row under the cursor would
                    // read as a side effect of typing a name.
                    if !was_renaming {
                        this.toggle_workspace_expanded(idx, cx);
                    }
                }
                cx.notify();
            }))
            .on_aux_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                if e.is_right_click()
                    && let Some(position) = e.mouse_position()
                {
                    // Not a bare `commit_rename`: that clears `renaming_idx`,
                    // this row stops tracking `sidebar_rename_focus`, and the
                    // context menu about to open tracks no focus handle of its
                    // own - so nothing would be focused (issue #108).
                    this.commit_inline_rename(window, cx);
                    this.dismiss_transient_surfaces();
                    this.workspace_menu_open = Some(WorkspaceContextMenu { idx, position });
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(move |this, e: &KeyDownEvent, window, cx| {
                // M1: this `f2` branch is structurally unreachable, and is
                // kept only so the handler reads as a whole. A row is on
                // GPUI's dispatch path only while it tracks
                // `sidebar_rename_focus`, and it tracks it only while its own
                // rename is ALREADY live - precisely the case this branch
                // excludes. Nothing to fix here: making `f2` start a rename
                // needs a focus handle per row, not a change to this branch.
                if this.renaming_idx != Some(idx) {
                    if e.keystroke.key.as_str() == "f2" {
                        this.begin_workspace_rename(idx, window, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                // Issue #79: stop every key the editor consumes. Escape in
                // particular has container handlers above this row, and a
                // cancelled rename must not also read as "dismiss that too".
                match rename_key_action(
                    e.keystroke.key.as_str(),
                    e.keystroke.key_char.as_deref(),
                    e.keystroke.modifiers,
                ) {
                    RenameKey::Commit => {
                        this.commit_rename(cx);
                        this.restore_focus_after_rename(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Cancel => {
                        this.renaming_idx = None;
                        this.rename_text.clear();
                        this.rename_seeded = false;
                        this.restore_focus_after_rename(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Backspace => {
                        if !take_rename_selection(&mut this.rename_text, &mut this.rename_seeded) {
                            this.rename_text.pop();
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Insert(ch) => {
                        take_rename_selection(&mut this.rename_text, &mut this.rename_seeded);
                        this.rename_text.push_str(&ch);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Ignore => {}
                }
            }));

        // Issue #79: only the row actually being renamed claims the handle.
        // One handle is enough - `renaming_idx` and `renaming_tab` share
        // `rename_text`, so at most one editor is live - but every row taking
        // it would leave many elements claiming one handle in a single frame.
        let row_shell = if self.renaming_idx == Some(idx) {
            row_shell.track_focus(&self.sidebar_rename_focus)
        } else {
            row_shell
        };

        // Row 1: title
        //
        // US-012: expanded, the folder only speaks for what no tab can claim;
        // collapsed, it speaks for everything again. The tooltip reads the very
        // same set, or an expanded folder would enumerate tools its badge is no
        // longer counting.
        let folder_sessions = || folder_row_sessions(ws.agent_sessions.values(), is_expanded);
        let agent_status = ai_types::workspace_agent_status(folder_sessions(), &ws.detected_agents);
        let row_agent_status = sidebar_agent_summary(
            folder_sessions(),
            ws.agent_completion_notification.is_unread(),
        );
        // Issue #76: a waiting hook is authoritative even if the process scan
        // has not caught up yet. Use the whole workspace here rather than the
        // folder badge's filtered session set: when the folder is expanded,
        // an attributed waiting session moves to its tab badge but must still
        // prevent a contradictory dim on the folder title.
        let is_waiting_for_input = ws
            .agent_sessions
            .values()
            .any(|session| session.state == ai_types::AgentState::WaitingForInput);
        let tone =
            sidebar_workspace_tone(i == self.active_idx, ws.is_idle(cx), is_waiting_for_input);
        let title_el = if self.renaming_idx == Some(i) {
            let (editor_bg, editor_body) =
                rename_editor_skin(&self.rename_text, self.rename_seeded, ui);
            div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .text_color(ui.text.opacity(tone.title_opacity))
                .text_sm()
                .line_height(px(SIDEBAR_ROW_LINE_HEIGHT))
                .font_weight(FontWeight::MEDIUM)
                .bg(editor_bg)
                .px_1()
                .rounded_sm()
                .child(editor_body)
        } else {
            div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(ui.text.opacity(tone.title_opacity))
                .text_sm()
                .line_height(px(SIDEBAR_ROW_LINE_HEIGHT))
                .font_weight(FontWeight::MEDIUM)
                .child(title)
        };

        // US-008: the open/closed folder glyph reports the disclosure state -
        // no chevron beside it, and no click target of its own. The whole card
        // is the disclosure affordance (see the row's click handler), so the
        // icon is a pure indicator; giving it a private handler would have made
        // its 14px square the one spot on the row that toggles without also
        // selecting the workspace.
        let folder_path = if is_expanded {
            "icons/folder-open.svg"
        } else {
            "icons/folder.svg"
        };
        let disclosure = div()
            .flex_none()
            .size(px(SIDEBAR_FOLDER_ICON_WIDTH))
            .flex()
            .items_center()
            .justify_center()
            .child(
                svg()
                    .size(px(SIDEBAR_FOLDER_ICON_WIDTH))
                    .flex_none()
                    .path(folder_path)
                    .text_color(ui.muted),
            );

        let mut title_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_TITLE_ROW_GAP))
            .w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
            .max_w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
            .min_w_0()
            .overflow_x_hidden()
            // The lane is reserved at all times, not opened on hover: a hover
            // padding only shifts what can still shrink, so a long name ended
            // up under the `+` anyway.
            .pr(px(SIDEBAR_ACTION_LANE_WIDTH))
            .child(disclosure)
            .child(title_el);
        // Issue #107: the pin's only visible state. A text glyph, not an SVG -
        // Paneflow ships no pin asset - and no hover outline when unpinned: the
        // right-click menu owns the toggle, and the action lane
        // (`SIDEBAR_ACTION_LANE_WIDTH`) is sized for exactly two buttons.
        if ws.pinned {
            title_row = title_row.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(ui.accent)
                    .child("★"),
            );
        }
        if let Some(row_agent_status) = row_agent_status {
            let status_tooltip = sidebar_agent_status_tooltip(row_agent_status, &agent_status);
            title_row = title_row.child(render_workspace_agent_summary(
                row_agent_status,
                &format!("ws-{ws_id}"),
                status_tooltip,
                ui,
            ));
        }

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(SIDEBAR_ROW_GAP))
            .child(title_row);

        if let Some(meta_row) = self.render_workspace_meta_row(ws, tone.meta_opacity, ui, cx) {
            body = body.child(meta_row);
        }

        // US-010: hover action cluster on the folder row, on the Agents
        // `hover_actions_cluster` patron - absolute, right-aligned, 20x20
        // buttons, hidden until the row is hovered. The title row reserves the
        // matching lane (`SIDEBAR_ACTION_LANE_WIDTH`), so the cluster never
        // covers the agent badge.
        //
        // The `+` opens the « New pane » preset palette, which covers the
        // shell, the agents, and the workspace's custom commands.
        //
        // The `x` closes the whole folder. Issue #111 routes it through the
        // workspace-wide guard because dropping the `Workspace` also drops
        // every tab, pane, terminal, and live agent it contains.
        body = body.child(
            sidebar_hover_actions(group_name.clone())
                .child(
                    sidebar_action_button(
                        SharedString::from(format!("ws-new-tab-{ws_id}")),
                        SharedString::from(format!("New pane in {ws_title}")),
                        "icons/plus.svg",
                        12.,
                        ui,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            this.open_pane_palette(idx, window, cx);
                            cx.stop_propagation();
                        },
                    )),
                )
                .child(
                    sidebar_action_button(
                        SharedString::from(format!("ws-close-{ws_id}")),
                        SharedString::from(format!("Close {ws_title}")),
                        "icons/close.svg",
                        12.,
                        ui,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, window, cx| {
                            // Re-resolve by id, never by the captured index: rows
                            // reorder under a drag and a stale position would close
                            // the wrong folder.
                            if let Some(at) = this.workspaces.iter().position(|ws| ws.id == ws_id) {
                                this.commit_rename(cx);
                                this.request_close_workspace(
                                    at,
                                    crate::app::close_guard::ConfirmStyle::Modal,
                                    window,
                                    cx,
                                );
                            }
                            cx.stop_propagation();
                        },
                    )),
                ),
        );

        // Pure hover affordance: selecting a workspace never leaves the row
        // filled. The folder is a container, not a leaf - the selected tab
        // underneath is the one that rests filled, and a second filled block
        // above it read as two selections at once.
        let row = squircle_row(row_shell, group_name.clone(), None, Some(hover_bg), body);

        div()
            .id(SharedString::from(format!("ws-drop-{ws_id}")))
            .mx(px(SIDEBAR_ROW_MARGIN_X))
            .flex_none()
            .flex()
            .flex_col()
            .rounded(ROW_RADIUS)
            .child(row)
    }

    /// US-009 / US-010 / US-011: one tab rendered as a child row of its
    /// workspace folder, on the Agents `thread_row` patron - a leading
    /// invisible placeholder carries the indent and there is deliberately no
    /// per-tab icon.
    fn render_tab_row(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ws = &self.workspaces[ws_idx];
        let ws_id = ws.id;
        let tab = &ws.tabs()[tab_idx];
        let tab_id = tab.id;
        let title = tab_row_title(tab, tab_idx, cx);
        let is_active_tab = tab_idx == ws.active_tab_idx();
        let is_active_workspace = ws_idx == self.active_idx;
        let is_renaming = self.renaming_tab == Some((ws_idx, tab_idx));

        // Per-tab activity (US-012) and per-pane identity (US-013) are read in
        // one walk of the tab's leaves: `AgentSession::surface_id` holds a
        // terminal entity id, so a tab's sessions are exactly the workspace
        // sessions whose surface lives in one of that tab's panes, and the
        // cluster is that same leaf order.
        let panes = tab.collect_panes();
        let mut surfaces: std::collections::HashSet<u64> =
            std::collections::HashSet::with_capacity(panes.len());
        let mut pane_icons: Vec<TabPaneIcon> = Vec::with_capacity(panes.len());
        // US-012: the tab's own detected-agent set. The tooltip must name the
        // tools of THIS tab's panes: handed `Workspace::detected_agents` it
        // would append "Codex running" to a claude-only tab because a sibling
        // tab runs codex, which is exactly the misattribution this epic
        // removes. `apply_pane_scan` writes the workspace set and each
        // terminal's `detected_agent` from the same scan, so this is that set
        // restricted to the tab, not a second source of truth.
        let mut tab_agents: std::collections::HashSet<String> = std::collections::HashSet::new();
        for pane in &panes {
            let pane = pane.read(cx);
            for terminal in pane.terminals() {
                surfaces.insert(terminal.entity_id().as_u64());
                if let Some(agent) = terminal.read(cx).terminal.detected_agent {
                    tab_agents.insert(agent.binary().to_string());
                }
            }
            pane_icons.push(tab_pane_icon(pane, cx));
        }
        // The split picker holds a slot this tab is about to fill, but nothing
        // is split until a preset is picked - so the tab still counts one pane
        // and its icon would render bare. Card it early: the row then already
        // reads as the stack the choice is about to produce, and the pane the
        // new one lands next to is named rather than left ambiguous.
        let pending_split_pane = match self.pane_palette.as_ref().map(|p| &p.placement) {
            Some(PalettePlacement::Split { target, .. }) => {
                let target = target.entity_id();
                panes.iter().any(|pane| pane.entity_id() == target)
            }
            _ => false,
        };
        let tab_sessions = || tab_row_sessions(ws.agent_sessions.values(), &surfaces);
        let row_agent_status = sidebar_agent_summary(tab_sessions(), false);
        let agent_status = ai_types::workspace_agent_status(tab_sessions(), &tab_agents);
        // One tint for both states, deliberately: the selected row rests at the
        // very fill a hovered row lifts to, so moving the pointer across the
        // rail never produces a block heavier than the selection it is passing
        // over. `sidebar_tab_active_background` is the stronger step, and it
        // stays reserved for what sits *on* a filled row - the hover action
        // buttons - which needs one step further to read at all.
        let hover_bg = crate::app::constants::sidebar_tab_hover_background();
        // Leaf-only selection, on the Agents `thread_row` grammar: exactly one
        // row in the whole rail rests filled - the visible tab of the visible
        // workspace. The visible tab of another workspace stays flat and is
        // marked by its title color alone (US-009 AC3), so an expanded rail
        // reads as a tree instead of a stack of gray blocks.
        let (resting_bg, hovered_bg) = if is_active_tab && is_active_workspace {
            (Some(hover_bg), None)
        } else {
            (None, Some(hover_bg))
        };
        // Two rules meet on this line, and they are NOT the same rule.
        //
        // Dimming by *tab* stays retired: inside the workspace on screen every
        // title carries the same weight, selected or not. The resting fill is
        // what marks the visible tab (that is what replaced US-009 AC3), and a
        // muted title on top of that fill read as a disabled row rather than
        // an unselected one.
        //
        // Dimming by *workspace* is a different question and survives: a tab
        // of a workspace that is not on screen is not competing with its
        // siblings for the eye, it is background, and at full strength an
        // expanded rail reads as one flat list of equals. It takes the same
        // quiet step an idle workspace title takes, so the rail has one dim.
        //
        // Foreground only, deliberately: the resting/hover fills below stay
        // untouched, or a background workspace's rows would also stop looking
        // clickable.
        let text_color = ui
            .text
            .opacity(sidebar_tab_title_opacity(is_active_workspace));

        let title_el = if is_renaming {
            let (editor_bg, editor_body) =
                rename_editor_skin(&self.rename_text, self.rename_seeded, ui);
            div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .text_color(ui.text)
                .text_sm()
                .line_height(px(SIDEBAR_ROW_LINE_HEIGHT))
                .bg(editor_bg)
                .px_1()
                .rounded_sm()
                .child(editor_body)
        } else {
            div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(text_color)
                .text_sm()
                .line_height(px(SIDEBAR_ROW_LINE_HEIGHT))
                .child(title.clone())
        };

        // US-012: the activity badge leads the title, in the folder-icon slot.
        // Without a running agent the same slot stays empty, so the flex gap
        // lands the tab title on the same X as the workspace title above it -
        // no extra indent, no per-tab icon, the folder glyph alone marks the
        // level.
        let leading_slot = match row_agent_status {
            Some(status) => render_tab_agent_summary(
                status,
                &format!("tab-{tab_id}"),
                sidebar_agent_status_tooltip(status, &agent_status),
                ui,
            ),
            None => div()
                .flex_none()
                .w(px(SIDEBAR_FOLDER_ICON_WIDTH))
                .into_any_element(),
        };

        // No `overflow_x_hidden()` here, deliberately. GPUI's `overflow_mask`
        // builds a mask as soon as *either* axis is hidden, and on the
        // x-hidden/y-visible arm that mask still clamps Y to the element's own
        // bounds - 18px here, the title's line height. The pane cards are
        // taller than that by design (they overhang into the row's vertical
        // padding), so a mask on this row sliced their top and bottom off and
        // they painted squashed. The row cannot overflow horizontally anyway:
        // its width is pinned, every child but the title is `flex_none`, and
        // the title carries its own `overflow_x_hidden` + `text_ellipsis`.
        let mut title_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(SIDEBAR_TITLE_ROW_GAP))
            .w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
            .max_w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
            .min_w_0()
            .child(leading_slot)
            .child(title_el);
        let tab_group = SharedString::from(format!("tab-row-group-{tab_id}"));
        // US-013: the cluster owns the trailing lane - the title is the only
        // thing that shrinks, so the icons are never pushed out of the row.
        //
        // The lane is shared with the row's close action, by swap rather than
        // by reservation: the pane cards state what the tab holds at rest, the
        // `x` takes their place under the pointer. A folder row can afford to
        // hold its lane open at all times (its trailing badge is narrow), but a
        // tab row's cluster is up to four cards wide - reserving room next to
        // it would eat the title on every row to serve a hover-only control.
        // An empty tab has no cards, so it reserves the button's own width
        // instead, or the `x` would land on the title's tail.
        match render_tab_pane_icons(
            &pane_icons,
            &format!("tab-{tab_id}"),
            pending_split_pane,
            ui,
        ) {
            Some(cluster) => {
                title_row = title_row.child(
                    div()
                        .flex_none()
                        // Hiding by `visibility`, not by `display`, for the
                        // reason `sidebar_hover_actions` documents: the two
                        // element phases must agree within the frame the hover
                        // flips. It also stops the hidden lane from holding its
                        // tooltip open under the close button.
                        .group_hover(tab_group.clone(), |style| style.invisible())
                        .child(cluster),
                );
            }
            None => {
                title_row = title_row.child(div().flex_none().w(px(SIDEBAR_ACTION_BUTTON_SIZE)));
            }
        }

        // US-010 patron, applied to a tab: closing drops the `Tab`, and with it
        // its panes and their terminals. Closing the last tab of a workspace
        // leaves an empty tab behind and never closes the workspace (FR-01) -
        // the folder row's own `x` is what closes that.
        //
        // Issue #183: one click routes through the SAME modal chokepoint as
        // Cmd+W and the tab context menu's Close. A tab with nothing live
        // closes immediately; a live-agent tab gets the confirmation window.
        // The old inline arm-then-confirm (#83) needed two clicks and threw a
        // double-click's second half away, so closing often took three.
        let close_actions = sidebar_hover_actions(tab_group.clone());
        title_row = title_row.child(
            close_actions.child(
                sidebar_action_button(
                    SharedString::from(format!("tab-close-{tab_id}")),
                    SharedString::from("Close tab"),
                    "icons/close.svg",
                    12.,
                    ui,
                )
                .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                    // A double-click delivers BOTH clicks to this listener
                    // (the row's own double-click-to-rename below works only
                    // because it does). The first click already acted: an idle
                    // tab closed - the rows below slide up, so the gesture's
                    // second half would land on the NEXT row's x - or the
                    // modal opened, and a second half delivered before its
                    // occluding backdrop paints would re-request the same
                    // close. Either way the tail must do nothing.
                    if matches!(e, ClickEvent::Mouse(m) if m.down.click_count >= 2) {
                        cx.stop_propagation();
                        return;
                    }
                    // Re-resolve both indices by id: rows reorder under a drag,
                    // and a stale position would close the wrong tab.
                    if let Some((at_ws, at_tab)) = this
                        .workspaces
                        .iter()
                        .position(|ws| ws.id == ws_id)
                        .and_then(|at_ws| {
                            this.workspaces[at_ws]
                                .tabs()
                                .iter()
                                .position(|tab| tab.id == tab_id)
                                .map(|at_tab| (at_ws, at_tab))
                        })
                    {
                        this.commit_inline_rename(window, cx);
                        this.request_close_workspace_tab(
                            at_ws,
                            at_tab,
                            crate::app::close_guard::ConfirmStyle::Modal,
                            window,
                            cx,
                        );
                    }
                    cx.stop_propagation();
                })),
            ),
        );

        let row_shell = sidebar_row_shell()
            .id(SharedString::from(format!("tab-row-{tab_id}")))
            .group(tab_group.clone())
            .on_drag(
                TabDrag {
                    workspace_id: ws_id,
                    tab_id,
                    title: SharedString::from(title),
                },
                |drag, _offset, _window, cx| {
                    cx.new(|_| WorkspaceDragPreview {
                        title: drag.title.clone(),
                    })
                },
            )
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                let is_double = matches!(e, ClickEvent::Mouse(m) if m.down.click_count == 2);
                if is_double {
                    // `select_workspace_tab` on the single-click branch already
                    // commits and dismisses (see `workspace_ops::tab`), so the
                    // rename branch must not dismiss again after it - issue #79
                    // made that call cancel a live editor.
                    this.begin_tab_rename(ws_idx, tab_idx, window, cx);
                } else {
                    this.select_workspace_tab(ws_idx, tab_idx, window, cx);
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .on_aux_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                if e.is_right_click()
                    && let Some(position) = e.mouse_position()
                {
                    // Same as the folder row: the menu that replaces the
                    // editor cannot take the focus the row gives up.
                    this.commit_inline_rename(window, cx);
                    this.dismiss_transient_surfaces();
                    this.tab_menu_open = Some(TabContextMenu {
                        ws_idx,
                        tab_idx,
                        position,
                    });
                    // Issue #347: refresh the repository's branch and worktree
                    // lists for the menu's Branch section. Off the render
                    // thread, and only on this gesture - the menu is the only
                    // reader.
                    this.spawn_worktree_listing(ws_idx, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(move |this, e: &KeyDownEvent, window, cx| {
                // M1: this `f2` branch is structurally unreachable, and is
                // kept only so the handler reads as a whole. A row is on
                // GPUI's dispatch path only while it tracks
                // `sidebar_rename_focus`, and it tracks it only while its own
                // rename is ALREADY live - precisely the case this branch
                // excludes. Nothing to fix here: making `f2` start a rename
                // needs a focus handle per row, not a change to this branch.
                if this.renaming_tab != Some((ws_idx, tab_idx)) {
                    if e.keystroke.key.as_str() == "f2" {
                        this.begin_tab_rename(ws_idx, tab_idx, window, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                match rename_key_action(
                    e.keystroke.key.as_str(),
                    e.keystroke.key_char.as_deref(),
                    e.keystroke.modifiers,
                ) {
                    RenameKey::Commit => {
                        this.commit_rename(cx);
                        this.restore_focus_after_rename(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Cancel => {
                        this.renaming_tab = None;
                        this.rename_text.clear();
                        this.rename_seeded = false;
                        this.restore_focus_after_rename(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Backspace => {
                        if !take_rename_selection(&mut this.rename_text, &mut this.rename_seeded) {
                            this.rename_text.pop();
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Insert(ch) => {
                        take_rename_selection(&mut this.rename_text, &mut this.rename_seeded);
                        this.rename_text.push_str(&ch);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKey::Ignore => {}
                }
            }));

        // Issue #79: same gate as the folder row - the tab being renamed is the
        // one row that puts the shared handle on the dispatch path.
        // Issue #80: that row also owns the click-outside commit. Keeping the
        // listener on the live editor only avoids installing one capture-phase
        // listener per tab while preserving the typed buffer before another
        // surface takes focus.
        let row_shell = if is_renaming {
            row_shell
                .track_focus(&self.sidebar_rename_focus)
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.commit_inline_rename(window, cx);
                    cx.notify();
                }))
        } else {
            row_shell
        };

        // Issue #347: a bound tab names the branch of its own checkout on a
        // second line, so two tabs of one workspace on two worktrees read
        // apart at a glance. An unbound tab draws nothing extra - its branch
        // is the workspace's, already on the folder row above.
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(SIDEBAR_ROW_GAP))
            .child(title_row);
        if let Some(meta_row) = self.render_tab_worktree_meta_row(tab, ui) {
            body = body.child(meta_row);
        }
        let row = squircle_row(row_shell, tab_group, resting_bg, hovered_bg, body);

        div()
            .id(SharedString::from(format!("tab-drop-{tab_id}")))
            .mx(px(SIDEBAR_ROW_MARGIN_X))
            .flex_none()
            .flex()
            .flex_col()
            .rounded(ROW_RADIUS)
            .child(row)
    }

    /// The branch line under a bound tab's title (issue #347): the branch its
    /// worktree is on, from the cached probe, or the checkout's directory name
    /// while the first probe is in flight or when HEAD is detached. `None` for
    /// an unbound tab, which has nothing of its own to say.
    fn render_tab_worktree_meta_row(
        &self,
        tab: &Tab,
        ui: crate::theme::UiColors,
    ) -> Option<AnyElement> {
        let path = tab.worktree.as_ref()?;
        let branch = self
            .tab_checkout_git(tab)
            .map(|git| git.branch.clone())
            .unwrap_or_default();
        let repo_root = self
            .workspaces
            .iter()
            .find(|ws| ws.tabs().iter().any(|t| t.id == tab.id))
            .map(|ws| ws.worktree_root.clone())
            .unwrap_or_default();
        let label = crate::workspace::worktree::checkout_label(Some(&branch), path, &repo_root);
        if label.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .pl(px(SIDEBAR_FOLDER_ICON_WIDTH + SIDEBAR_TITLE_ROW_GAP))
                .w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
                .max_w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
                .h(px(14.))
                .overflow_x_hidden()
                .whitespace_nowrap()
                .text_xs()
                .text_color(ui.muted)
                .child(
                    svg()
                        .size(px(10.))
                        .flex_none()
                        .path("icons/git-branch-sidebar.svg")
                        .text_color(ui.muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .overflow_x_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                )
                .into_any_element(),
        )
    }

    fn render_workspace_meta_row(
        &self,
        ws: &Workspace,
        opacity: f32,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // Branch + detected services on one clipped line. Diffstat stays out of
        // the rail (Diff view owns change counts); the branch is the identity
        // cue that makes sibling worktrees distinguishable at a glance.
        let has_branch = !ws.git_branch.is_empty();
        let service = sidebar_service_summary(&ws.active_ports, &ws.service_labels);
        let has_ports = service.is_some();
        if !has_branch && !has_ports {
            return None;
        }

        let mut meta_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
            .max_w(px(SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH))
            .h(px(14.))
            .overflow_x_hidden()
            .whitespace_nowrap()
            .text_xs()
            .opacity(opacity)
            .text_color(ui.muted);

        if has_branch {
            let branch_width = if has_ports {
                126.0
            } else {
                SIDEBAR_WORKSPACE_ROW_CONTENT_WIDTH
            };
            meta_row = meta_row.child(
                div()
                    .id(SharedString::from(format!("branch-{}", ws.id)))
                    .min_w_0()
                    .max_w(px(branch_width))
                    .overflow_x_hidden()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .delayed_tooltip({
                        let label: SharedString = ws.git_branch.clone().into();
                        move |_w, cx| {
                            cx.new(|_| SidebarTooltip {
                                label: label.clone(),
                            })
                            .into()
                        }
                    })
                    .child(
                        svg()
                            .size(px(10.))
                            .flex_none()
                            .path("icons/git-branch-sidebar.svg")
                            .text_color(ui.muted),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .overflow_x_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(ws.git_branch.clone()),
                    ),
            );
        }

        if has_branch && has_ports {
            meta_row = meta_row.child(div().flex_none().text_color(ui.muted).child("·"));
        }

        if let Some(service) = service {
            let port = service.primary;
            let workspace_id = ws.id;
            let info = ws.service_labels.get(&port);
            let is_frontend = info.is_some_and(|service| service.is_frontend);
            let service_name = info
                .and_then(|service| service.label.clone())
                .unwrap_or_else(|| "Local service".to_string());
            let service_tooltip: SharedString = format!("{service_name}  :{port}").into();

            if is_frontend {
                let url = info
                    .and_then(|service| service.url.clone())
                    .unwrap_or_else(|| format!("http://localhost:{port}"));
                meta_row = meta_row.child(
                    div()
                        .id(SharedString::from(format!("port-{workspace_id}-{port}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.))
                        .text_size(px(10.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ui.muted)
                        .hover(move |style| style.text_color(ui.text))
                        .delayed_tooltip({
                            let label = service_tooltip.clone();
                            move |_w, cx| {
                                cx.new(|_| SidebarTooltip {
                                    label: label.clone(),
                                })
                                .into()
                            }
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.open_workspace_service_url(&url, cx);
                            cx.stop_propagation();
                        }))
                        .child(
                            svg()
                                .size(px(10.))
                                .flex_none()
                                .path("icons/world.svg")
                                .text_color(ui.muted),
                        )
                        .child(format!(":{port}")),
                );
            } else {
                meta_row = meta_row.child(
                    div()
                        .id(SharedString::from(format!(
                            "port-{workspace_id}-{port}-info"
                        )))
                        .text_size(px(10.))
                        .text_color(ui.muted)
                        .delayed_tooltip({
                            let label = service_tooltip.clone();
                            move |_w, cx| {
                                cx.new(|_| SidebarTooltip {
                                    label: label.clone(),
                                })
                                .into()
                            }
                        })
                        .child(format!(":{port}")),
                );
            }

            if service.overflow > 0 {
                let overflow = service.overflow;
                meta_row = meta_row.child(
                    div()
                        .id(SharedString::from(format!("ports-{workspace_id}-overflow")))
                        .flex_none()
                        .text_size(px(10.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ui.muted)
                        .delayed_tooltip(move |_w, cx| {
                            cx.new(|_| SidebarTooltip {
                                label: format!(
                                    "{overflow} more services · Right-click workspace to view"
                                )
                                .into(),
                            })
                            .into()
                        })
                        .child(format!("+{overflow}")),
                );
            }
        }

        Some(meta_row.into_any_element())
    }

    pub(crate) fn sidebar_list_wrapper(
        &self,
        list: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        // The visible scroll bar was removed; wheel-scroll on the
        // inner `list` (driven by `overflow_y_scroll + track_scroll`)
        // is the only scrolling surface now. The wrapper still
        // exists so callers keep a stable insertion point if a
        // trailing affordance lands here later.
        //
        // It is also the rail's drop target: a folder dragged out of the OS
        // file manager and released here is filed as a workspace. The whole
        // list area accepts it rather than a dedicated strip, because the
        // gesture aims at "the sidebar", not at a position in it - the rail's
        // order is the user's own, and a dropped folder joins at the end.
        div()
            .id("sidebar-list-wrapper")
            .relative()
            .group(SIDEBAR_DROP_GROUP)
            .flex_1()
            .flex()
            .flex_col()
            .min_h_0()
            // The wrapper is the drop target for the full area, margin band
            // included, so nothing lands in the gap around the placeholder.
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.open_workspace_folders(paths.paths(), cx);
                }),
            )
            .child(list)
            .child(Self::render_sidebar_drop_placeholder(cx))
    }

    /// Neutral drop placeholder for a folder dragged in from the OS file
    /// manager, on the pane-swap grammar (`pane.rs`): a translucent wash of the
    /// theme's text color with a hairline of the same, rounded and floating
    /// inside the rail. Neutral, not the blue split preview - blue means "a new
    /// pane lands here", and this files a folder instead.
    ///
    /// It is absolute, so it never reflows the list, and `invisible()` until a
    /// drag enters the wrapper's group. The `on_drop` is what earns it a
    /// hitbox: GPUI evaluates `group_drag_over` styles only inside
    /// `if let Some(hitbox)`, so a handler-less div would stay invisible
    /// forever (same trap documented on the pane overlay).
    fn render_sidebar_drop_placeholder(cx: &mut Context<Self>) -> impl IntoElement {
        let tint = crate::theme::ui_colors().text;
        div()
            .absolute()
            .top(px(SIDEBAR_DROP_PLACEHOLDER_MARGIN))
            .left(px(SIDEBAR_DROP_PLACEHOLDER_MARGIN))
            .right(px(SIDEBAR_DROP_PLACEHOLDER_MARGIN))
            .bottom(px(SIDEBAR_DROP_PLACEHOLDER_MARGIN))
            .rounded(px(SIDEBAR_DROP_PLACEHOLDER_RADIUS))
            .bg(tint.opacity(SIDEBAR_DROP_PLACEHOLDER_FILL_ALPHA))
            .border_2()
            .border_color(tint.opacity(SIDEBAR_DROP_PLACEHOLDER_BORDER_ALPHA))
            .invisible()
            .group_drag_over::<gpui::ExternalPaths>(SIDEBAR_DROP_GROUP, |style| style.visible())
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.open_workspace_folders(paths.paths(), cx);
                }),
            )
    }
}

fn sidebar_agent_status_tooltip(
    summary: SidebarAgentSummary,
    status: &ai_types::WorkspaceAgentStatus,
) -> SharedString {
    let state = summary.tooltip_state();
    if summary.state == SidebarAgentState::Finished {
        return state.into();
    }

    let mut details: Vec<String> = status
        .hooked
        .iter()
        .map(|aggregate| {
            format!(
                "{}{}",
                aggregate.tool.display_name(),
                aggregate.extra_suffix()
            )
        })
        .chain(
            status
                .unhooked
                .iter()
                .map(|tool| format!("{} running", tool.display_name())),
        )
        .collect();
    for label in &status.active_labels {
        if !details.iter().any(|detail| detail.starts_with(label)) {
            details.push(label.clone());
        }
    }

    if details.is_empty() {
        state.into()
    } else {
        format!("{state} · {}", details.join(", ")).into()
    }
}

/// US-013: one icon per pane of a tab, in the leaf order of its tree, so a
/// work thread is recognizable without opening it. `None` for an empty tab -
/// nothing is painted and the row keeps the height of every other row.
fn render_tab_pane_icons(
    icons: &[TabPaneIcon],
    row_key: &str,
    pending_pane: bool,
    ui: crate::theme::UiColors,
) -> Option<AnyElement> {
    if icons.is_empty() {
        return None;
    }
    let (shown, overflow) = tab_icon_cluster_split(icons.len());
    let tooltip: SharedString = icons
        .iter()
        .map(|icon| icon.label)
        .collect::<Vec<_>>()
        .join(", ")
        .into();

    // Every glyph is monochrome - one tint for the whole rail, no brand color.
    // That means `svg()` for all of them, and `svg()` is a mask: without its
    // own `text_color` it paints nothing, and the parent's does not cascade.
    let glyph = ui.text.opacity(0.75);
    // A lone pane is drawn bare: a card around a single icon is a box, not a
    // stack. From two panes up each one gets its own card and the cards slide
    // over each other, which is what reads as a stack. `pending_pane` is the
    // one exception - a split picker already stands in the second slot, so the
    // lone pane cards up now instead of popping into a card the moment the
    // preset is picked.
    if shown == 1 && !pending_pane {
        return Some(tab_pane_icon_lane(
            row_key,
            tooltip,
            overflow,
            ui,
            svg()
                .size(px(SIDEBAR_TAB_ICON_SIZE))
                .min_w(px(SIDEBAR_TAB_ICON_SIZE))
                .flex_none()
                .path(icons[0].path)
                .text_color(glyph),
        ));
    }

    let card_fill = crate::app::constants::sidebar_tab_icon_card_background();
    let card_border = ui.text.opacity(0.14);
    // Brighter than the bare glyph above: the card fill is a step darker than
    // the row, so the mark needs the contrast back to read as the same weight.
    let card_glyph = ui.text.opacity(0.92);

    // The stack is laid out absolutely, not by flex with negative margins.
    // The cluster sits at the end of a fixed-width title row, and a flex row
    // is free to shrink or clip a child whose size is derived from its
    // content: derived, the cards measured about half their width, then
    // vanished entirely once there were two of them. Stating the cluster's
    // exact box and placing each card at its own offset inside it takes the
    // row's remaining space out of the equation.
    let step = SIDEBAR_TAB_CARD_WIDTH - SIDEBAR_TAB_ICON_OVERLAP;
    let cluster_width = SIDEBAR_TAB_CARD_WIDTH + (shown.saturating_sub(1) as f32) * step;
    // The cluster reserves the row's line height, not the card's: the cards
    // are absolute, so their extra two pixels overhang into the row's vertical
    // padding instead of making a tab row taller than every other row.
    let card_overhang = (SIDEBAR_ROW_LINE_HEIGHT - SIDEBAR_TAB_CARD_HEIGHT) / 2.0;
    let mut cluster = div()
        .flex_none()
        .relative()
        .w(px(cluster_width))
        .min_w(px(cluster_width))
        .h(px(SIDEBAR_ROW_LINE_HEIGHT));
    for (slot, icon) in icons[..shown].iter().enumerate() {
        // Cards paint in declaration order, so the later pane sits on top -
        // the same direction as the visual reference.
        cluster = cluster.child(
            div()
                .absolute()
                .top(px(card_overhang))
                .left(px(slot as f32 * step))
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .w(px(SIDEBAR_TAB_CARD_WIDTH))
                .h(px(SIDEBAR_TAB_CARD_HEIGHT))
                // Fill and hairline are painted paths, not `bg()` + `border_1()`:
                // GPUI's box border is a circular arc and would betray the
                // squircle underneath it at the corners, where the two curves
                // diverge most. Both children are absolute, so the flex box
                // still centers the glyph alone.
                .child(squircle::squircle_fill(ROW_RADIUS, card_fill))
                .child(squircle::squircle_border(ROW_RADIUS, px(1.), card_border))
                .child(
                    svg()
                        .size(px(SIDEBAR_TAB_CARD_ICON_SIZE))
                        .flex_none()
                        .path(icon.path)
                        .text_color(card_glyph),
                ),
        );
    }
    Some(tab_pane_icon_lane(row_key, tooltip, overflow, ui, cluster))
}

/// US-013: the trailing lane of a tab row - the pane cluster, and past the cap
/// the `+N`. The counter is text, not a glyph: it labels the stack from
/// outside it, so it neither joins the overlap nor makes a card's width depend
/// on how a digit measures. The lane carries the row's pane tooltip.
fn tab_pane_icon_lane(
    row_key: &str,
    tooltip: SharedString,
    overflow: usize,
    ui: crate::theme::UiColors,
    cluster: impl IntoElement,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("tab-panes-{row_key}")))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(SIDEBAR_TAB_ICON_GAP))
        .delayed_tooltip(move |_w, cx| {
            cx.new(|_| SidebarTooltip {
                label: tooltip.clone(),
            })
            .into()
        })
        .child(cluster)
        .when(overflow > 0, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_size(px(10.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(ui.muted)
                    .child(format!("+{overflow}")),
            )
        })
        .into_any_element()
}

/// `row_key` scopes the element and animation ids to one sidebar row. It is a
/// string, not an id: workspace ids and tab ids come from independent counters,
/// so a folder row and a tab row could otherwise collide on the same numeric
/// key inside the same list.
fn render_workspace_agent_summary(
    summary: SidebarAgentSummary,
    row_key: &str,
    tooltip: SharedString,
    ui: crate::theme::UiColors,
) -> AnyElement {
    let (color, glyph, label) = agent_summary_visual(summary, row_key, ui);

    div()
        .id(SharedString::from(format!("agent-status-{row_key}")))
        .w(px(summary.slot_width()))
        .h(px(20.))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(3.))
        .text_size(px(10.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .delayed_tooltip(move |_w, cx| {
            cx.new(|_| SidebarTooltip {
                label: tooltip.clone(),
            })
            .into()
        })
        .child(glyph)
        .when_some(label, |d, label| d.child(label))
        .into_any_element()
}

/// US-012: on a tab row the activity badge leads the title instead of trailing
/// it - the trailing lane belongs to the pane-icon cluster. It occupies exactly
/// the folder-icon slot the invisible placeholder would have taken, so the tab
/// title keeps the same X whether or not an agent is running, and the count
/// stays in the tooltip rather than widening the slot and shifting the title.
fn render_tab_agent_summary(
    summary: SidebarAgentSummary,
    row_key: &str,
    tooltip: SharedString,
    ui: crate::theme::UiColors,
) -> AnyElement {
    let (_, glyph, _) = agent_summary_visual(summary, row_key, ui);

    div()
        .id(SharedString::from(format!("agent-status-{row_key}")))
        .w(px(SIDEBAR_FOLDER_ICON_WIDTH))
        .h(px(20.))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .delayed_tooltip(move |_w, cx| {
            cx.new(|_| SidebarTooltip {
                label: tooltip.clone(),
            })
            .into()
        })
        .child(glyph)
        .into_any_element()
}

fn agent_summary_visual(
    summary: SidebarAgentSummary,
    row_key: &str,
    ui: crate::theme::UiColors,
) -> (gpui::Hsla, AnyElement, Option<String>) {
    match summary.state {
        SidebarAgentState::NeedsInput => (
            rgb(0xFBBF24).into(),
            svg()
                .size(px(11.))
                .flex_none()
                .path("icons/bell.svg")
                .text_color(rgb(0xFBBF24))
                .into_any_element(),
            Some(if summary.count > 1 {
                format!("Input {}", summary.count)
            } else {
                "Input".to_string()
            }),
        ),
        SidebarAgentState::Errored => (
            ui.agent_error,
            svg()
                .size(px(11.))
                .flex_none()
                .path("icons/x_circle.svg")
                .text_color(ui.agent_error)
                .into_any_element(),
            (summary.count > 1).then(|| summary.count.to_string()),
        ),
        SidebarAgentState::Stalled => (
            ui.agent_stalled,
            svg()
                .size(px(11.))
                .flex_none()
                .path("icons/triangle-alert.svg")
                .text_color(ui.agent_stalled)
                .into_any_element(),
            (summary.count > 1).then(|| summary.count.to_string()),
        ),
        SidebarAgentState::Thinking => {
            let color = ui.muted;
            (
                color,
                render_comet_trail_loader(row_key, color),
                (summary.count > 1).then(|| summary.count.to_string()),
            )
        }
        SidebarAgentState::Finished => {
            let color: gpui::Hsla = rgb(0x83C3FF).into();
            (
                color,
                div()
                    .size(px(11.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(px(7.)).rounded_full().bg(color))
                    .into_any_element(),
                None,
            )
        }
    }
}

/// Compact GPUI adaptation of Dot Matrix's `Comet Trail` loader. The 3x3
/// perimeter leaves room for larger dots while keeping the native sidebar free
/// of a web runtime, glow, or accent color.
fn render_comet_trail_loader(row_key: &str, color: gpui::Hsla) -> AnyElement {
    static SYNC_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

    const MATRIX_SIZE: usize = 3;
    const DOT_SIZE: f32 = 3.0;
    const DOT_GAP: f32 = 1.0;
    const CYCLE_MS: u64 = 720;
    const PERIMETER: usize = 8;
    const BASE_OPACITY: f32 = 0.06;
    const TAIL_OPACITIES: [f32; 3] = [0.8144, 0.4864, 0.2568];

    div()
        .size(px(11.))
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(DOT_GAP))
        .with_animation(
            SharedString::from(format!("comet-trail-{row_key}")),
            Animation::new(std::time::Duration::from_millis(CYCLE_MS)).repeat(),
            move |loader, _delta| {
                let cycle_elapsed = SYNC_EPOCH
                    .get_or_init(std::time::Instant::now)
                    .elapsed()
                    .as_millis()
                    % u128::from(CYCLE_MS);
                let head = (cycle_elapsed * PERIMETER as u128 / u128::from(CYCLE_MS)) as usize;

                loader.children((0..MATRIX_SIZE).map(|row| {
                    div()
                        .h(px(DOT_SIZE))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .gap(px(DOT_GAP))
                        .children((0..MATRIX_SIZE).map(move |col| {
                            let order = match (row, col) {
                                (0, 0) => Some(0),
                                (0, 1) => Some(1),
                                (0, 2) => Some(2),
                                (1, 2) => Some(3),
                                (2, 2) => Some(4),
                                (2, 1) => Some(5),
                                (2, 0) => Some(6),
                                (1, 0) => Some(7),
                                _ => None,
                            };
                            let opacity = order.map_or_else(
                                || if head.is_multiple_of(2) { 0.1 } else { 0.18 },
                                |order| {
                                    let trail = (head + PERIMETER - order) % PERIMETER;
                                    TAIL_OPACITIES.get(trail).copied().unwrap_or(BASE_OPACITY)
                                },
                            );

                            div()
                                .size(px(DOT_SIZE))
                                .flex_none()
                                .rounded_full()
                                .bg(color.opacity(opacity))
                        }))
                }))
            },
        )
        .into_any_element()
}

/// Lightweight tooltip body reused by sidebar affordances that just
/// need to show one short label.
/// `pub(crate)`: the tab identity pill (EP-005, pane.rs) reuses it rather
/// than duplicating a fourth one-label tooltip body.
pub(crate) struct SidebarTooltip {
    pub(crate) label: SharedString,
}

impl Render for SidebarTooltip {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        crate::ui_primitives::tooltip_shell().child(self.label.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IDLE_WORKSPACE_TEXT_OPACITY, PaneFlowApp, ROW_RADIUS, RenameKey, SIDEBAR_ACTION_BUTTON_GAP,
        SIDEBAR_ACTION_BUTTON_SIZE, SIDEBAR_ACTION_LANE_WIDTH, SIDEBAR_DROP_BAND_REACH,
        SIDEBAR_DROP_LINE_PX, SIDEBAR_FOLDER_ICON_WIDTH, SIDEBAR_ROW_LINE_HEIGHT,
        SIDEBAR_ROW_MARGIN_X, SIDEBAR_ROW_PADDING_Y, SIDEBAR_ROW_SPACING, SIDEBAR_TAB_CARD_HEIGHT,
        SIDEBAR_TAB_CARD_ICON_SIZE, SIDEBAR_TAB_CARD_WIDTH, SIDEBAR_TAB_ICON_CAP,
        SIDEBAR_TAB_ICON_SIZE, SIDEBAR_TITLE_ROW_GAP, SIDEBAR_WIDTH, SidebarAgentState,
        SidebarAgentSummary, SidebarDropSlot, SidebarRow, SidebarServiceSummary, WorkspaceOrderKey,
        compute_auto_order, folder_row_sessions, rename_editor_skin, rename_key_action,
        reorder_target, sidebar_agent_summary, sidebar_drop_slots, sidebar_row_shell,
        sidebar_service_summary, sidebar_tab_title_opacity, sidebar_workspace_tone,
        tab_display_title, tab_icon_cluster_split, tab_row_sessions, tab_row_title,
        take_rename_selection, visible_service_ports,
    };
    use crate::agent_launcher::TerminalAgent;
    use crate::ai_types::{AgentSession, AgentState};
    use crate::source_probe::source_slice;
    use crate::terminal::ServiceInfo;
    use crate::workspace::Tab;
    use gpui::{
        AppContext, AvailableSpace, InteractiveElement, Modifiers, ParentElement, Styled,
        TestAppContext, div, point, px, size,
    };
    use std::collections::{HashMap, HashSet};

    fn session(state: AgentState) -> AgentSession {
        AgentSession::new(TerminalAgent::ClaudeCode, state)
    }

    #[test]
    fn idle_unselected_workspace_quiets_only_its_text_content() {
        let tone = sidebar_workspace_tone(false, true, false);
        assert!(tone.title_opacity < 1.0);
        assert!(tone.meta_opacity < 1.0);
    }

    #[test]
    fn selected_busy_and_waiting_workspaces_stay_full_strength() {
        for tone in [
            sidebar_workspace_tone(true, true, false),
            sidebar_workspace_tone(false, false, false),
            sidebar_workspace_tone(false, true, true),
        ] {
            assert_eq!(tone.title_opacity, 1.0);
            assert_eq!(tone.meta_opacity, 1.0);
        }
    }

    #[test]
    fn idle_workspace_tone_never_dims_the_row_shell_or_agent_badge() {
        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production sidebar source");
        let workspace_row = source_slice(
            production,
            "fn render_workspace_row(\n",
            "fn render_tab_row(\n",
        );

        assert!(
            workspace_row.contains("text_color(ui.text.opacity(tone.title_opacity))"),
            "idle tone must be applied directly to title text"
        );
        assert!(
            !workspace_row.contains(".opacity(tone.title_opacity)\n"),
            "idle tone on a parent would also fade the row chrome or agent badge"
        );
        assert!(
            workspace_row
                .contains("render_workspace_agent_summary(\n                row_agent_status,"),
            "the agent badge must remain an independent full-strength element"
        );
    }

    #[test]
    fn reorder_target_accounts_for_the_removed_source() {
        // Moving down: the source leaves first, so everything after it slides
        // up and the gap the line pointed at is one index lower.
        assert_eq!(reorder_target(0, 3), 2);
        // Moving up: nothing before the gap moves.
        assert_eq!(reorder_target(4, 1), 1);
        // The two gaps around the source are both no-ops.
        assert_eq!(reorder_target(2, 2), 2);
        assert_eq!(reorder_target(2, 3), 2);
    }

    #[test]
    fn drop_slots_sit_between_the_rendered_rows() {
        // One expanded workspace with two tabs, then a collapsed one.
        let rows = [
            SidebarRow::Folder(0),
            SidebarRow::Tab(0, 0),
            SidebarRow::Tab(0, 1),
            SidebarRow::Folder(1),
        ];
        let slots = sidebar_drop_slots(&rows, 2, false);

        assert_eq!(slots.len(), rows.len() + 1);
        // Above everything: a folder lands first, a tab has nowhere to go.
        assert_eq!(
            slots[0],
            SidebarDropSlot {
                tab: None,
                workspace: Some(0)
            }
        );
        // Under a folder row: that workspace's first tab. Not a folder gap.
        assert_eq!(
            slots[1],
            SidebarDropSlot {
                tab: Some((0, 0)),
                workspace: None
            }
        );
        assert_eq!(
            slots[2],
            SidebarDropSlot {
                tab: Some((0, 1)),
                workspace: None
            }
        );
        // Between two folders: appends to the one above, and reorders folders.
        assert_eq!(
            slots[3],
            SidebarDropSlot {
                tab: Some((0, 2)),
                workspace: Some(1)
            }
        );
        // Past the last row: appends to the collapsed workspace, folder last.
        assert_eq!(
            slots[4],
            SidebarDropSlot {
                tab: Some((1, 0)),
                workspace: Some(2)
            }
        );
    }

    #[gpui::test]
    /// The divider carries the insertion line as an absolutely positioned band
    /// so it can reach over its neighbors without displacing them - a geometry
    /// worth pinning, since getting it wrong paints nothing at all.
    fn a_drop_divider_spans_the_rail_and_reaches_over_its_neighbors(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        cx.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(SIDEBAR_WIDTH)),
                AvailableSpace::Definite(px(100.)),
            ),
            |_, _| {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(div().h(px(30.)))
                    .child(
                        div()
                            .h(px(SIDEBAR_ROW_SPACING))
                            .flex_none()
                            .relative()
                            .child(
                                div()
                                    .absolute()
                                    .top(px(-SIDEBAR_DROP_BAND_REACH))
                                    .w_full()
                                    .px(px(SIDEBAR_ROW_MARGIN_X))
                                    .h(px(SIDEBAR_ROW_SPACING + SIDEBAR_DROP_BAND_REACH * 2.0))
                                    .flex()
                                    .flex_col()
                                    .justify_center()
                                    .debug_selector(|| "band".into())
                                    .child(
                                        div()
                                            .h(px(SIDEBAR_DROP_LINE_PX))
                                            .w_full()
                                            .bg(gpui::rgb(0xff0000))
                                            .debug_selector(|| "line".into()),
                                    ),
                            ),
                    )
            },
        );
        let band = cx.debug_bounds("band").expect("drop band not painted");
        let line = cx.debug_bounds("line").expect("drop line not painted");

        // Sized by a width, never by a `left`/`right` inset pair: insets alone
        // leave an absolutely positioned element 0 px wide, and a zero-width
        // band is both invisible and impossible to hover.
        assert_eq!(band.size.width, px(SIDEBAR_WIDTH));
        assert_eq!(
            line.size.width,
            px(SIDEBAR_WIDTH - 2. * SIDEBAR_ROW_MARGIN_X)
        );
        // Reaches half a row above the gap it owns, so consecutive bands tile
        // the rail: every drop aims at the nearest gap.
        assert_eq!(band.origin.y, px(30.) - px(SIDEBAR_DROP_BAND_REACH));
        assert_eq!(
            band.size.height,
            px(SIDEBAR_ROW_SPACING + SIDEBAR_DROP_BAND_REACH * 2.0)
        );
        // The line rests in the gap itself, centered in the band.
        assert_eq!(line.origin.y, px(30.) + px(SIDEBAR_ROW_SPACING / 2.0 - 1.0));
    }

    #[gpui::test]
    fn a_single_line_row_is_thirty_pixels_tall(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        cx.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(SIDEBAR_WIDTH)),
                AvailableSpace::Definite(px(100.)),
            ),
            |_, _| {
                sidebar_row_shell()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .child(div().flex_none().size(px(SIDEBAR_FOLDER_ICON_WIDTH)))
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(px(SIDEBAR_ROW_LINE_HEIGHT))
                                    .child("paneflow"),
                            ),
                    )
                    .debug_selector(|| "row".into())
            },
        );

        let bounds = cx.debug_bounds("row").expect("row not painted");
        assert_eq!(
            bounds.size.height,
            px(SIDEBAR_ROW_LINE_HEIGHT + 2. * SIDEBAR_ROW_PADDING_Y),
            "a title line must not let the font's own line height set the row height"
        );
        assert_eq!(bounds.size.height, px(30.));
        // `squircle::trace` silently clamps the radius to half the shorter
        // side, so a corner larger than this would stop being the corner the
        // constant claims.
        assert!(
            ROW_RADIUS <= bounds.size.height / 2.,
            "row corner {ROW_RADIUS:?} exceeds half of a {:?} row",
            bounds.size.height
        );
    }

    #[gpui::test]
    fn sidebar_workspace_rows_keep_height_when_list_overflows(cx: &mut TestAppContext) {
        const ROWS: [&str; 8] = [
            "sidebar-row-0",
            "sidebar-row-1",
            "sidebar-row-2",
            "sidebar-row-3",
            "sidebar-row-4",
            "sidebar-row-5",
            "sidebar-row-6",
            "sidebar-row-7",
        ];

        let cx = cx.add_empty_window();
        cx.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(240.)),
                AvailableSpace::Definite(px(200.)),
            ),
            |_, _| {
                let mut list = div()
                    .w(px(240.))
                    .h(px(200.))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(px(4.));

                for selector in ROWS {
                    list = list.child(
                        sidebar_row_shell()
                            .child(div().h(px(20.)).flex_none())
                            .child(div().h(px(14.)).flex_none())
                            .debug_selector(move || selector.into()),
                    );
                }
                list
            },
        );

        for selector in ROWS {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} not painted"));
            assert_eq!(bounds.size.height, px(50.), "{selector}");
        }
    }

    #[test]
    fn visible_service_ports_hide_unlabeled_ephemeral_ports() {
        let labels = HashMap::from([
            (
                3000,
                ServiceInfo {
                    port: 3000,
                    url: Some("http://localhost:3000".to_string()),
                    label: Some("Next.js".to_string()),
                    is_frontend: true,
                },
            ),
            (
                8000,
                ServiceInfo {
                    port: 8000,
                    url: Some("http://localhost:8000".to_string()),
                    label: Some("Fastify".to_string()),
                    is_frontend: false,
                },
            ),
        ]);

        assert_eq!(
            visible_service_ports(&[3000, 53154, 8000, 53155], &labels),
            vec![3000, 8000]
        );
    }

    #[test]
    fn sidebar_service_summary_prefers_frontend_and_counts_overflow() {
        let labels = HashMap::from([
            (
                3000,
                ServiceInfo {
                    port: 3000,
                    url: Some("http://localhost:3000".to_string()),
                    label: Some("API".to_string()),
                    is_frontend: false,
                },
            ),
            (
                5173,
                ServiceInfo {
                    port: 5173,
                    url: Some("http://localhost:5173".to_string()),
                    label: Some("Vite".to_string()),
                    is_frontend: true,
                },
            ),
            (
                8000,
                ServiceInfo {
                    port: 8000,
                    url: Some("http://localhost:8000".to_string()),
                    label: Some("Fastify".to_string()),
                    is_frontend: false,
                },
            ),
        ]);

        assert_eq!(
            sidebar_service_summary(&[3000, 53154, 5173, 8000], &labels),
            Some(SidebarServiceSummary {
                primary: 5173,
                overflow: 2,
            })
        );
    }

    #[test]
    fn sidebar_service_summary_falls_back_to_first_visible_service() {
        let labels = HashMap::from([
            (
                3000,
                ServiceInfo {
                    port: 3000,
                    url: None,
                    label: Some("API".to_string()),
                    is_frontend: false,
                },
            ),
            (
                8000,
                ServiceInfo {
                    port: 8000,
                    url: None,
                    label: Some("Worker".to_string()),
                    is_frontend: false,
                },
            ),
        ]);

        assert_eq!(
            sidebar_service_summary(&[3000, 8000], &labels),
            Some(SidebarServiceSummary {
                primary: 3000,
                overflow: 1,
            })
        );
    }

    #[test]
    fn sidebar_agent_summary_hides_idle_without_signal() {
        assert_eq!(sidebar_agent_summary(std::iter::empty(), false), None);
    }

    #[test]
    fn sidebar_agent_summary_counts_winning_needs_input_sessions() {
        let sessions = [
            session(AgentState::WaitingForInput),
            session(AgentState::Errored),
            session(AgentState::WaitingForInput),
        ];
        assert_eq!(
            sidebar_agent_summary(sessions.iter(), false),
            Some(SidebarAgentSummary {
                state: SidebarAgentState::NeedsInput,
                count: 2
            })
        );
    }

    #[test]
    fn sidebar_agent_summary_applies_sidebar_priority() {
        let cases = [
            (
                vec![AgentState::Finished, AgentState::Thinking],
                SidebarAgentState::Thinking,
            ),
            (
                vec![AgentState::Thinking, AgentState::Stalled],
                SidebarAgentState::Stalled,
            ),
            (
                vec![AgentState::Stalled, AgentState::Errored],
                SidebarAgentState::Errored,
            ),
            (
                vec![AgentState::Errored, AgentState::WaitingForInput],
                SidebarAgentState::NeedsInput,
            ),
        ];
        for (states, expected) in cases {
            let sessions: Vec<_> = states.into_iter().map(session).collect();
            assert_eq!(
                sidebar_agent_summary(sessions.iter(), false).map(|summary| summary.state),
                Some(expected)
            );
        }
    }

    #[test]
    fn sidebar_agent_summary_surfaces_unread_completion_without_live_session() {
        assert_eq!(
            sidebar_agent_summary(std::iter::empty(), true),
            Some(SidebarAgentSummary {
                state: SidebarAgentState::Finished,
                count: 1
            })
        );
    }

    #[test]
    fn sidebar_agent_summary_hides_acknowledged_finished_session() {
        let sessions = [session(AgentState::Finished)];
        assert_eq!(sidebar_agent_summary(sessions.iter(), false), None);
    }

    #[test]
    fn tab_display_title_falls_back_to_position() {
        // US-009: a tab is unnamed until it is renamed or created from a named
        // preset, and a blank sidebar row would be unclickable in practice.
        let unnamed = Tab::new(String::new(), None);
        assert_eq!(tab_display_title(&unnamed, 0), "Tab 1");
        assert_eq!(tab_display_title(&unnamed, 4), "Tab 5");

        let blank = Tab::new("   ".to_string(), None);
        assert_eq!(tab_display_title(&blank, 1), "Tab 2");

        let named = Tab::new("build".to_string(), None);
        assert_eq!(tab_display_title(&named, 3), "build");
    }

    #[gpui::test]
    fn a_manual_tab_title_outranks_the_pane_it_holds(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let pane = titled_test_pane(cx, "claude");
        let tab = Tab::new(
            "build".to_string(),
            Some(crate::layout::LayoutTree::Leaf(pane)),
        );
        cx.update(|_, cx| {
            assert_eq!(
                tab_row_title(&tab, 0, cx),
                "build",
                "a name the user typed must survive whatever the agent renames its terminal to"
            );
        });
    }

    #[gpui::test]
    fn an_unnamed_tab_borrows_its_first_panes_title(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let pane = titled_test_pane(cx, "claude");
        let tab = Tab::new(String::new(), Some(crate::layout::LayoutTree::Leaf(pane)));
        cx.update(|_, cx| {
            let cx: &gpui::App = cx;
            // Asserted against the pane resolver rather than a literal: the
            // OSC precedence is `pane.rs`'s contract, and the rule under test
            // here is only that the rail defers to it instead of keeping a
            // second copy.
            let resolved = tab
                .root
                .as_ref()
                .and_then(|root| root.first_leaf())
                .map(|pane| crate::pane::Pane::surface_title(&pane.read(cx).surface, cx))
                .expect("the tab holds one pane");
            assert!(!resolved.is_empty(), "the resolver always names something");
            assert_eq!(tab_row_title(&tab, 0, cx), resolved);
            assert_ne!(
                tab_row_title(&tab, 0, cx),
                "Tab 1",
                "the positional fallback is the LAST resort, not the default"
            );
        });
    }

    #[gpui::test]
    fn a_paneless_tab_falls_back_to_its_position(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let tab = Tab::new(String::new(), None);
        cx.update(|_, cx| {
            assert_eq!(tab_row_title(&tab, 2, cx), "Tab 3");
        });
    }

    /// A PTY-free pane whose terminal carries `title` the way an agent's
    /// OSC 0/2 would have left it.
    fn titled_test_pane(
        cx: &mut gpui::VisualTestContext,
        title: &str,
    ) -> gpui::Entity<crate::pane::Pane> {
        let terminal = cx.new(|cx| crate::terminal::TerminalView::display_only_for_test(1, cx));
        terminal.update(cx, |view, _| {
            view.terminal.title = title.to_string();
        });
        cx.new(|cx| crate::pane::Pane::new(terminal, 1, cx))
    }

    #[test]
    fn a_tab_of_a_background_workspace_quiets_its_title() {
        assert_eq!(sidebar_tab_title_opacity(true), 1.0);
        assert_eq!(
            sidebar_tab_title_opacity(false),
            IDLE_WORKSPACE_TEXT_OPACITY,
            "the rail has one quiet step; a second value would read as a ranking"
        );
    }

    #[test]
    fn the_inactive_workspace_dim_never_reaches_the_tab_row_fill() {
        // The workspace row has the same guard: the dim is a text treatment,
        // and applied to the shell it would make a whole background workspace
        // look disabled rather than backgrounded.
        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        let tab_row = source_slice(
            production,
            "fn render_tab_row(\n",
            "fn render_workspace_meta_row(\n",
        );
        // Whitespace-stripped, because rustfmt is free to wrap a builder chain
        // and the rule under test is about which binding the dim lands on, not
        // about where the line breaks.
        let compact: String = tab_row.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains(
                "lettext_color=ui.text.opacity(sidebar_tab_title_opacity(is_active_workspace));"
            ),
            "the inactive-workspace dim must land on the title foreground, alone"
        );
        assert!(
            compact.contains("let(resting_bg,hovered_bg)=ifis_active_tab&&is_active_workspace{"),
            "the resting/hover fills stay a function of selection alone"
        );
        assert!(
            !compact.contains("hover_bg.opacity("),
            "dimming the row fill would make a background workspace look unclickable"
        );
    }

    #[test]
    fn a_seeded_rename_is_replaced_by_the_first_key() {
        let mut text = "Tab 1".to_string();
        let mut seeded = true;
        assert!(take_rename_selection(&mut text, &mut seeded));
        text.push('b');
        assert_eq!(text, "b");
        assert!(!seeded, "one edit spends the selection");

        // The second key appends, like any editor.
        assert!(!take_rename_selection(&mut text, &mut seeded));
        text.push_str("uild");
        assert_eq!(text, "build");
    }

    #[test]
    fn a_seeded_rename_paints_a_selection_and_no_caret() {
        let theme = crate::theme::theme_by_name(crate::theme::DEFAULT_THEME)
            .expect("the default theme is bundled");
        let ui = crate::theme::ui_colors_with(&theme);

        let (seeded_bg, seeded_body) = rename_editor_skin("build", true, ui);
        assert_eq!(
            seeded_body, "build",
            "a selected value has no insertion point to mark"
        );
        assert_ne!(
            seeded_bg, ui.overlay,
            "the selection must be visibly distinct from the resting editor fill"
        );

        let (typed_bg, typed_body) = rename_editor_skin("build", false, ui);
        assert_eq!(typed_body, "build|");
        assert_eq!(typed_bg, ui.overlay);
    }

    #[test]
    fn backspace_on_a_seeded_rename_clears_the_whole_value() {
        let mut text = "claude".to_string();
        let mut seeded = true;
        // The caller only pops when the selection was NOT what it consumed;
        // popping as well would leave "claud" behind.
        assert!(take_rename_selection(&mut text, &mut seeded));
        assert_eq!(text, "");
        assert!(!seeded);

        let mut typed = "build".to_string();
        let mut plain = false;
        assert!(!take_rename_selection(&mut typed, &mut plain));
        typed.pop();
        assert_eq!(typed, "buil");
    }

    /// US-012: a session bound to a terminal of the tab, one bound elsewhere,
    /// one never resolved. Only the first may reach the tab row.
    fn attributed_sessions() -> [AgentSession; 3] {
        let mut mine = session(AgentState::WaitingForInput);
        mine.surface_id = Some(11);
        let mut other_tab = session(AgentState::Errored);
        other_tab.surface_id = Some(22);
        let unattributed = session(AgentState::Thinking);
        [mine, other_tab, unattributed]
    }

    #[test]
    fn tab_row_speaks_only_for_the_sessions_of_its_own_surfaces() {
        let sessions = attributed_sessions();
        let surfaces = HashSet::from([11u64]);
        assert_eq!(
            sidebar_agent_summary(tab_row_sessions(sessions.iter(), &surfaces), false),
            Some(SidebarAgentSummary {
                state: SidebarAgentState::NeedsInput,
                count: 1
            }),
            "a tab must not inherit a sibling tab's session, nor an unattributed one"
        );

        // FR-03: a tab with no terminal of its own stays silent even while the
        // workspace is busy.
        assert_eq!(
            sidebar_agent_summary(tab_row_sessions(sessions.iter(), &HashSet::new()), false),
            None
        );
    }

    #[test]
    fn expanded_folder_keeps_only_the_unattributed_sessions() {
        let sessions = attributed_sessions();
        // FR-04: the residue is the `surface_id: None` session, and nothing
        // else - the two resolved ones are spoken for by their tab rows.
        assert_eq!(
            sidebar_agent_summary(folder_row_sessions(sessions.iter(), true), false),
            Some(SidebarAgentSummary {
                state: SidebarAgentState::Thinking,
                count: 1
            })
        );

        // ... and an expanded folder with no residue paints nothing at all.
        let resolved = [attributed_sessions()[0].clone()];
        assert_eq!(
            sidebar_agent_summary(folder_row_sessions(resolved.iter(), true), false),
            None
        );
    }

    #[test]
    fn collapsed_folder_re_aggregates_every_tab() {
        let sessions = attributed_sessions();
        // FR-05: folding hides no state, so the collapsed row falls back to the
        // full precedence over every session, resolved or not.
        assert_eq!(
            sidebar_agent_summary(folder_row_sessions(sessions.iter(), false), false),
            Some(SidebarAgentSummary {
                state: SidebarAgentState::NeedsInput,
                count: 1
            })
        );
    }

    #[test]
    fn a_late_resolution_never_double_counts() {
        // Edge case 6: the two filters partition on `surface_id`, so a session
        // is counted by the folder or by its tab, never by both.
        let mut sessions = attributed_sessions().to_vec();
        let surfaces = HashSet::from([11u64, 33u64]);
        let folder_before = folder_row_sessions(sessions.iter(), true).count();
        let tab_before = tab_row_sessions(sessions.iter(), &surfaces).count();
        assert_eq!((folder_before, tab_before), (1, 1));

        sessions[2].surface_id = Some(33);
        let folder_after = folder_row_sessions(sessions.iter(), true).count();
        let tab_after = tab_row_sessions(sessions.iter(), &surfaces).count();
        assert_eq!((folder_after, tab_after), (0, 2));
        assert_eq!(folder_before + tab_before, folder_after + tab_after);
    }

    #[test]
    fn the_folder_action_lane_holds_both_of_its_buttons() {
        // The folder row reserves this lane as trailing padding at all times,
        // so it is the one geometry that must track the cluster's real width:
        // a lane sized for one button leaves the second one sitting on the
        // agent badge, which is what the reservation exists to prevent.
        let cluster =
            2. * SIDEBAR_ACTION_BUTTON_SIZE + SIDEBAR_ACTION_BUTTON_GAP + SIDEBAR_TITLE_ROW_GAP;
        assert_eq!(
            SIDEBAR_ACTION_LANE_WIDTH, cluster,
            "the reserved lane no longer matches the two-button cluster it holds"
        );
        // A tab row reserves nothing: it hides its pane cluster and drops the
        // button in its place. That only holds while the narrowest cluster - a
        // lone bare glyph - plus the row's gap still clears the button, or the
        // `x` would overhang a title it is not covering for.
        let narrowest_cluster = SIDEBAR_TAB_ICON_SIZE + SIDEBAR_TITLE_ROW_GAP;
        assert!(
            narrowest_cluster >= SIDEBAR_ACTION_BUTTON_SIZE,
            "a tab row's close button now overhangs past its pane cluster onto the title"
        );
    }

    #[test]
    fn tab_card_fits_inside_a_row() {
        // The pane cards are absolute and overhang the title's line height into
        // the row's vertical padding. The row shell clips its own overflow, so
        // a card taller than the whole row paints sliced - which is exactly how
        // an oversized card failed once. This is the bound that keeps it whole.
        let row_height = SIDEBAR_ROW_LINE_HEIGHT + 2. * SIDEBAR_ROW_PADDING_Y;
        assert!(
            SIDEBAR_TAB_CARD_HEIGHT <= row_height,
            "a {SIDEBAR_TAB_CARD_HEIGHT}px card overflows a {row_height}px row and would be clipped"
        );
        // The glyph must leave breathing room on every side, or the mark reads
        // as a framed icon instead of a chip.
        assert!(
            SIDEBAR_TAB_CARD_ICON_SIZE + 8. <= SIDEBAR_TAB_CARD_WIDTH.min(SIDEBAR_TAB_CARD_HEIGHT),
            "a {SIDEBAR_TAB_CARD_ICON_SIZE}px glyph leaves under 4px of padding in the card"
        );
        // ... and the leftover must split into whole pixels on both axes, or
        // rounding lands the glyph off center by half a pixel.
        for side in [SIDEBAR_TAB_CARD_WIDTH, SIDEBAR_TAB_CARD_HEIGHT] {
            let gap = side - SIDEBAR_TAB_CARD_ICON_SIZE;
            assert_eq!(
                gap % 2.,
                0.,
                "a {gap}px gap around the glyph centers it on a half pixel"
            );
        }
    }

    #[test]
    fn tab_icon_cluster_caps_at_four_panes() {
        // US-013: up to the cap every pane gets its own slot; past it the tail
        // folds into a single `+N`.
        assert_eq!(tab_icon_cluster_split(0), (0, 0));
        assert_eq!(tab_icon_cluster_split(1), (1, 0));
        assert_eq!(
            tab_icon_cluster_split(SIDEBAR_TAB_ICON_CAP),
            (SIDEBAR_TAB_ICON_CAP, 0)
        );
        assert_eq!(
            tab_icon_cluster_split(SIDEBAR_TAB_ICON_CAP + 3),
            (SIDEBAR_TAB_ICON_CAP, 3)
        );
    }

    /// Every modifier off: the state an unmodified letter arrives with.
    fn bare_modifiers() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn rename_key_action_maps_the_editor_keys() {
        assert_eq!(
            rename_key_action("enter", None, bare_modifiers()),
            RenameKey::Commit
        );
        assert_eq!(
            rename_key_action("escape", None, bare_modifiers()),
            RenameKey::Cancel
        );
        assert_eq!(
            rename_key_action("backspace", None, bare_modifiers()),
            RenameKey::Backspace
        );
        assert_eq!(
            rename_key_action("a", Some("a"), bare_modifiers()),
            RenameKey::Insert("a".to_string())
        );
    }

    #[test]
    fn rename_key_action_ignores_held_modifiers_and_empty_chars() {
        // Issue #79: without the `alt` arm, macOS Option+key inserts the dead
        // key it composes (Option+e is a combining acute), not a name.
        for (label, mods) in [
            (
                "control",
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                "platform",
                Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
            ),
            (
                "alt",
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            ),
        ] {
            assert_eq!(
                rename_key_action("a", Some("a"), mods),
                RenameKey::Ignore,
                "{label} held must not type into the rename editor"
            );
        }

        // Shift is not a suppressing modifier - it is how a capital arrives.
        assert_eq!(
            rename_key_action(
                "a",
                Some("A"),
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                }
            ),
            RenameKey::Insert("A".to_string())
        );

        // A key with no printable character, and a key whose character is the
        // empty string, are both nothing to insert.
        assert_eq!(
            rename_key_action("f5", None, bare_modifiers()),
            RenameKey::Ignore
        );
        assert_eq!(
            rename_key_action("left", Some(""), bare_modifiers()),
            RenameKey::Ignore
        );
    }

    #[test]
    fn both_sidebar_rename_rows_focus_and_share_the_key_decision() {
        // Issue #79: GPUI only walks a key event down the dispatch path to the
        // FOCUSED node. A sidebar row that tracks no focus handle is never on
        // that path, so every branch of its `on_key_down` is dead code. This
        // reads the module's own source because `PaneFlowApp` cannot be built
        // in a unit test (bootstrap opens a window and does real I/O), so the
        // rendered element tree is not reachable from here.
        let src = include_str!("mod.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        // The trailing `(\n` matters: `fn render_workspace_row` is also a
        // prefix of `fn render_workspace_rows`, the plural that renders the
        // whole rail, and slicing on the bare name lands in the wrong body.
        let workspace_row = source_slice(
            production,
            "fn render_workspace_row(\n",
            "fn render_tab_row(\n",
        );
        let tab_row = source_slice(
            production,
            "fn render_tab_row(\n",
            "fn render_workspace_meta_row(\n",
        );

        for (label, body) in [("workspace row", workspace_row), ("tab row", tab_row)] {
            assert!(
                body.contains("track_focus(&self.sidebar_rename_focus)"),
                "{label} must track the rename focus handle, or its on_key_down is unreachable"
            );
            assert!(
                body.contains("rename_key_action("),
                "{label} must route its key decision through the shared pure function"
            );
            // Issue #79 meets issue #108: the renamed row is the only element
            // tracking the shared handle, and it stops tracking it the instant
            // the rename state clears. Ending an edit without handing focus
            // back leaves the window with nothing focused, which silently kills
            // every global `context: None` binding.
            assert!(
                body.contains("restore_focus_after_rename(window, cx)"),
                "{label} must hand focus back when its rename ends"
            );
        }
    }

    /// Issue #105: the "Workspaces" header no longer carries a `+`. Creating
    /// a workspace already has four other entry points (`Cmd+Shift+N`, the
    /// Window menu, the profile menu, and the empty-state "Open folder"
    /// button), and the header glyph was the redundant fifth. Read from
    /// source because `PaneFlowApp` cannot be built in a unit test (bootstrap
    /// opens a window and does real I/O), so the rendered element tree is
    /// unreachable from here.
    #[test]
    fn the_workspaces_header_carries_no_new_workspace_button() {
        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        assert!(
            !production.contains("sidebar-new-workspace"),
            "the sidebar `+` button is back; issue #105 removed it"
        );
        // The empty-state open-folder button is a different affordance on a
        // different surface, and deliberately stays.
        assert!(
            production.contains("empty-new-ws"),
            "the empty-state open-folder button must survive the `+` removal"
        );
    }

    /// Workspace folder rows show the detected git branch under the title.
    /// Issue #88's tab-level redesign dropped it as "crowding"; without it,
    /// sibling worktrees of one repo are hard to tell apart in the rail.
    #[test]
    fn the_workspace_meta_row_renders_the_git_branch() {
        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        let meta = source_slice(
            production,
            "fn render_workspace_meta_row(",
            "\n    pub(crate) fn ",
        );
        assert!(
            meta.contains("icons/git-branch-sidebar.svg"),
            "workspace meta row must paint the git branch icon"
        );
        assert!(
            meta.contains("ws.git_branch"),
            "workspace meta row must read Workspace::git_branch"
        );
        assert!(
            !meta.contains("Neither the git branch nor a diffstat"),
            "stale comment still claims the branch was dropped from the rail"
        );
    }

    #[test]
    fn both_rename_entry_points_claim_the_rename_focus_handle() {
        // Issue #79 review (I1): the row *renderers* are covered above, but
        // the two functions that START a rename are not - and the focus claim
        // lives in them, not in the renderers. `begin_workspace_rename` sits
        // above the slice the renderer test reads, and `begin_tab_rename`
        // lives in another module no test read at all, so deleting both
        // `sidebar_rename_focus.focus(window, cx)` lines left every existing
        // test green with the feature 100% dead: the editor draws, and GPUI
        // hands its `on_key_down` nothing because the row is not on the
        // dispatch path to the focused node.
        let sidebar = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the sidebar module");
        let begin_workspace_rename =
            source_slice(sidebar, "fn begin_workspace_rename(\n", "\n    }");
        let tab_ops = include_str!("../workspace_ops/tab.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the tab-ops module");
        let begin_tab_rename = source_slice(tab_ops, "fn begin_tab_rename(\n", "\n    }");

        for (label, body) in [
            ("begin_workspace_rename", begin_workspace_rename),
            ("begin_tab_rename", begin_tab_rename),
        ] {
            assert!(
                body.contains("self.sidebar_rename_focus.focus(window, cx);"),
                "{label} must claim the rename focus handle, or the editor it opens is drawn but receives no keys"
            );
        }
    }

    #[test]
    fn tab_rename_uses_the_visible_title_and_commits_on_click_outside() {
        // Issue #80: an unnamed tab displays "Tab N", so seeding the editor
        // from the raw empty `Tab::title` opens as a bare cursor. The same
        // editor must also settle when the next mouse press lands elsewhere;
        // otherwise the typed buffer remains stranded on an unfocused row.
        let tab_ops = include_str!("../workspace_ops/tab.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the tab-ops module");
        let begin_tab_rename = source_slice(tab_ops, "fn begin_tab_rename(\n", "\n    }");
        assert!(
            begin_tab_rename.contains("tab_row_title(tab, tab_idx, cx)"),
            "the rename buffer must seed from the title the sidebar actually displays - which is \
             the DERIVED row label, not the raw `Tab::title`"
        );

        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the sidebar module");
        let tab_row = source_slice(
            production,
            "fn render_tab_row(\n",
            "fn render_workspace_meta_row(\n",
        );
        assert!(
            tab_row.contains("on_mouse_down_out(cx.listener(|this, _, window, cx|")
                && tab_row.contains("this.commit_inline_rename(window, cx);"),
            "a mouse press outside the renamed tab row must commit the typed buffer"
        );
    }

    #[test]
    fn the_workspace_row_commits_its_rename_before_it_dismisses_anything() {
        // Issue #79 review (I2): the single-click branch's ordering is load
        // bearing twice over. `was_renaming` has to be read before
        // `commit_rename` clears `renaming_idx`, or a click that ends an edit
        // also folds the row shut; and the commit has to run before the
        // dismiss, or the click that is meant to keep the typed name throws it
        // away instead. Both orderings are invisible to every other test - a
        // contributor moving the dismiss back to the top of the handler would
        // pass the whole suite and silently re-break double-click-to-rename.
        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        let workspace_row = source_slice(
            production,
            "fn render_workspace_row(\n",
            "fn render_tab_row(\n",
        );

        let capture = workspace_row
            .find("let was_renaming")
            .expect("the single-click branch must capture whether this row was being renamed");
        let commit = workspace_row
            .find("this.commit_rename(cx);")
            .expect("the single-click branch must commit the live rename");
        assert!(
            capture < commit,
            "`was_renaming` must be read BEFORE `commit_rename` clears `renaming_idx`, \
             or the click that ends a rename also toggles the row's disclosure"
        );

        let single_click = &workspace_row[capture..];
        let commit_in_branch = single_click
            .find("this.commit_rename(cx);")
            .expect("the single-click branch must commit the live rename");
        let dismiss_in_branch = single_click
            .find("this.dismiss_transient_surfaces();")
            .expect("the single-click branch must dismiss the open popovers after committing");
        assert!(
            commit_in_branch < dismiss_in_branch,
            "the single-click branch must commit the rename before it dismisses anything, \
             or the click meant to end the edit discards the typed name"
        );
    }

    #[test]
    fn only_the_window_taking_rename_ender_clears_the_focus_tracking_state() {
        // Issue #79 review (C1): `dismiss_transient_surfaces` takes no
        // `Window`, and ~17 call sites rely on that - several of them (the
        // title-bar sidebar-collapse and menu toggles, the IPC/CLI
        // `workspace.select` path) have no `Window` to give it. The rename
        // fields track focus: the renamed row is the only element that tracks
        // `sidebar_rename_focus`, and it stops the instant they clear. Clearing
        // them from a `Window`-less caller therefore leaves the window with
        // NOTHING focused - the issue #108 state where every global
        // `context: None` binding matches but finds no handler, with no
        // recovery because this app registers no focus-lost listeners.
        //
        // Same constraint as the tests above: there is no `PaneFlowApp` to call
        // these on, so assert the seam that IS reachable - the bodies as
        // written.
        let src = include_str!("../workspace_ops/mod.rs");
        let dismiss = source_slice(src, "pub(crate) fn dismiss_transient_surfaces", "\n    }");
        for field in ["renaming_idx", "renaming_tab", "rename_text"] {
            assert!(
                !dismiss.contains(field),
                "dismiss_transient_surfaces must not touch `{field}`: it has no `Window` to hand \
                 focus back with, so clearing focus-tracking state here strands the whole window"
            );
        }

        let cancel = source_slice(src, "pub(crate) fn cancel_inline_rename(", "\n    }");
        for clear in [
            "self.renaming_idx = None;",
            "self.renaming_tab = None;",
            "self.rename_text.clear();",
        ] {
            assert!(
                cancel.contains(clear),
                "cancel_inline_rename must run `{clear}` - it is the one path that ends a rename \
                 without keeping the name"
            );
        }
        assert!(
            cancel.contains("self.restore_focus_after_rename(window, cx);"),
            "cancel_inline_rename must hand focus back, or it is just the stranding bug again"
        );
    }

    fn order_key(pinned: bool, active: bool, title: &str, repo: Option<&str>) -> WorkspaceOrderKey {
        WorkspaceOrderKey {
            pinned,
            active,
            title_lower: title.to_lowercase(),
            repo_root: repo.map(std::path::PathBuf::from),
        }
    }

    /// Issue #107: the Auto buckets, in order. Titles are deliberately the
    /// reverse of the expected order so only the bucket can be producing it.
    #[test]
    fn auto_order_puts_pinned_before_active_before_idle() {
        let keys = [
            order_key(false, false, "a", None),
            order_key(false, true, "b", None),
            order_key(true, false, "c", None),
        ];
        assert_eq!(compute_auto_order(&keys), vec![2, 1, 0]);
    }

    #[test]
    fn auto_order_sorts_case_insensitively_inside_a_bucket() {
        let keys = [
            order_key(false, true, "gamma", None),
            order_key(false, true, "Beta", None),
            order_key(false, true, "alpha", None),
        ];
        assert_eq!(compute_auto_order(&keys), vec![2, 1, 0]);
    }

    /// R-D2: a sibling-worktree group travels as one block. Its bucket is its
    /// strongest member, its sort title is its FIRST member in storage order,
    /// and members keep storage order inside it.
    #[test]
    fn auto_order_keeps_worktree_siblings_contiguous() {
        let keys = [
            order_key(false, true, "zeta", Some("/repo")),
            order_key(false, true, "alpha", None),
            order_key(false, false, "beta", Some("/repo")),
        ];
        // The group sorts under "zeta" (its first member), not "beta", so the
        // lone "alpha" leads - and the idle sibling still rides with the group.
        assert_eq!(compute_auto_order(&keys), vec![1, 0, 2]);
    }

    #[test]
    fn auto_order_lifts_a_worktree_group_by_its_strongest_member() {
        let keys = [
            order_key(false, false, "zeta", Some("/repo")),
            order_key(false, true, "alpha", None),
            order_key(true, false, "beta", Some("/repo")),
        ];
        // One pinned member pins the whole group, so the idle "zeta" outranks
        // the active "alpha" - without that, pinning a worktree would tear its
        // siblings to the bottom of the rail.
        assert_eq!(compute_auto_order(&keys), vec![0, 2, 1]);
    }

    #[test]
    fn auto_order_is_stable_for_equal_keys() {
        let keys = [
            order_key(false, true, "same", None),
            order_key(false, false, "zzz", None),
            order_key(false, true, "same", None),
        ];
        assert_eq!(compute_auto_order(&keys), vec![0, 2, 1]);
    }

    /// The memoization trap: the rail repaints from a cached order, so a
    /// signature that ignores a field means that field can never reorder the
    /// rail. Every input the two order functions read has to be in the hash.
    #[test]
    fn order_signature_changes_with_every_input_the_order_depends_on() {
        let base = [order_key(false, true, "alpha", Some("/repo"))];
        let sig = PaneFlowApp::sidebar_order_signature(&base, false);

        assert_eq!(
            sig,
            PaneFlowApp::sidebar_order_signature(&base, false),
            "the signature must be deterministic"
        );
        assert_ne!(
            sig,
            PaneFlowApp::sidebar_order_signature(
                &[order_key(true, true, "alpha", Some("/repo"))],
                false
            ),
            "a pin flip must repaint the rail"
        );
        assert_ne!(
            sig,
            PaneFlowApp::sidebar_order_signature(
                &[order_key(false, true, "beta", Some("/repo"))],
                false
            ),
            "a rename must repaint the rail"
        );
        assert_ne!(
            sig,
            PaneFlowApp::sidebar_order_signature(
                &[order_key(false, false, "alpha", Some("/repo"))],
                false
            ),
            "an activity change must repaint the rail"
        );
        assert_ne!(
            sig,
            PaneFlowApp::sidebar_order_signature(
                &[order_key(false, true, "alpha", Some("/other"))],
                false
            ),
            "a repo root change must repaint the rail"
        );
        assert_ne!(
            sig,
            PaneFlowApp::sidebar_order_signature(&base, true),
            "switching Manual to Auto must repaint the rail"
        );
    }

    /// R6: under Auto the folder drop targets are gone, because `slot.workspace`
    /// is a storage index and Auto breaks display-order == storage-order. Tab
    /// targets are untouched - only folder drags are disabled.
    #[test]
    fn drop_slots_offer_no_workspace_target_under_auto_sort() {
        let rows = [
            SidebarRow::Folder(0),
            SidebarRow::Tab(0, 0),
            SidebarRow::Folder(1),
        ];
        let manual = sidebar_drop_slots(&rows, 2, false);
        let auto = sidebar_drop_slots(&rows, 2, true);

        assert!(manual.iter().any(|slot| slot.workspace.is_some()));
        assert_eq!(auto.len(), rows.len() + 1);
        assert!(auto.iter().all(|slot| slot.workspace.is_none()));
        assert_eq!(
            auto.iter().map(|slot| slot.tab).collect::<Vec<_>>(),
            manual.iter().map(|slot| slot.tab).collect::<Vec<_>>(),
        );
    }

    /// Issue #340: the rail's hover actions (new pane, close workspace, close
    /// tab) had tooltips but no button role and no accessible name, so a
    /// screen reader could not find them at all. The primitive now owns the
    /// whole recipe - one `label` feeds `Role::Button`, `aria_label` and the
    /// tooltip - and every caller activates on `on_click`, the only activation
    /// AccessKit exposes.
    #[test]
    fn sidebar_action_buttons_are_accessible_named_buttons() {
        let source = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production sidebar source");
        let body = source_slice(source, "fn sidebar_action_button(", "\n}\n");
        for needle in [
            "label: SharedString,",
            ".role(Role::Button)",
            ".aria_label(label.clone())",
            ".delayed_tooltip(",
            "SidebarTooltip {",
        ] {
            assert!(
                body.contains(needle),
                "sidebar_action_button lost `{needle}`; its label must feed the role, \
                 the accessible name and the tooltip"
            );
        }
        for id in ["ws-new-tab-", "ws-close-", "tab-close-"] {
            let id_anchor = format!("format!(\"{id}{{");
            let at = source
                .find(&id_anchor)
                .unwrap_or_else(|| panic!("the sidebar builds the `{id}` action"));
            let lead = source[..at]
                .rsplit("sidebar_action_button(")
                .next()
                .expect("rsplit always yields");
            assert_eq!(
                lead.trim(),
                "SharedString::from(",
                "the `{id}` action must be built through sidebar_action_button, which names it"
            );
            let chain = source_slice(&source[at..], &id_anchor, ".on_click(");
            assert!(
                !chain.contains(".on_mouse_down("),
                "the `{id}` action must activate on on_click, not on_mouse_down"
            );
        }
    }
}
