//! The rail header's "Customize Sidebar" button and the popover it opens
//! (issue #349).
//!
//! Same shape as the diff dock's overflow menu
//! ([`crate::app::diff_dock::options_menu`]), which is itself modeled on
//! Cursor's changes-panel menu: a row carrying a side submenu, opened from a
//! header button skinned like the rail's other header action.
//!
//! The submenu is a set of independent checks rather than a "compact /
//! detailed" pair, because that is what the menu is: one switch per thing a
//! row shows. The defaults are the rail before the menu existed - the branch
//! on, everything else off - which is what a fresh install and a
//! `paneflow.json` without `sidebar_show` get.
//!
//! Not a Settings entry point: issue #105 keeps every Settings affordance out
//! of the sidebar, and this menu only writes the `sidebar_show` object and
//! folds rows.

use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseUpEvent,
    ParentElement, StatefulInteractiveElement, Styled, deferred, div, prelude::FluentBuilder, px,
    svg,
};

use crate::PaneFlowApp;
use crate::settings::components::{menu_divider_color, menu_surface, select_item};

use super::{fold_all_target, sidebar_action_button};
use paneflow_config::schema::SidebarShow;

/// Accessible name and tooltip of the header trigger: one string feeds both.
const CUSTOMIZE_LABEL: &str = "Customize Sidebar";

/// What the popover needs to paint itself, read once by the caller while it
/// still holds `&self`. A render helper cannot read the app entity back
/// through `cx` - it is already mutably borrowed by the render pass.
#[derive(Clone, Copy)]
pub(super) struct CustomizeMenuState {
    pub open: bool,
    pub submenu_open: bool,
    pub show: SidebarShow,
    /// The fold state the Expand / Collapse row will apply, which is also
    /// what decides its wording: `Some(true)` is "Expand all", `Some(false)`
    /// is "Collapse all". `None` when no workspace has rows to fold: the row
    /// is then hidden rather than offering an action with nothing to act on.
    pub fold_target: Option<bool>,
}

/// Width of the popover. Sized on its longest row, "Collapse all", with room
/// for the labels a second section would add later.
const MENU_WIDTH: f32 = 208.0;
/// Width of the "Show" submenu. Holds "Indent guide" and its check without the
/// flyout overhanging more of the main panel than it must.
const SUBMENU_WIDTH: f32 = 156.0;

impl PaneFlowApp {
    /// Close the Customize Sidebar popover, folding its submenu with it so the
    /// menu always reopens collapsed.
    pub(crate) fn close_sidebar_customize_menu(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_customize_menu_open {
            self.sidebar_customize_menu_open = false;
            self.sidebar_show_submenu_open = false;
            cx.notify();
        }
    }

    /// The fold state Expand all / Collapse all would apply, from every
    /// workspace that has rows to show. See [`fold_all_target`].
    pub(super) fn sidebar_fold_all_target(&self) -> Option<bool> {
        fold_all_target(
            self.workspaces
                .iter()
                .map(|ws| (!ws.is_empty_shell(), ws.sidebar_expanded)),
        )
    }

    /// Flip one of the rail's optional lines and persist the whole object.
    /// All four keys are written, because [`crate::config_writer`] merges by
    /// top-level key and would otherwise drop the siblings - and because a
    /// `branch` turned off has to land as an explicit `false`, its absent
    /// value meaning `true` (issue #349).
    fn toggle_sidebar_show(&mut self, line: SidebarShowLine, cx: &mut Context<Self>) {
        // Hold the menu open across the flip. The popover's `on_mouse_up_out`
        // runs in the capture phase of this very release - the submenu is
        // outside the parent menu's bounds, so a click on a check reads as
        // "outside" and closes it before this bubble handler runs. Turning two
        // lines on has to be one trip through the menu, not two, and the rail
        // behind the popover already shows each flip as it happens.
        self.sidebar_customize_menu_open = true;
        self.sidebar_show_submenu_open = true;

        let mut show = self.cached_config.sidebar_show;
        match line {
            SidebarShowLine::Branch => show.branch = Some(!show.branch_enabled()),
            SidebarShowLine::Diffstat => show.diffstat = Some(!show.diffstat_enabled()),
            SidebarShowLine::Pr => show.pr = Some(!show.pr_enabled()),
            SidebarShowLine::IndentGuide => {
                show.indent_guide = Some(!show.indent_guide_enabled());
            }
        }
        let value = serde_json::json!({
            "branch": show.branch_enabled(),
            "diffstat": show.diffstat_enabled(),
            "pr": show.pr_enabled(),
            "indent_guide": show.indent_guide_enabled(),
        });
        if !crate::config_writer::save_config_values_checked([("sidebar_show", value)]) {
            self.show_toast("Could not save the sidebar setting", cx);
            return;
        }
        // The file write comes back through the config watcher on a later
        // tick; the switch has to show its effect on this frame.
        self.cached_config.sidebar_show = show;
        // Read the pull requests now rather than at the next git tick: a
        // switch whose effect appears half a minute later reads as broken.
        // A no-op while `pr` is off (issue #350).
        self.refresh_pull_requests(cx);
        cx.notify();
    }

