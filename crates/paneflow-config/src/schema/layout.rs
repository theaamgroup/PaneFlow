use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::warn;

/// A single command definition, compatible with the cmux workspace format.
///
/// Each entry is either a workspace definition (with `workspace`) or a simple
/// shell command (with `command`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CommandDefinition {
    /// Display name (must not be blank).
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Search keywords for fuzzy matching.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Workspace layout definition (mutually exclusive with `command`).
    pub workspace: Option<WorkspaceDefinition>,
    /// Shell command string (mutually exclusive with `workspace`).
    pub command: Option<String>,
}

impl<'de> Deserialize<'de> for CommandDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawCommandDefinition {
            name: String,
            description: Option<String>,
            #[serde(default)]
            keywords: Vec<String>,
            workspace: Option<WorkspaceDefinition>,
            command: Option<String>,
        }
        let raw = RawCommandDefinition::deserialize(deserializer)?;
        if raw.name.trim().is_empty() {
            return Err(serde::de::Error::custom("command name must not be blank"));
        }
        match (&raw.workspace, &raw.command) {
            (Some(_), None) => {}
            (None, Some(c)) if !c.trim().is_empty() => {}
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "command must contain exactly one of `workspace` or `command`",
                ))
            }
            (None, Some(_)) => {
                return Err(serde::de::Error::custom("shell command must not be blank"))
            }
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "command must contain either `workspace` or `command`",
                ))
            }
        }
        Ok(Self {
            name: raw.name,
            description: raw.description,
            keywords: raw.keywords,
            workspace: raw.workspace,
            command: raw.command,
        })
    }
}

/// Workspace definition containing layout, working directory, and visual config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceDefinition {
    /// Workspace display name.
    pub name: Option<String>,
    /// Default working directory for the workspace.
    pub cwd: Option<String>,
    /// Layout preset used by the visual workspace builder.
    ///
    /// Accepted values mirror `paneflow up`: `"even_h"`, `"even_v"`,
    /// `"main_vertical"`, and `"tiled"`. Older configs may omit this and rely
    /// on `layout` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_preset: Option<String>,
    /// Color as a 6-digit hex string (e.g. "ff6600").
    pub color: Option<String>,
    /// Root layout node describing pane arrangement.
    pub layout: Option<LayoutNode>,
}

/// A node in the layout tree: either a leaf pane or a split container.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    /// A leaf pane containing one or more surfaces.
    Pane {
        /// Surfaces within this pane (must have >= 1).
        #[serde(default)]
        surfaces: Vec<SurfaceDefinition>,
    },
    /// A split container dividing space between 2 or more children.
    Split {
        /// Split direction: "horizontal" or "vertical".
        direction: String,
        /// Legacy: single split ratio for binary (2-child) layouts.
        /// Ignored when `ratios` is present. Defaults to 0.5 if omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ratio: Option<f64>,
        /// Per-child ratios for N-ary layouts. When present, must have
        /// the same length as `children`. Values should sum to ~1.0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ratios: Option<Vec<f64>>,
        /// 2 or more child layout nodes.
        #[serde(default)]
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    /// Count the number of leaf (Pane) nodes in the layout tree.
    pub fn leaf_count(&self) -> usize {
        match self {
            LayoutNode::Pane { .. } => 1,
            LayoutNode::Split { children, .. } => children.iter().map(|c| c.leaf_count()).sum(),
        }
    }

    /// Resolve per-child ratios for a Split node.
    ///
    /// Returns `ratios` if present, else converts legacy `ratio` to binary
    /// `[ratio, 1-ratio]`, else returns equal ratios for the child count.
    ///
    /// US-056: persisted ratios are untrusted input - a hand-edited or corrupt
    /// `session.json` can carry NaN, negative, zero, or wrong-length values. Any
    /// user-supplied set is run through [`sanitize_ratios`] (clamp into
    /// `[MIN_RATIO, 1.0]`, reject non-finite/negative, normalize to sum 1.0)
    /// before it reaches layout construction; the internally generated
    /// equal-share fallback is already valid and returned verbatim.
    pub fn resolved_ratios(&self) -> Vec<f64> {
        match self {
            LayoutNode::Pane { .. } => vec![1.0],
            LayoutNode::Split {
                ratio,
                ratios,
                children,
                ..
            } => {
                let n = children.len().max(1);
                let raw = if let Some(rs) = ratios {
                    rs.clone()
                } else if let Some(r) = ratio {
                    if children.len() == 2 {
                        vec![*r, 1.0 - *r]
                    } else {
                        return vec![1.0 / n as f64; n];
                    }
                } else {
                    return vec![1.0 / n as f64; n];
                };
                sanitize_ratios(raw, n)
            }
        }
    }
}

