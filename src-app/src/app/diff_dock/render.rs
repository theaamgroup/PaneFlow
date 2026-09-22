//! Free render helpers for the diff dock chrome: the resize handle, the
//! toolbar toggle button, the tab strip, the files toolbar, and the
//! empty/loading/error placeholder. The body (the shared `DiffElement`) and the
//! panel orchestration live on `PaneFlowApp` in [`super`].

use gpui::{
    AnyElement, ClickEvent, Context, CursorStyle, FontWeight, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Role, SharedString,
    StatefulInteractiveElement, Styled, Window, div, img, prelude::FluentBuilder, px, svg,
};

use super::model::{DiffChrome, DiffDockTab};
use super::new_tab_menu::render_diff_new_tab_menu;
use super::options_menu::render_diff_options_button;
use crate::PaneFlowApp;
use crate::settings::components::with_alpha;
use crate::ui_primitives::{
    AnimatedHoverExt, ROW_RADIUS, TooltipDelayExt, squircle_skin, text_tooltip,
};

/// Accessible name and tooltip of the tab strip's `+` (issue #340: one string
/// feeds both).
const NEW_TAB_LABEL: &str = "New tab";

/// The thin, column-resize hit target straddling the panel's left border.
/// Captures the drag anchor `(cursor_x, width_at_grab)`; the resize math runs
/// in the CLI dock wrapper's `on_mouse_move` (a wide capture surface, so the
/// drag survives the cursor leaving the dock), which supplies each frame's
/// ceiling itself.
pub(super) fn render_diff_resize_handle(
    width: f32,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    div()
        .id("diff-dock-resize")
        .absolute()
        .left(px(-3.))
        .top_0()
        .bottom_0()
        .w(px(7.))
        .cursor(CursorStyle::ResizeLeftRight)
        .animated_hover_bg(with_alpha(ui.text, 0.0), with_alpha(ui.text, 0.06))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _w, cx| {
                // Anchor on the *rendered* width, not the stored preference:
                // while a right rail clamps the dock, a drag must continue from
                // the edge the cursor grabbed instead of jumping to a width
                // that is not on screen.
                this.diff_dock.resize = Some((f32::from(event.position.x), width));
                cx.notify();
            }),
        )
        .into_any_element()
}

/// The dock's tab strip: the "Changes" diff tab when opened, then one tab per
/// terminal opened from the trailing `+` (which opens the surface picker in
/// [`super::new_tab_menu`]). The dock's own close button is pinned right, so it
/// stays reachable from every tab.
pub(super) fn render_diff_tab_strip(
    tabs: &[DiffDockTab],
    active: usize,
    new_tab_menu_open: bool,
    maximized: bool,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let mut strip = div()
        .h(px(40.))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.))
        .px(px(8.))
        .border_b_1()
        .border_color(ui.border);

    for (index, tab) in tabs.iter().enumerate() {
        strip = strip.child(render_diff_tab(tab, index, index == active, ui, cx));
    }

    // Toggle off the render-time snapshot, not the live flag: the open menu's
    // `on_mouse_up_out` fires on this same release and has already cleared it,
    // so a live toggle would re-open the menu on every second press.
    let open = new_tab_menu_open;
    // Same skin as the rail's own `+`: 28 px box, `ROW_RADIUS` superellipse,
    // rail hover tint. While the picker is up the hover fill is pinned on as
    // the resting fill so the trigger stays lit.
    let rail_hover = crate::app::constants::sidebar_tab_hover_background();

    strip
        .child(
            squircle_skin(
                div()
                    .id("diff-dock-tab-new")
                    .role(Role::Button)
                    .aria_label(NEW_TAB_LABEL)
                    .flex_none()
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor(CursorStyle::PointingHand),
                "diff-dock-tab-new-group",
                ROW_RADIUS,
                open.then_some(rail_hover),
                Some(rail_hover),
            )
            .delayed_tooltip(text_tooltip(NEW_TAB_LABEL))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                this.toggle_diff_new_tab_menu(!open, cx);
            }))
            .child(
                svg()
                    .size(px(14.))
                    .flex_none()
                    .path("icons/plus.svg")
                    .text_color(ui.muted),
            )
            .when(open, |trigger| {
                trigger.child(render_diff_new_tab_menu(ui, cx))
            }),
        )
        .child(div().flex_1().min_w_0())
        .child(render_diff_header_icon_button(
            "diff-dock-maximize",
            if maximized {
                "icons/minimize.svg"
            } else {
                "icons/maximize.svg"
            },
            if maximized {
                "Restore dock"
            } else {
                "Maximize dock"
            },
            cx.listener(|this, _: &ClickEvent, window, cx| {
                this.toggle_diff_dock_maximize(window, cx);
            }),
            ui.muted,
        ))
        .child(render_diff_header_icon_button(
            "diff-dock-close",
            "icons/close.svg",
            "Close dock",
            cx.listener(|this, _: &ClickEvent, window, cx| {
                this.close_diff_dock_panel_from_strip(window, cx);
            }),
            ui.muted,
        ))
        .into_any_element()
}