    /// Fold or unfold every workspace at once (issue #349).
    ///
    /// One row rather than two: at any moment only one of the two is worth
    /// doing, and the label names that one. The folds are persisted like a
    /// per-row chevron's (`WorkspaceSession::sidebar_collapsed`).
    ///
    /// The menu stays up, and the row re-labels itself under the pointer: the
    /// rail behind the popover shows each fold as it happens, so folding and
    /// unfolding again is one trip through the menu. Held open the way
    /// [`Self::toggle_sidebar_show`] holds it, for the same capture-phase
    /// reason.
    fn set_all_workspaces_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        self.sidebar_customize_menu_open = true;
        for ws in &mut self.workspaces {
            ws.sidebar_expanded = expanded;
        }
        self.save_session(cx);
        cx.notify();
    }
}

/// The lines the "Show" submenu switches.
#[derive(Clone, Copy)]
enum SidebarShowLine {
    Branch,
    Diffstat,
    Pr,
    IndentGuide,
}

/// The header trigger, with the popover deferred over it while open. Skinned
/// like its Pane Overview neighbor; while the menu is up the hover fill is
/// pinned on as the resting fill so the trigger stays lit under the popover.
pub(super) fn render_customize_sidebar_button(
    state: CustomizeMenuState,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let open = state.open;
    let lit = crate::app::constants::sidebar_tab_active_background();

    sidebar_action_button(
        "sidebar-customize".into(),
        CUSTOMIZE_LABEL.into(),
        "icons/filter-2.svg",
        12.,
        ui,
    )
    .relative()
    .when(open, |trigger| trigger.bg(lit))
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    // Toggle off the render-time `open` snapshot, not the live state: while the
    // menu is up, its `on_mouse_up_out` fires on this same release and has
    // already cleared the flag, so a live toggle would re-open it and a second
    // press on the trigger could never close the menu.
    .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
        this.sidebar_customize_menu_open = !open;
        if open {
            this.sidebar_show_submenu_open = false;
        }
        cx.notify();
        cx.stop_propagation();
    }))
    .when(open, |trigger| trigger.child(render_menu(state, ui, cx)))
    .into_any_element()
}

fn render_menu(
    state: CustomizeMenuState,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    // `menu_surface` rather than `select_menu`: the latter bakes in an
    // `overflow_y_scroll` host, and the submenu has to fly out of this menu's
    // own box.
    let menu = menu_surface(div().id("sidebar-customize-menu"), ui)
        .flex()
        .flex_col()
        .gap(px(1.))
        .p(px(4.))
        .w(px(MENU_WIDTH))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        // Dismiss on release, not on press: `on_mouse_down_out` runs in the
        // capture phase (no bubble `stop_propagation` holds it back) and the
        // submenu flies out of this menu's bounds, so a press on a submenu row
        // read as "outside", closed the menu, and the row's click died with the
        // frame. Closing on mouse-up keeps that click alive, because the
        // capture close and the bubble click share one event.
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this, _: &MouseUpEvent, _w, cx| {
                this.close_sidebar_customize_menu(cx);
            }),
        )
        .child(render_show_row(state, ui, cx))
        .when_some(state.fold_target, |menu, target| {
            menu.child(
                div()
                    .mx(px(6.))
                    .my(px(4.))
                    .h(px(1.))
                    .bg(menu_divider_color(ui)),
            )
            .child(render_fold_all_row(target, ui, cx))
        });

    // Hangs under the trigger's right edge: the trigger sits at the rail's
    // right edge, so a left-anchored menu would overhang the main panel.
    deferred(
        div()
            .absolute()
            .top(px(26.))
            .right(px(0.))
            .occlude()
            .child(menu),
    )
    .with_priority(3)
    .into_any_element()
}

/// "Collapse all" while everything is open, "Expand all" otherwise: the label
/// is the action, not the state. No leading icon - the rows above it use that
/// column for what a switch is about, and this row is about the rail itself.
fn render_fold_all_row(
    target: bool,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    select_item("sidebar-customize-fold-all", false, ui)
        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
            this.set_all_workspaces_expanded(target, cx);
        }))
        .child(menu_label(
            if target { "Expand all" } else { "Collapse all" },
            ui,
        ))
        .into_any_element()
}

