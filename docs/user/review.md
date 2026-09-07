# Review

Review lets you read changes from several Git checkouts in one window.
Open a Git-backed workspace and select **Review** in the sidebar footer,
or press `Cmd+Shift+G` to switch between Agents and Review.

## Workspaces, changes, and diff panes

The **Workspaces** rail groups your open repositories and their checkouts,
including worktrees bound to tabs and sibling worktrees. Expand a repository
and click a checkout to show it in the active diff pane. If that checkout is
already open in another pane, PaneFlow focuses that pane instead.

The **Changes** rail follows the active diff pane. Filter its files, switch
between a flat list and a tree, and click a file to jump to its diff. The
**vs …** control chooses the comparison's base branch or Git ref for that
pane. The diff includes branch changes since the merge base and uncommitted
work in that checkout, regardless of who made the edits.

Each diff pane shows one checkout. File headers stay pinned while scrolling,
and Tree-sitter provides syntax highlighting. Header controls refresh the
diff, toggle unified or side-by-side display, and expand or collapse all
files. You can also switch the display mode by right-clicking inside the diff.

## Arrange comparisons

Review supports up to six diff panes:

* Drag a checkout from Workspaces onto a pane's edge to split the space
  and open another diff. Drop it in the center to replace that pane's checkout.
* Use the pane header's split controls to duplicate its checkout into a
  neighboring pane, then choose another checkout or base to compare.
* Drag a pane header to move it within the grid, and drag dividers to resize.
* Zoom a pane for a closer look, then unzoom before adding another split.
  Close panes from their headers when finished.

Each pane has its own base and display mode. The Workspaces rail marks
checkouts already in the grid, while Changes always follows the active pane.

PaneFlow saves the grid's checkouts, split directions and sizes, and collapsed
repository groups with your session. On restart it restores checkouts that
still exist and belong to an open repository. Base selections and unified or
side-by-side choices are not saved with the grid.

## Ask an agent to review

Choose **Review with agent** in a diff pane's header and select the installed
agents you want to use: Claude Code, Codex, OpenCode, or Pi. PaneFlow opens
an ordinary workspace tab for each agent in that checkout's directory and
switches to Agents. Additional agents get a second-opinion prompt. The diff
remains available when you return to Review.

PaneFlow prefills a review prompt after a short delay. Read or edit it, then
press **Enter** yourself to submit it. The first review prompt is also copied to your
clipboard, so you can paste it if the agent was not ready for the prefill.
The `review_prefill_delay_ms` configuration setting adjusts that delay.

## Turn Review off

Disable Review in Settings to hide the entire Agents/Review mode strip.
While disabled, `Cmd+Shift+G` has no effect.

## See also

* [Features](features.md) — projects, worktrees, and agents.
* [Keybindings](keybindings.md) — default shortcuts and remapping.
* [Configuration](configuration/schema.md) — available settings.