/// Floor for any single persisted split ratio. Clamping to this keeps every
/// pane visible and prevents a divide-by-zero when the set is normalized.
const MIN_RATIO: f64 = 0.01;

/// Clamp every ratio into `[MIN_RATIO, 1.0]` (mapping NaN/inf/negative to the
/// floor), then normalize so the set sums to 1.0. A length mismatch with the
/// child count is unrecoverable - we cannot know which child a stale ratio was
/// meant for - so it degrades to equal shares.
fn sanitize_ratios(mut ratios: Vec<f64>, n: usize) -> Vec<f64> {
    if ratios.len() != n {
        return vec![1.0 / n as f64; n];
    }
    for r in ratios.iter_mut() {
        *r = if r.is_finite() {
            r.clamp(MIN_RATIO, 1.0)
        } else {
            MIN_RATIO
        };
    }
    let sum: f64 = ratios.iter().sum();
    if sum > 0.0 && (sum - 1.0).abs() > 1e-9 {
        for r in ratios.iter_mut() {
            *r /= sum;
        }
    }
    // US-056 (EP-010 review): re-clamp after normalize. Dividing by a sum > 1
    // can push a just-clamped ratio back below `MIN_RATIO` (e.g. raw
    // `[1.0, 0.005]` → clamp `[1.0, 0.01]` → normalize `[0.990, 0.0099]`),
    // silently violating the floor this fn promises. The config-loader sibling
    // (`loader::validate_layout`) already re-clamps for this exact reason
    // (US-057); the session path must match so both frontiers honour the same
    // 0.01 floor. The renderer re-normalizes proportionally at paint time, so
    // the post-re-clamp sum need not be exactly 1.0 - the floor is the invariant.
    for r in ratios.iter_mut() {
        *r = r.clamp(MIN_RATIO, 1.0);
    }
    ratios
}

/// US-011: total pane (leaf) budget for a single restored layout. Mirrors
/// src-app's `MAX_PANES` - defined locally because `paneflow-config` is a leaf
/// crate that cannot import the src-app constant (US-013 documents the pairing).
pub const MAX_LAYOUT_LEAVES: usize = 32;

/// US-011: max direct children of one `Split` node at the schema boundary.
const MAX_SPLIT_CHILDREN: usize = 32;

/// US-011 / issue #30: max surfaces (tabs) in one `Pane` at the schema
/// boundary. Live `add_tab` in src-app re-exports this as
/// `limits::MAX_PANE_SURFACES` so the write cap cannot drift from the
/// restore truncate. Includes markdown (and any other non-PTY tab): the
/// restore cap is surfaces, not terminals-only.
pub const MAX_PANE_SURFACES: usize = 64;

/// Recursively validate and fix a layout node, bounding its breadth and total
/// leaf count at the schema boundary (U-008/U-016).
///
/// - Attacker-driven panes (leaves) are capped to [`MAX_LAYOUT_LEAVES`]. That
///   is a leaf budget, not a PTY budget: each leaf may still hold up to
///   [`MAX_PANE_SURFACES`] surfaces, and every non-markdown surface becomes a
///   terminal on restore. Session restore counts those PTY surfaces against a
///   workspace terminal budget (issue #30). A pruned split may gain ≤1
///   app-synthesized pad pane to stay structurally valid; that is bounded and
///   not attacker-amplified - see the pad note.
/// - Split nodes: direct children bounded to [`MAX_SPLIT_CHILDREN`]; must have
///   at least 2 children; legacy `ratio` clamped to [0.1, 0.9] and (for a
///   2-child split) converted to an explicit `ratios` pair (U-007); per-child
///   `ratios` clamped to [0.01, 1.0].
/// - Pane nodes: surfaces bounded to [`MAX_PANE_SURFACES`] (logged truncate);
///   must have at least 1.
pub fn validate_layout(node: &mut LayoutNode) {
    let mut leaf_budget = MAX_LAYOUT_LEAVES;
    validate_node(node, &mut leaf_budget);
}