/// One tab chip. The active one carries the raised fill and hairline; the rest
/// stay flat until hovered. Terminal tabs get a trailing close button; the
/// `Changes` tab is permanent and has none.
fn render_diff_tab(
    tab: &DiffDockTab,
    index: usize,
    active: bool,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let (icon, label) = match tab {
        DiffDockTab::Changes => ("icons/plus-minus.svg", "Changes".to_string()),
        DiffDockTab::Terminal(_) => ("icons/terminal.svg", "Terminal".to_string()),
        DiffDockTab::Setup(_) => ("icons/list.svg", "Agent setup".to_string()),
    };
    // The rail's row grammar, verbatim: exactly one chip rests filled (the
    // active one, which then has no hover step), every other stays flat and
    // takes the same fill on hover. No hairline - the rail marks selection with
    // material, not with a drawn border - and the same `ROW_RADIUS`
    // superellipse instead of GPUI's circular `rounded()`.
    let rail_hover = crate::app::constants::sidebar_tab_hover_background();
    let (resting, hovered) = if active {
        (Some(rail_hover), None)
    } else {
        (None, Some(rail_hover))
    };
    let text = if active { ui.text } else { ui.muted };
    let group = SharedString::from(format!("diff-dock-tab-{index}-group"));

    let mut chip = squircle_skin(
        div()
            .id(SharedString::from(format!("diff-dock-tab-{index}")))
            .flex_none()
            .h(px(26.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .cursor(CursorStyle::PointingHand),
        group.clone(),
        ROW_RADIUS,
        resting,
        hovered,
    )
    .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
        this.select_diff_tab(index, cx);
    }))
    .child(file_icon_element(
        icon,
        px(13.),
        if active { ui.muted } else { text },
    ))
    .child(
        div()
            .flex_none()
            .whitespace_nowrap()
            .text_size(crate::ui_primitives::BODY)
            .font_weight(FontWeight::MEDIUM)
            .text_color(text)
            .child(label),
    );

    // Every tab closes, `Changes` included (upstream f587f7fc): the strip
    // that empties hands the dock back to its surface picker.
    //
    // Issue #340: the control is a named button, as the pane header's `x`
    // (issue #83) already is.
    let close_label = "Close tab";
    let mark: AnyElement = svg()
        .size(px(11.))
        .flex_none()
        .path("icons/close.svg")
        .text_color(ui.muted)
        .into_any_element();
    chip = chip.child(
        div()
            .id(SharedString::from(format!("diff-dock-tab-close-{index}")))
            .role(Role::Button)
            .aria_label(close_label)
            .flex_none()
            .size(px(16.))
            .flex()
            .items_center()
            .justify_center()
            // A control nested inside a filled row, like the rail's own
            // hover actions: it keeps a plain 6 px corner (a superellipse
            // this small resolves to a lozenge) and hovers one tint step
            // past the row it sits on, or it would be invisible.
            .rounded(px(6.))
            .animated_hover_bg(
                gpui::transparent_black(),
                crate::app::constants::sidebar_tab_active_background(),
            )
            .delayed_tooltip(text_tooltip(close_label))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                this.request_close_diff_tab(index, cx);
                // The chip underneath selects on click; let the close control
                // own this gesture so the arm it just set survives.
                cx.stop_propagation();
            }))
            .child(mark),
    );

    chip.into_any_element()
}

/// Whether an icon asset carries its own colors.
///
/// The `icons/languages/` set is multi-fill artwork; everything else in the
/// bundle is a single-color glyph meant to be tinted by its caller.
fn icon_is_colored(icon: &str) -> bool {
    icon.starts_with("icons/languages/")
}

/// Paint a file icon on the side of the fence it belongs to.
///
/// A colored asset goes through `img()`; `svg()` would rasterize it to an alpha
/// mask and repaint the whole glyph in `color`, collapsing it to a solid blob.
/// The monochrome fallback still wants the tint, so it keeps the `svg()` path.
/// See `crate::file_icons` for the policy this enforces.
fn file_icon_element(icon: &'static str, size: Pixels, color: Hsla) -> AnyElement {
    if icon_is_colored(icon) {
        img(icon).size(size).flex_none().into_any_element()
    } else {
        svg()
            .size(size)
            .flex_none()
            .path(icon)
            .text_color(color)
            .into_any_element()
    }
}

