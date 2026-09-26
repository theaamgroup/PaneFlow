use crate::ui_primitives::TooltipDelayExt;

use gpui::{
    Context, EventEmitter, IntoElement, MouseButton, Render, Role, Styled, Window,
    WindowControlArea, div, prelude::*, px, svg,
};

use crate::{
    app::constants::{
        SIDEBAR_WIDTH, TITLE_BAR_CONTROL_SIZE, TITLE_BAR_EDGE_INSET, TITLE_BAR_MIN_HEIGHT,
    },
    ui_primitives::{AnimatedHoverExt, lerp_color},
};

pub struct TitleBar {
    should_move: bool,
    pub sidebar_visible: bool,
    /// Stable expanded width of the active left rail. The body can animate to
    /// zero independently, while title-bar controls remain stationary and
    /// align with the open rail in CLI, Diff, and Settings.
    pub left_rail_width: f32,
}

impl TitleBar {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            should_move: false,
            sidebar_visible: true,
            left_rail_width: SIDEBAR_WIDTH,
        }
    }
}

pub enum TitleBarEvent {
    ToggleSidebar,
}

impl EventEmitter<TitleBarEvent> for TitleBar {}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = (1.75 * window.rem_size()).max(TITLE_BAR_MIN_HEIGHT);

        // --- Left section: brand slot, fixed width aligned with sidebar ---
        let ui = crate::theme::ui_colors();
        // US-011: on macOS, reserve the leftmost ~80px of the custom titlebar
        // for the native red/yellow/green traffic lights (positioned at
        // x=12,y=12 by WindowOptions::titlebar::traffic_light_position in
        // main.rs).
        //
        // In macOS fullscreen AppKit hides the traffic lights, so the 80px
        // reservation would leave a dead gap before the brand cluster - drop
        // back to the shared 8px inset there.
        let brand_pl = if !window.is_fullscreen() {
            gpui::px(80.0)
        } else {
            TITLE_BAR_EDGE_INSET
        };
        let toggle_sidebar_handle = cx.entity().downgrade();
        let control_hover_bg = crate::app::constants::sidebar_tab_active_background();
        let toggle_sidebar_resting_bg = if self.sidebar_visible {
            control_hover_bg.opacity(0.0)
        } else {
            control_hover_bg
        };
        let sidebar_tooltip: gpui::SharedString = if self.sidebar_visible {
            "Hide sidebar"
        } else {
            "Show sidebar"
        }
        .into();
        let brand = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .pl(brand_pl)
            .pr(px(4.))
            .overflow_x_hidden()
            .child(
                div()
                    .id("toggle-primary-sidebar")
                    .role(Role::Button)
                    .aria_label(sidebar_tooltip.clone())
                    .flex_none()
                    .size(TITLE_BAR_CONTROL_SIZE)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .animated_hover(move |style, delta| {
                        style.bg(lerp_color(
                            toggle_sidebar_resting_bg,
                            control_hover_bg,
                            delta,
                        ));
                    })
                    .delayed_tooltip(move |_window, cx| {
                        let label = sidebar_tooltip.clone();
                        cx.new(|_| crate::app::sidebar::SidebarTooltip { label })
                            .into()
                    })
                    // Swallow the press so the bar's drag-to-move state machine
                    // never arms; the toggle itself fires on click so AccessKit
                    // exposes `Action::Click` on the button.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        if let Some(entity) = toggle_sidebar_handle.upgrade() {
                            entity.update(cx, |_this, cx| {
                                cx.emit(TitleBarEvent::ToggleSidebar);
                            });
                        }
                    })
                    .child(
                        svg()
                            .size(px(14.))
                            .path("icons/sidebar.svg")
                            .text_color(ui.muted),
                    ),
            );
        let left_rail = div()
            .flex_none()
            .w(px(self.left_rail_width))
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .overflow_x_hidden()
            .child(brand);

        // --- Center section: an empty drag area ---
        // Takes the remaining flex space. The workspace name already anchors
        // the sidebar, so the title-bar centre stays a clean drag area.
        let content = div().flex_1().min_w_0();

        let bar = div()
            .id("title-bar")
            .window_control_area(WindowControlArea::Drag)
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(height)
            // No fill: the parent window shell owns the active/inactive tint,
            // so the themed shell or the native material is composed once.
            .pr(TITLE_BAR_EDGE_INSET);

        bar
            // Drag-to-move state machine
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.should_move = true;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.should_move = false;
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.should_move = false;
            }))
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.should_move {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    window.zoom_window();
                }
            })
            .child(left_rail)
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    /// Issue #321: the sidebar toggle had a visual tooltip but no button
    /// role, no accessible name, and fired on `mouse_down`, so AccessKit never
    /// exposed it as a named, clickable control. This scan pins the toggle's
    /// builder chain to the repo recipe (`Role::Button` + `aria_label` bound
    /// to the state-tracking tooltip text + `on_click`).
    #[test]
    fn sidebar_toggle_is_an_accessible_named_button() {
        let source = include_str!("title_bar.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production title bar source");
        let chain = source
            .split(".id(\"toggle-primary-sidebar\")")
            .nth(1)
            .and_then(|rest| rest.split("svg()").next())
            .expect("title_bar.rs builds the toggle-primary-sidebar control");
        for needle in [
            ".role(Role::Button)",
            ".aria_label(sidebar_tooltip.clone())",
            ".on_click(",
        ] {
            assert!(
                chain.contains(needle),
                "sidebar toggle lost `{needle}`; AccessKit needs it to expose a named button"
            );
        }
        let click_at = chain.find(".on_click(").unwrap();
        let emit_at = chain
            .find("cx.emit(TitleBarEvent::ToggleSidebar)")
            .expect("sidebar toggle emits TitleBarEvent::ToggleSidebar");
        assert!(
            emit_at > click_at && !chain[click_at..emit_at].contains(".on_mouse_down("),
            "TitleBarEvent::ToggleSidebar must be emitted from the on_click handler, \
             not from on_mouse_down, so accesskit::Action::Click reaches it"
        );
    }

    #[test]
    fn title_bar_files_and_help_popovers_are_removed_end_to_end() {
        let title_bar = include_str!("title_bar.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production title bar source");
        for removed in [
            "files_menu_open",
            "help_menu_open",
            "ToggleFilesMenu",
            "ToggleHelpMenu",
            "title-bar-files-menu-trigger",
            "title-bar-help-menu-trigger",
        ] {
            assert!(
                !title_bar.contains(removed),
                "custom title bar still contains removed menu surface `{removed}`"
            );
        }

        for removed in ["profile-menu", "\"Guest\""] {
            assert!(
                !title_bar.contains(removed),
                "title bar still contains the removed avatar menu `{removed}`"
            );
        }

        let event_handlers = include_str!("../app/event_handlers.rs");
        let bootstrap = include_str!("../app/bootstrap.rs");
        let workspace_ops = include_str!("../app/workspace_ops/mod.rs");
        let main = include_str!("../main.rs");
        for (path, source) in [
            ("event_handlers.rs", event_handlers),
            ("bootstrap.rs", bootstrap),
            ("workspace_ops/mod.rs", workspace_ops),
            ("main.rs", main),
        ] {
            for removed in [
                "title_bar_files_menu_open",
                "title_bar_help_menu_open",
                "render_title_bar_files_menu",
                "render_title_bar_help_menu",
            ] {
                assert!(
                    !source.contains(removed),
                    "{path} still contains removed custom title-menu state `{removed}`"
                );
            }
        }

        for native_item in [
            "MenuItem::action(\"About PaneFlow\", About)",
            "MenuItem::action(\"New Workspace\", NewWorkspace)",
            "Menu::new(\"Help\")",
            "MenuItem::action(\"PaneFlow Help\", OpenHelp)",
        ] {
            assert!(
                bootstrap.contains(native_item),
                "removing custom popovers must preserve native macOS item `{native_item}`"
            );
        }
        assert!(
            !main.contains("profile-menu"),
            "the title-bar avatar menu must not be mounted from main.rs"
        );
    }

    /// Issue #852: `main.rs` pinned the bar to cockpit chrome on every frame,
    /// so the breadcrumb, the IPC pill and the bottom divider behind
    /// `!cockpit` never rendered, and macOS has no native window menu for a
    /// right-click to show. All of it was removed; keep it from coming back.
    #[test]
    fn title_bar_carries_no_dead_cockpit_branches() {
        let title_bar = include_str!("title_bar.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production title bar source");
        for removed in [
            "cockpit",
            "workspace_name",
            "ipc_state",
            "ipc-offline-pill",
            "show_window_menu",
            "window_controls()",
        ] {
            assert!(
                !title_bar.contains(removed),
                "title bar still carries removed dead chrome `{removed}`"
            );
        }
        let main = include_str!("../main.rs");
        for removed in ["tb.cockpit", "tb.workspace_name", "tb.ipc_state"] {
            assert!(
                !main.contains(removed),
                "main.rs still pushes removed title-bar state `{removed}`"
            );
        }
    }
}
