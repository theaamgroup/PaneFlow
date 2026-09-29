use super::*;
use crate::diff::review_terminal::ReviewCli;
use gpui::{Toggled, canvas};

impl Pane {
    pub(super) fn render_review_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = pane_colors();
        self.bind_review_menu_owner(cx);
        let focus = self.review_menu_focus(cx);
        let claim = focus.clone();
        let claim_pending = self.review_menu_needs_focus.clone();
        let mut menu = crate::settings::components::menu_surface(div().id("pane-review-menu"), ui)
            .absolute()
            .top(px(PANE_HEADER_HEIGHT))
            .right(px(6.))
            .w(px(256.))
            .flex()
            .flex_col()
            .p(px(6.))
            .gap(px(2.))
            .occlude()
            .track_focus(&focus)
            .tab_group()
            .role(Role::Menu)
            .aria_label("Review with agent")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.dismiss_review_menu(window, cx);
            }))
            .on_key_down(cx.listener(Self::handle_review_menu_key))
            .child(
                div()
                    .text_color(ui.muted)
                    .text_size(px(12.))
                    .child("Review in a new agent tab"),
            )
            // Claim focus once, on the frame the menu opens. Later frames
            // must not pull it back after the user moves to another control.
            // A diff that still holds focus is closed by `DiffDismiss`.
            .child(
                canvas(
                    move |_bounds, window, cx| {
                        if !claim_pending.replace(false) {
                            return;
                        }
                        if !claim.contains_focused(window, cx) {
                            claim.focus(window, cx);
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        for row in self.review_pick_rows(cx) {
            menu = menu.child(row);
        }
        menu = menu.child(self.review_launch_row(cx)).child(
            div()
                .text_size(px(11.))
                .text_color(ui.muted)
                .child("Prompt copied too · ⌘V to paste · Enter to submit"),
        );
        deferred(crate::ui_primitives::menu_reveal(
            "pane-review-agent-menu-reveal",
            menu,
        ))
        .priority(8)
        .into_any_element()
    }

    fn review_menu_focus(&self, cx: &App) -> FocusHandle {
        match &self.surface {
            PaneSurface::Diff(diff) => diff.read(cx).review_menu_focus_handle(),
            PaneSurface::Terminal(_) => cx.focus_handle(),
        }
    }

    fn bind_review_menu_owner(&self, cx: &Context<Self>) {
        let PaneSurface::Diff(diff) = &self.surface else {
            return;
        };
        let owner = cx.entity().downgrade();
        diff.read(cx).bind_review_menu_owner(owner);
    }

    pub(super) fn toggle_review_menu(&mut self) {
        self.review_menu_open = !self.review_menu_open;
        self.review_menu_needs_focus.set(self.review_menu_open);
    }

    /// Drop the popover without moving focus. `dismiss_overlays` focuses the
    /// diff itself; the menu's own close path restores focus separately so it
    /// does not re-enter the diff entity from inside the diff's update.
    pub(crate) fn close_review_menu(&mut self, cx: &mut Context<Self>) {
        if !self.review_menu_open {
            return;
        }
        self.review_menu_open = false;
        self.review_menu_needs_focus.set(false);
        cx.notify();
    }

    fn dismiss_review_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_review_menu(cx);
        self.focus_handle(cx).focus(window, cx);
    }

    fn handle_review_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.dismiss_review_menu(window, cx);
                cx.stop_propagation();
            }
            "up" => {
                window.focus_prev(cx);
                cx.stop_propagation();
            }
            "down" => {
                window.focus_next(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn review_pick_rows(&self, cx: &mut Context<Self>) -> Vec<crate::ui_primitives::AnimatedHover> {
        let ui = pane_colors();
        let mut rows = Vec::with_capacity(ReviewCli::all().len());
        for (index, cli) in ReviewCli::all().into_iter().enumerate() {
            let checked = self.review_picks[index];
            let label = cli.label();
            rows.push(
                crate::settings::components::select_item(
                    SharedString::from(format!("review-pick-{index}")),
                    false,
                    ui,
                )
                .role(Role::CheckBox)
                .aria_toggled(if checked {
                    Toggled::True
                } else {
                    Toggled::False
                })
                .aria_label(label)
                .tab_index(0)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.review_picks[index] = !this.review_picks[index];
                    cx.notify();
                }))
                .child(if checked { "✓" } else { "○" })
                .child(label),
            );
        }
        rows
    }

    fn review_launch_row(&self, cx: &mut Context<Self>) -> crate::ui_primitives::AnimatedHover {
        let ui = pane_colors();
        crate::settings::components::select_item("review-launch", false, ui)
            .role(Role::Button)
            .aria_label("Review with agent")
            .tab_index(0)
            .on_click(cx.listener(|this, _, window, cx| {
                this.launch_review_from_menu(window, cx);
            }))
            .child("Review with agent")
    }

    fn launch_review_from_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let PaneSurface::Diff(view) = &self.surface else {
            return;
        };
        let subject = view.read(cx).subject();
        let base = view.read(cx).base_ref().to_owned();
        let picks = ReviewCli::all()
            .into_iter()
            .enumerate()
            .filter_map(|(i, cli)| self.review_picks[i].then_some(cli))
            .collect();
        cx.emit(PaneEvent::ReviewWithAgent {
            subject,
            base,
            picks,
        });
        self.dismiss_review_menu(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use gpui::Element;
    use gpui::Focusable;
    use gpui::accesskit::{Node, Role, Toggled};

    fn subject() -> crate::diff::ReviewSubject {
        let root = std::env::temp_dir();
        crate::diff::ReviewSubject {
            repo_root: root.clone(),
            worktree: crate::diff::DiffWorktree {
                path: root,
                branch: "main".into(),
                workspace_id: None,
            },
        }
    }

    fn diff_pane(cx: &mut gpui::App) -> gpui::Entity<super::super::Pane> {
        use gpui::AppContext as _;
        let diff = cx.new(|cx| crate::diff::DiffView::for_test(subject(), cx));
        cx.new(|cx| {
            super::super::Pane::new_with_surface(super::super::PaneSurface::Diff(diff), 1, cx)
        })
    }

    /// Issue #883: pick rows are checkboxes, not an unlabeled glyph.
    #[gpui::test]
    fn review_menu_picks_are_checkboxes(cx: &mut gpui::TestAppContext) {
        let pane = cx.update(diff_pane);
        pane.update(cx, |pane, cx| {
            pane.review_picks = [true, false, false];
            let rows = pane.review_pick_rows(cx);
            assert_eq!(rows.len(), 3, "the menu renders one row per review agent");
            for (index, row) in rows.iter().enumerate() {
                let mut node = Node::new(Role::Unknown);
                Element::write_a11y_info(row, &mut node);
                assert_eq!(
                    Element::a11y_role(row),
                    Some(Role::CheckBox),
                    "pick row {index} is not a checkbox"
                );
                assert_eq!(
                    node.toggled(),
                    Some(if index == 0 {
                        Toggled::True
                    } else {
                        Toggled::False
                    }),
                    "pick row {index} reported the wrong toggled state"
                );
                assert_eq!(
                    node.label(),
                    Some(crate::diff::review_terminal::ReviewCli::all()[index].label())
                );
            }
            let launch = pane.review_launch_row(cx);
            let mut node = Node::new(Role::Unknown);
            Element::write_a11y_info(&launch, &mut node);
            assert_eq!(Element::a11y_role(&launch), Some(Role::Button));
            assert_eq!(node.label(), Some("Review with agent"));
            // The menu mounts those same rows.
            let _menu = pane.render_review_menu(cx);
        });
    }

    /// Issue #883: Escape closes the popover opened by `DiffReviewWithAgent`.
    #[gpui::test]
    fn review_menu_closes_on_escape(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        let (pane, cx) = cx.add_window_view(|_window, cx| {
            let diff = cx.new(|cx| crate::diff::DiffView::for_test(subject(), cx));
            super::super::Pane::new_with_surface(super::super::PaneSurface::Diff(diff), 1, cx)
        });
        cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(600.)));
        cx.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                pane.focus_handle(cx).focus(window, cx);
            });
            window.draw(cx).clear(cx);
        });
        cx.dispatch_action(crate::DiffReviewWithAgent);
        assert!(
            cx.update(|_, app| pane.read(app).review_menu_open),
            "DiffReviewWithAgent did not open the menu"
        );
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.simulate_keystrokes("escape");
        assert!(
            !cx.update(|_, app| pane.read(app).review_menu_open),
            "escape left the review menu open"
        );
    }
}