/// A dock-header control skinned exactly like the sidebar's rail actions: the
/// same 28 px box, the same continuous corner (`ROW_RADIUS` traced by
/// `squircle`, not a circular `rounded()`), and the same hover tint. The dock
/// chrome and the workspace rail are the same control family, so they share one
/// silhouette instead of drifting into two.
///
/// `label` is the button's accessible name and its tooltip (issue #340).
pub(super) fn render_diff_header_icon_button(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    color: Hsla,
) -> AnyElement {
    squircle_skin(
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(label)
            .flex_none()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center(),
        SharedString::from(format!("{id}-group")),
        ROW_RADIUS,
        None,
        Some(crate::app::constants::sidebar_tab_hover_background()),
    )
    .delayed_tooltip(text_tooltip(label))
    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    .on_click(on_click)
    .child(svg().size(px(14.)).flex_none().path(icon).text_color(color))
    .into_any_element()
}

/// The summary row under the tab strip, shown with the `Changes` tab: the scope
/// ("Uncommitted" plus its +/- totals), the branch chip, then the overflow menu
/// pushed to the right edge.
pub(super) fn render_diff_files_toolbar(
    chrome: &DiffChrome<'_>,
    branch_chip: Option<AnyElement>,
    ui: crate::theme::UiColors,
    cx: &mut Context<PaneFlowApp>,
) -> AnyElement {
    let loaded = chrome
        .data
        .as_ref()
        .filter(|d| !d.loading && d.error.is_none());
    let diff = ui.diff_colors();

    let mut row = div()
        .flex_none()
        .h(px(36.))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .border_b_1()
        .border_color(ui.border)
        .child(
            svg()
                .size(px(14.))
                .flex_none()
                .path("icons/file-text.svg")
                .text_color(ui.muted),
        )
        .child(
            div()
                .flex_none()
                .text_size(crate::ui_primitives::BODY)
                .text_color(ui.text)
                .child("Uncommitted"),
        );

    if let Some(data) = loaded {
        row = row
            .child(
                div()
                    .flex_none()
                    .text_size(crate::ui_primitives::BODY)
                    .text_color(diff.added)
                    .child(format!("+{}", data.added)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(crate::ui_primitives::BODY)
                    .text_color(diff.deleted)
                    .child(format!("-{}", data.removed)),
            );
    }

    if let Some(chip) = branch_chip {
        row = row.child(chip);
    }

    row.child(div().flex_1().min_w_0())
        .child(render_diff_options_button(chrome, ui, cx))
        .into_any_element()
}

pub(super) fn diff_panel_centered(
    icon: &'static str,
    label: impl Into<String>,
    ui: crate::theme::UiColors,
) -> AnyElement {
    crate::ui_primitives::panel_empty_state(
        ui,
        Some(icon),
        None,
        label.into(),
        icon == "icons/loader-circle.svg",
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use crate::source_probe::source_slice;

    /// Issue #340: the dock's icon-only controls (the strip's `+`, a tab's
    /// close, the header close) had no button role, no accessible name and no
    /// tooltip. Each chain now carries all three from one label and activates
    /// on `on_click`, the only activation AccessKit exposes.
    #[test]
    fn dock_icon_buttons_are_accessible_named_buttons() {
        let source = include_str!("render.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production dock render source");
        let header = source_slice(
            source,
            "pub(super) fn render_diff_header_icon_button(",
            "\n}\n",
        );
        for needle in [
            "label: &'static str,",
            ".role(Role::Button)",
            ".aria_label(label)",
            ".delayed_tooltip(text_tooltip(label))",
            ".on_click(on_click)",
        ] {
            assert!(
                header.contains(needle),
                "render_diff_header_icon_button lost `{needle}`"
            );
        }
        let new_tab = source_slice(
            source,
            ".id(\"diff-dock-tab-new\")",
            "this.toggle_diff_new_tab_menu(",
        );
        for needle in [
            ".role(Role::Button)",
            ".aria_label(NEW_TAB_LABEL)",
            ".delayed_tooltip(text_tooltip(NEW_TAB_LABEL))",
            ".on_click(",
        ] {
            assert!(new_tab.contains(needle), "the strip's `+` lost `{needle}`");
        }
        let close = source_slice(
            source,
            "format!(\"diff-dock-tab-close-{index}\")",
            "this.request_close_diff_tab(",
        );
        for needle in [
            ".role(Role::Button)",
            ".aria_label(close_label)",
            ".delayed_tooltip(text_tooltip(close_label))",
            ".on_click(",
        ] {
            assert!(close.contains(needle), "the tab close lost `{needle}`");
        }
    }
}