fn validate_node(node: &mut LayoutNode, leaf_budget: &mut usize) {
    match node {
        LayoutNode::Split {
            ref mut direction,
            ref mut ratio,
            ref mut ratios,
            ref mut children,
            ..
        } => {
            if direction != "horizontal" && direction != "vertical" {
                warn!("split direction `{direction}` is invalid; resetting to horizontal");
                *direction = "horizontal".to_string();
            }

            // U-008: bound a single Split's direct breadth before anything else
            // touches the (possibly huge) children vec.
            if children.len() > MAX_SPLIT_CHILDREN {
                warn!(
                    "split has {} children (cap {MAX_SPLIT_CHILDREN}); truncating",
                    children.len()
                );
                children.truncate(MAX_SPLIT_CHILDREN);
            }

            // U-008/U-016: recurse under a shared leaf budget and drop whole
            // subtrees once it is spent, so the total pane count across the tree
            // can never exceed MAX_LAYOUT_LEAVES.
            let mut kept = 0usize;
            for child in children.iter_mut() {
                if *leaf_budget == 0 {
                    break;
                }
                validate_node(child, leaf_budget);
                kept += 1;
            }
            if kept < children.len() {
                warn!(
                    "layout exceeds {MAX_LAYOUT_LEAVES} panes; dropping {} subtree(s)",
                    children.len() - kept
                );
                children.truncate(kept);
            }

            // Must have at least 2 children; pad if fewer (malformed input, or
            // an over-pruned split when earlier siblings spent the budget).
            // The DoS guarantee is about ATTACKER-DRIVEN leaves: those are hard
            // capped at MAX_LAYOUT_LEAVES above. These pad panes are
            // app-synthesized to keep a split structurally valid (>= 2
            // children) and add at most one per pruned split - a bounded
            // structural overshoot, never attacker-amplified PTY spawning.
            while children.len() < 2 {
                warn!(
                    "split node has {} children (need >= 2); padding",
                    children.len()
                );
                children.push(LayoutNode::Pane {
                    surfaces: vec![Default::default()],
                });
                *leaf_budget = leaf_budget.saturating_sub(1);
            }

            // Clamp legacy ratio to [0.1, 0.9]; reject non-finite values.
            if let Some(r) = ratio {
                if !r.is_finite() {
                    warn!("split ratio is NaN/Infinity; resetting to 0.5");
                    *r = 0.5;
                } else if *r < 0.1 {
                    warn!("split ratio {r} is below minimum; clamping to 0.1");
                    *r = 0.1;
                } else if *r > 0.9 {
                    warn!("split ratio {r} is above maximum; clamping to 0.9");
                    *r = 0.9;
                }
            }

            // U-007: a legacy single `ratio` is only meaningful for a 2-child
            // split - convert it to an explicit `ratios` pair so it survives
            // restore (resolved_ratios only honors it transiently). For an
            // N-ary split it is ambiguous, so warn that it is ignored rather
            // than silently returning equal shares.
            if ratios.is_none() {
                if let Some(r) = ratio {
                    if children.len() == 2 {
                        *ratios = Some(vec![*r, 1.0 - *r]);
                    } else {
                        warn!(
                            "legacy ratio ignored on N-ary split ({} children)",
                            children.len()
                        );
                    }
                }
            }

            // Validate per-child ratios: reject non-finite, fix length mismatch, normalize.
            if let Some(ref mut rs) = ratios {
                // Reject NaN/Infinity values.
                for r in rs.iter_mut() {
                    if !r.is_finite() {
                        warn!("per-child ratio is NaN/Infinity; resetting");
                        *r = 1.0 / children.len() as f64;
                    }
                }
                // Fix length mismatch: trim or extend to match children count.
                let n = children.len();
                if rs.len() != n {
                    warn!(
                        "ratios length ({}) != children count ({}); fixing",
                        rs.len(),
                        n
                    );
                    rs.resize(n, 1.0 / n as f64);
                }
                // Clamp individual values to [0.01, 1.0].
                for r in rs.iter_mut() {
                    *r = r.clamp(0.01, 1.0);
                }
                // Normalize to sum ~1.0. `1e-6` (not `f64::EPSILON`, ~2.2e-16)
                // so trivial float drift does not trigger a needless rescale.
                let sum: f64 = rs.iter().sum();
                if sum > 0.0 && (sum - 1.0).abs() > 1e-6 {
                    for r in rs.iter_mut() {
                        *r /= sum;
                    }
                }
                // Re-clamp: normalization can push a value back below the 0.01
                // floor (e.g. one near-1.0 ratio among many children). The floor
                // is the invariant we guarantee; the renderer re-normalizes
                // proportionally at paint time.
                for r in rs.iter_mut() {
                    *r = r.clamp(0.01, 1.0);
                }
            }
            // Note: children were already validated in the budget-bounded
            // recursion above, so there is no separate recurse pass here.
        }
        LayoutNode::Pane {
            ref mut surfaces, ..
        } => {
            // U-008 / issue #30: bound tabs per pane - a pane is one leaf in
            // the tree (so the leaf budget does not catch it), but each
            // non-markdown surface still spawns a real terminal on restore.
            if surfaces.len() > MAX_PANE_SURFACES {
                warn!(
                    "pane has {} surfaces (cap {MAX_PANE_SURFACES}); truncating",
                    surfaces.len()
                );
                surfaces.truncate(MAX_PANE_SURFACES);
            }
            if surfaces.is_empty() {
                warn!("pane has no surfaces; adding a default surface");
                surfaces.push(Default::default());
            }
            let mut focus_seen = false;
            for surface in surfaces.iter_mut() {
                if surface.focus == Some(true) {
                    if focus_seen {
                        warn!("pane has multiple focused surfaces; dropping extra focus flag");
                        surface.focus = None;
                    } else {
                        focus_seen = true;
                    }
                }
            }
            *leaf_budget = leaf_budget.saturating_sub(1);
        }
    }
}

