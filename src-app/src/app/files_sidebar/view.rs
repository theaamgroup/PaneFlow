//! Files sidebar presentation: header (title + filter pill) + the
//! `uniform_list` body. The per-row render lives in `row.rs`; the rows come
//! from the panel's cached projection, so a frame builds only the range the
//! list asks for.

use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Role, Styled, Window, div, prelude::*, px,
};

use super::panel::{FilesEvent, FilesSidebar};
use super::{SIDEBAR_WIDTH, list};

use crate::ui_primitives::{AnimatedHoverExt, TooltipDelayExt, lerp_color, text_tooltip};

/// Accessible name and tooltip of the rail's close `×` (issue #340: one string
/// feeds both).
const CLOSE_FILES_LABEL: &str = "Close files sidebar";

impl FilesSidebar {
    pub(super) fn files_sidebar_header(
        &self,
        ui: crate::theme::UiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = self.title.clone();
        let hover_background = crate::app::constants::sidebar_tab_hover_background();
        let title_row = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .h(px(36.))
            .flex_none()
            .px(px(12.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text)
                    .child(title),
            )
            .child(
                div()
                    .id("files-sidebar-close")
                    .role(Role::Button)
                    .aria_label(CLOSE_FILES_LABEL)
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded(px(5.))
                    .text_size(px(14.))
                    .text_color(ui.muted)
                    .animated_hover(move |style, delta| {
                        style
                            .bg(lerp_color(
                                hover_background.opacity(0.0),
                                hover_background,
                                delta,
                            ))
                            .text_color(lerp_color(ui.muted, ui.text, delta));
                    })
                    .delayed_tooltip(text_tooltip(CLOSE_FILES_LABEL))
                    .on_click(cx.listener(|_, _: &ClickEvent, _window, cx| {
                        cx.emit(FilesEvent::Close);
                        cx.stop_propagation();
                    }))
                    .child("×"),
            );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .when(!self.docked, |header| header.child(title_row))
            .child(self.files_filter_row(ui, cx))
            .into_any_element()
    }

    fn files_filter_row(&self, ui: crate::theme::UiColors, cx: &mut Context<Self>) -> AnyElement {
        let is_empty = self.filter_input.read(cx).value().is_empty();
        div()
            .flex()
            .flex_none()
            .px(px(8.))
            .pb(px(6.))
            .when(self.docked, |row| row.pt(px(8.)))
            .child(
                crate::ui_primitives::filter_pill(
                    "files-sidebar-filter",
                    "files-sidebar-filter-clear",
                    ui,
                    self.filter_input.clone(),
                    !is_empty,
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.clear_files_filter(window, cx);
                    }),
                )
                .w_full()
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key.as_str() == "escape" && this.clear_files_filter(window, cx)
                    {
                        cx.stop_propagation();
                    }
                })),
            )
            .into_any_element()
    }

    fn files_sidebar_body(&self, ui: crate::theme::UiColors, cx: &mut Context<Self>) -> AnyElement {
        if self.projection.rows.is_empty() {
            let message = if self.projection_task.is_some() && !self.tree.root_listing_ready() {
                "Loading files..."
            } else if !self.query.is_empty() {
                "No matching files"
            } else if self.tree.root_listing_ready() {
                "This folder is empty."
            } else {
                "Loading files..."
            };
            return div()
                .flex_1()
                .p(px(14.))
                .text_size(px(12.))
                .text_color(ui.muted)
                .child(message)
                .into_any_element();
        }
        list::files_list(
            self.projection.rows.len(),
            &self.scroll,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let projection = this.projection.clone();
                range
                    .filter_map(|index| projection.rows.get(index))
                    .map(|row| {
                        this.files_row(
                            row,
                            this.selected.as_deref() == Some(row.node.path.as_path()),
                            ui,
                            cx,
                        )
                    })
                    .collect()
            }),
        )
        .into_any_element()
    }
}

impl Render for FilesSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.render_count += 1;
        }
        let ui = crate::theme::ui_colors();
        let theme = crate::theme::active_theme();
        div()
            .id("files-sidebar")
            .flex()
            .flex_col()
            .w(if self.docked {
                px(super::DOCK_TREE_WIDTH)
            } else {
                SIDEBAR_WIDTH
            })
            .h_full()
            .min_h_0()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_files_sidebar_key_down))
            .when(!self.docked, |panel| {
                panel.bg(crate::app::constants::cockpit_chrome_background(
                    theme.title_bar_background,
                    self.window_active,
                    self.material,
                ))
            })
            .child(self.files_sidebar_header(ui, cx))
            .child(self.files_sidebar_body(ui, cx))
    }
}

#[cfg(test)]
mod dock_tests {
    use super::*;
    use crate::app::files_sidebar::projection::FilesProjection;
    use crate::app::files_tree::{FileNode, FilesTreeState};
    use gpui::{TestAppContext, size};
    use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

    #[gpui::test]
    fn both_mounts_share_markdown_source_activation_filter_and_panel_state(
        cx: &mut TestAppContext,
    ) {
        let (panel, cx) = cx.add_window_view(|_, cx| FilesSidebar::new(cx));
        cx.simulate_resize(size(px(300.), px(350.)));
        let path = PathBuf::from("workspace/README.md");
        let root = PathBuf::from("workspace");
        panel.update(cx, |panel, _| {
            let mut tree = FilesTreeState::root_shell(root.clone());
            tree.children.insert(
                root.clone(),
                vec![FileNode {
                    path: path.clone(),
                    is_dir: false,
                    is_hidden: false,
                    is_ignored: false,
                    size: 0,
                }],
            );
            panel.active = true;
            panel.expanded = tree.expanded.clone();
            panel.projection = Arc::new(FilesProjection::build(&tree, &panel.expanded, ""));
            panel.tree = Arc::new(tree);
        });
        let opened = Rc::new(RefCell::new(Vec::new()));
        let events = opened.clone();
        cx.update(|_, cx| {
            cx.subscribe(&panel, move |_, event, _| {
                if let FilesEvent::OpenFile { path, root, .. } = event {
                    events.borrow_mut().push((path.clone(), root.clone()));
                }
            })
            .detach();
        });
        let snapshot = panel.read_with(cx, |panel, _| panel.tree.clone());
        for (docked, expected_width) in [(false, 284.), (true, 234.), (false, 284.)] {
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.set_chrome(true, true, docked, cx);
                    panel
                        .filter_input
                        .update(cx, |input, cx| input.set_value("readme", cx));
                });
                window.refresh();
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
            });
            let bounds = cx
                .debug_bounds("files-row-workspace/README.md")
                .expect("Markdown row in either mount");
            assert_eq!(bounds.size.width, px(expected_width));
            assert_eq!(bounds.size.height, px(28.));
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.activate_path(&path, false, window, cx)
                })
            });
            cx.run_until_parked();
            panel.read_with(cx, |panel, cx| {
                assert!(panel.active);
                assert!(Arc::ptr_eq(&snapshot, &panel.tree));
                assert_eq!(panel.filter_input.read(cx).value(), "readme");
                assert_eq!(panel.selected.as_ref(), Some(&path));
            });
        }
        assert_eq!(*opened.borrow(), vec![(path, root); 3]);
    }
}
