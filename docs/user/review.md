# Review

PaneFlow's Diff view puts your branch diff and the agents reviewing it
in the same workspace. Read every change, send it to one or several CLI
agents, and compare their feedback without switching windows or
branches.

It works for projects that live inside a Git repository. Open a project
and select **Diff** in the sidebar to get started.

## What the Diff view shows

The Diff view reflects the state of your Git branch, not just what an
agent edited. It shows every file changed since the branch diverged from
its base, uncommitted work included:

* Changes made by an agent
* Changes you made yourself
* Any other uncommitted changes in the repo

File headers stay pinned while you scroll, and syntax highlighting runs
on Tree-sitter.

## Open and navigate the Diff view

Open the Diff view from the sidebar or with `Cmd+Shift+G`. Once
it's open:

* Scroll through hunks to compare additions and deletions.
* Toggle between a unified view and side by side.
* Pinned file headers keep your place while you scroll a long diff.

## Compare branches without switching worktrees

You don't have to check branches out one at a time to read them:

* **Project view** shows the diff for your current task.
* **Multi-project mode** gathers your open repos into tabs.
* **Worktree mode** lines sibling branches up side by side, so you can
  read each one's progress at a glance.

## Ask agents to review the diff

Every branch column has a **Review** button. Pick Claude Code, Codex,
OpenCode, or Pi, and PaneFlow opens that agent in a real terminal
directly under the diff, already in the right directory.

* PaneFlow **stages the review prompt but never sends it without your
  approval**: you read it before pressing Enter. Implementation is a
  delayed `send_text` with no carriage return
  (`src-app/src/diff/view/review.rs`).
* The prompt is also copied to the clipboard as a fallback, and a
  "Prompt ready" pill appears with a paste hint, in case the delayed
  write missed the CLI's input box.
* The agent works through the diff and returns findings tied to specific
  files and lines.
* Launch a second agent and it gets a more skeptical framing, so you get
  a genuine second opinion instead of a near-identical echo of the first.
* Target a specific line or hunk and send it straight to an agent with a
  focused question.

## See also

* [Features](features.md) - the full tour, including projects, worktrees, and per-thread agents.
* [index](index.md) - the overview, and [installation](installation.md) to build it.
* [Keybindings](keybindings.md) - the default keymap and how to remap any action.