/// "Show  >": the row that opens the side submenu. No value on the right - what
/// is on is what the submenu's checks say, and a "Branch, Diffstat" summary
/// here would restate the checks one row away from them.
fn render_show_row(
    state: CustomizeMenuState,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let submenu_open = state.submenu_open;
    select_item("sidebar-customize-show", submenu_open, ui)
        .relative()
        .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
            this.sidebar_show_submenu_open = !this.sidebar_show_submenu_open;
            cx.notify();
        }))
        .child(menu_label("Show", ui))
        .child(
            svg()
                .size(px(13.))
                .flex_none()
                .path("icons/chevron-right.svg")
                .text_color(ui.muted),
        )
        .when(submenu_open, |row| {
            row.child(render_show_submenu(state.show, ui, cx))
        })
        .into_any_element()
}

/// The submenu flies out to the right: the rail is the window's left edge, so
/// the dock's leftward flyout would land off screen here.
fn render_show_submenu(
    show: SidebarShow,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let menu = menu_surface(div().id("sidebar-show-submenu"), ui)
        .flex()
        .flex_col()
        .gap(px(1.))
        .p(px(4.))
        .w(px(SUBMENU_WIDTH))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(render_show_option(
            "Branch",
            "icons/git-branch-sidebar.svg",
            SidebarShowLine::Branch,
            show.branch_enabled(),
            ui,
            cx,
        ))
        .child(render_show_option(
            "Diffstat",
            "icons/plus-minus.svg",
            SidebarShowLine::Diffstat,
            show.diffstat_enabled(),
            ui,
            cx,
        ))
        .child(render_show_option(
            "PR",
            "icons/git-pull-request.svg",
            SidebarShowLine::Pr,
            show.pr_enabled(),
            ui,
            cx,
        ))
        .child(render_show_option(
            "Indent guide",
            "icons/list.svg",
            SidebarShowLine::IndentGuide,
            show.indent_guide_enabled(),
            ui,
            cx,
        ));

    deferred(
        div()
            .absolute()
            .top(px(-5.))
            .left(px(MENU_WIDTH - 12.))
            .occlude()
            .child(menu),
    )
    .with_priority(4)
    .into_any_element()
}

/// One switch. Clicking it flips the line and leaves the menu up - see
/// [`PaneFlowApp::toggle_sidebar_show`] for why that takes an explicit hold.
fn render_show_option(
    label: &'static str,
    icon: &'static str,
    line: SidebarShowLine,
    checked: bool,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let id = match line {
        SidebarShowLine::Branch => "sidebar-show-branch",
        SidebarShowLine::Diffstat => "sidebar-show-diffstat",
        SidebarShowLine::Pr => "sidebar-show-pr",
        SidebarShowLine::IndentGuide => "sidebar-show-indent-guide",
    };

    select_item(id, false, ui)
        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
            this.toggle_sidebar_show(line, cx);
        }))
        .child(
            svg()
                .size(px(13.))
                .flex_none()
                .path(icon)
                .text_color(ui.muted),
        )
        .child(menu_label(label, ui))
        // The check's slot is reserved whether or not it is drawn, so
        // unchecking a line never shifts the label beside it.
        .child(div().w(px(14.)).flex_none().child(if checked {
            svg()
                .size(px(13.))
                .path("icons/check.svg")
                .text_color(ui.text)
                .into_any_element()
        } else {
            div().size(px(13.)).into_any_element()
        }))
        .into_any_element()
}

fn menu_label(label: &'static str, ui: crate::theme::UiColors) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .whitespace_nowrap()
        .text_size(px(13.))
        .text_color(ui.text)
        .child(label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use crate::source_probe::source_slice;

    /// Issue #105 meets issue #349: the menu is not a back door to Settings.
    /// It writes one config object and folds rows, and nothing in it opens
    /// the settings surface.
    #[test]
    fn the_customize_menu_offers_no_settings_affordance() {
        let production = include_str!("customize_menu.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        for needle in ["open_settings", "OpenSettings", "settings_window"] {
            assert!(
                !production.contains(needle),
                "the Customize Sidebar menu must not reach Settings (`{needle}`)"
            );
        }
        assert!(
            !production.contains("sidebar-new-workspace"),
            "issue #105 removed the header `+`; the menu must not bring it back"
        );
    }

    /// Every flip writes all four keys: `branch` defaults to `true` when
    /// absent, so turning it off has to land as an explicit `false`, and the
    /// writer merges by top-level key, so a partial object would drop the
    /// siblings.
    #[test]
    fn every_flip_writes_the_whole_sidebar_show_object() {
        let production = include_str!("customize_menu.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the module");
        let toggle = source_slice(production, "fn toggle_sidebar_show(", "\n    }\n");
        for key in [
            "\"branch\": show.branch_enabled(),",
            "\"diffstat\": show.diffstat_enabled(),",
            "\"pr\": show.pr_enabled(),",
            "\"indent_guide\": show.indent_guide_enabled(),",
        ] {
            assert!(
                toggle.contains(key),
                "toggle_sidebar_show must write `{key}`"
            );
        }
        assert!(
            toggle.contains("save_config_values_checked([(\"sidebar_show\", value)])"),
            "the object is persisted through config_writer's read-modify-write"
        );
    }
}