/// A surface within a pane (terminal, browser, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SurfaceDefinition {
    /// Surface type identifier: "terminal", "browser", etc.
    pub surface_type: Option<String>,
    /// Display name for this surface.
    pub name: Option<String>,
    /// User-assigned custom name (US-013). When set, it overrides the
    /// auto-derived surface name everywhere (sidebar/IPC `surface.list`/MCP),
    /// and survives restart via this field. Cleared by renaming to empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    /// Shell command to run in this surface.
    pub command: Option<String>,
    /// Prompt text to prefill after launching an agent command.
    ///
    /// Kept optional so session persistence and plain command panes do not
    /// carry template-only state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Working directory override for this surface.
    pub cwd: Option<String>,
    /// File path for non-terminal surfaces such as markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Extra environment variables merged over `terminal.env`. The same
    /// protected-key and loader-key filtering applies at PTY spawn.
    pub env: Option<HashMap<String, String>>,
    /// Whether this surface should receive initial focus.
    pub focus: Option<bool>,
    /// Saved scrollback text (plain, ANSI stripped). Up to 4000 lines / 400K chars.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
    /// EP-005 US-013: stable tag of the agent CLI last detected in this
    /// surface's PTY subtree (e.g. `"claude_code"`), so the identity pill
    /// survives restart as a dimmed "last known" until the first scan
    /// confirms it. Whitelisted at ingress against the known agent tags;
    /// unknown or malformed values are dropped silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// EP-006 US-019: per-pane font-size override in points. `None` =
    /// follow the global config. Validated at restore ingress (NaN/inf
    /// dropped, finite values clamped to [8.0, 32.0]) - never fed raw to
    /// the cell geometry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
}

impl Default for SurfaceDefinition {
    fn default() -> Self {
        Self {
            surface_type: Some("terminal".to_string()),
            name: None,
            custom_name: None,
            command: None,
            prompt: None,
            cwd: None,
            path: None,
            env: None,
            focus: None,
            scrollback: None,
            agent: None,
            font_size: None,
        }
    }
}
