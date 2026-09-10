use std::path::{Path, PathBuf};

use gpui::{AnyElement, Context, Entity, IntoElement, StyleRefinement, Styled, Window, px};

use super::{FILES_SIDEBAR_WIDTH, FilesEvent, FilesSidebar};
use crate::PaneFlowApp;

/// Which workspace an `Expanded` event from the rail belongs to: the index of
/// the workspace the rail was opened for (`files_sidebar_workspace`), and only
/// while that workspace's `cwd` is still the event's `root`.
///
/// Never the active workspace. The rail stays warm while unmounted (Review,
/// Settings), so a worker update rooted on workspace A can arrive after the
/// user has switched to workspace B; and two workspaces may share a `cwd`, so
/// a root-only match would let A's expansion overwrite B's and persist it.
pub(crate) fn files_expansion_target<'a>(
    owner: Option<u64>,
    root: &Path,
    workspaces: impl IntoIterator<Item = (u64, &'a str)>,
) -> Option<usize> {
    let owner = owner?;
    workspaces
        .into_iter()
        .position(|(id, cwd)| id == owner && Path::new(cwd) == root)
}

impl PaneFlowApp {
    pub(crate) fn handle_files_event(
        &mut self,
        _: Entity<FilesSidebar>,
        event: &FilesEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            FilesEvent::Close => self.close_files_sidebar(cx),
            FilesEvent::ContextMenu(menu) => {
                self.dismiss_transient_surfaces();
                self.files_menu_open = Some(menu.clone());
                cx.notify();
            }
            FilesEvent::Expanded { root, paths } => {
                let Some(idx) = files_expansion_target(
                    self.files_sidebar_workspace,
                    root,
                    self.workspaces.iter().map(|ws| (ws.id, ws.cwd.as_str())),
                ) else {
                    return;
                };
                let ws = &mut self.workspaces[idx];
                if ws.files_expanded != *paths {
                    ws.files_expanded = paths.clone();
                    self.save_session(cx);
                }
            }
            FilesEvent::OpenFile { path, root, window } => {
                let host = cx.weak_entity();
                let path = path.clone();
                let root = root.clone();
                let window = *window;
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        let _ = host.update(cx, |app, cx| {
                            app.open_file_in_diff_dock(path, root, window, cx)
                        });
                    });
                });
            }
        }
    }

    fn open_file_in_diff_dock(
        &mut self,
        path: PathBuf,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_section.is_some() {
            self.close_settings(cx);
        }
        self.enter_cli_mode(window, cx);
        if !self.diff_dock.open {
            self.open_diff_dock_panel(root.to_string_lossy().into_owned(), cx);
        }
        self.diff_dock.picker = false;
        self.diff_dock.picked = true;
        self.open_diff_file_tab(path, window, cx);
    }

    pub(crate) fn render_files_sidebar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let material = self.cached_config.cockpit_chrome_material_enabled();
        self.files_sidebar.update(cx, |panel, cx| {
            panel.set_chrome(
                material,
                window.is_window_active(),
                self.files_tree_in_dock(),
                cx,
            )
        });
        self.files_sidebar
            .clone()
            .cached(
                StyleRefinement::default()
                    .w(px(if self.files_tree_in_dock() {
                        super::DOCK_TREE_WIDTH
                    } else {
                        FILES_SIDEBAR_WIDTH
                    }))
                    .h_full()
                    .flex_shrink_0(),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::files_expansion_target;

    /// Two workspaces on one `cwd`, the rail opened for the first, the user
    /// now on the second: a worker update from the first's tree must land on
    /// the first, never on the active one.
    #[test]
    fn expansion_lands_on_the_rails_workspace_not_the_active_one() {
        let workspaces = [(7, "/repo"), (9, "/repo")];
        assert_eq!(
            files_expansion_target(Some(7), Path::new("/repo"), workspaces),
            Some(0)
        );
        assert_eq!(
            files_expansion_target(Some(9), Path::new("/repo"), workspaces),
            Some(1)
        );
    }

    /// The owning workspace's `cwd` still has to be the event's root: a rail
    /// rooted elsewhere (or a closed rail with no owner) writes nothing.
    #[test]
    fn expansion_is_dropped_when_the_owner_moved_or_the_rail_is_closed() {
        let workspaces = [(7, "/repo"), (9, "/other")];
        assert_eq!(
            files_expansion_target(Some(7), Path::new("/other"), workspaces),
            None
        );
        assert_eq!(
            files_expansion_target(Some(3), Path::new("/repo"), workspaces),
            None
        );
        assert_eq!(
            files_expansion_target(None, Path::new("/repo"), workspaces),
            None
        );
    }

    /// The handler must not fall back to the active workspace by path: that
    /// is the overwrite the owner check exists to prevent.
    #[test]
    fn handler_resolves_the_target_by_owner_id_not_by_active_workspace() {
        let src = include_str!("integration.rs");
        let handler = src
            .split("FilesEvent::Expanded { root, paths } =>")
            .nth(1)
            .and_then(|rest| rest.split("FilesEvent::OpenFile").next())
            .expect("Expanded arm");
        assert!(
            handler.contains("files_expansion_target(")
                && handler.contains("self.files_sidebar_workspace"),
            "the Expanded arm must resolve its workspace through the rail's owner id: {handler}"
        );
        assert!(
            !handler.contains("active_workspace_mut()"),
            "the Expanded arm must not write to the active workspace: {handler}"
        );
    }
}
