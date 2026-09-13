## What's new

Completed changes from the upstream v0.13.0 / v0.14.0 adoption (#487), for the next release:

- **Remembered editor controls:** Minimap and Scrollbar choices apply to every open file and survive a restart. Hand-editing `editor.minimap` or `editor.scrollbar` updates live and parked editors automatically.
- **Maximize the Changes dock:** Cmd+Shift+F expands the dock over the pane grid and restores it with the same shortcut. The sidebar and dock transitions honor Reduce Motion.
- **Remembered unread marks and workspace mute:** Session restoration keeps unread status and muted workspaces. Workspace menus offer Mark as Read and Mute/Unmute Notifications.
- **Pull-request markers at launch:** Restored markers appear immediately while their current state refreshes in the background.
- **Smoother menus:** Menus fade into place, with an immediate reveal when Reduce Motion is enabled.
- **Clearer sidebar rows:** Tab rows sit inside the indent guide, and one session per tool on a surface prevents duplicate agent counts.
