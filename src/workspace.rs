//! Tabs and the pane (panel) tree — the model the visual menu manipulates.
//!
//! This is structure only: every leaf is a pane id, and the app owns one live terminal
//! per leaf (see `App::reconcile_terms`). The model never spawns or drops terminals
//! itself — it just mutates the tree, and the app diffs `all_leaves()` against its live
//! terminals to keep them in sync.

use egui::{pos2, vec2, Rect};

pub type PaneId = u64;

/// How a pane is split. `Cols` puts children side-by-side (a vertical divider);
/// `Rows` stacks them (a horizontal divider).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Cols,
    Rows,
}

/// A binary layout tree. Leaves are panes; internal nodes are splits. Each branch carries a
/// stable id so the divider between its children can be addressed for drag-to-resize.
pub enum Node {
    Leaf(PaneId),
    Branch {
        id: u64,
        split: Split,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// Gap between panes, in points — leaves room to draw (and grab) the divider.
pub const GAP: f32 = 3.0;

/// A draggable split divider: which branch it belongs to, the gap strip to draw/grab, the
/// branch's full area (to turn a pointer position into a ratio), and the split orientation.
pub struct Divider {
    pub id: u64,
    pub rect: Rect,
    pub area: Rect,
    pub split: Split,
}

fn cut(area: Rect, split: Split, ratio: f32) -> (Rect, Rect) {
    match split {
        Split::Cols => {
            let w = ((area.width() - GAP) * ratio).max(0.0);
            let a = Rect::from_min_size(area.min, vec2(w, area.height()));
            let b = Rect::from_min_size(
                pos2(area.min.x + w + GAP, area.min.y),
                vec2((area.width() - w - GAP).max(0.0), area.height()),
            );
            (a, b)
        }
        Split::Rows => {
            let h = ((area.height() - GAP) * ratio).max(0.0);
            let a = Rect::from_min_size(area.min, vec2(area.width(), h));
            let b = Rect::from_min_size(
                pos2(area.min.x, area.min.y + h + GAP),
                vec2(area.width(), (area.height() - h - GAP).max(0.0)),
            );
            (a, b)
        }
    }
}

impl Node {
    fn leaf_rects(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, area)),
            Node::Branch { split, ratio, a, b, .. } => {
                let (ra, rb) = cut(area, *split, *ratio);
                a.leaf_rects(ra, out);
                b.leaf_rects(rb, out);
            }
        }
    }

    /// Collect each branch's divider strip (the gap between its children) and its area.
    fn dividers(&self, area: Rect, out: &mut Vec<Divider>) {
        if let Node::Branch { id, split, ratio, a, b } = self {
            let (ra, rb) = cut(area, *split, *ratio);
            let rect = match split {
                Split::Cols => Rect::from_min_max(pos2(ra.max.x, area.min.y), pos2(rb.min.x, area.max.y)),
                Split::Rows => Rect::from_min_max(pos2(area.min.x, ra.max.y), pos2(area.max.x, rb.min.y)),
            };
            out.push(Divider { id: *id, rect, area, split: *split });
            a.dividers(ra, out);
            b.dividers(rb, out);
        }
    }

    /// Set the ratio of the branch with `id` (no-op if not found). Clamped to keep both panes usable.
    fn set_ratio(&mut self, id: u64, ratio: f32) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Branch { id: bid, ratio: r, a, b, .. } => {
                if *bid == id {
                    *r = ratio.clamp(0.05, 0.95);
                    true
                } else {
                    a.set_ratio(id, ratio) || b.set_ratio(id, ratio)
                }
            }
        }
    }

    /// Replace leaf `target` with a split of itself and a fresh `new_id` pane (branch = `branch_id`).
    fn split_leaf(&mut self, target: PaneId, split: Split, new_id: PaneId, branch_id: u64) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                let old = *id;
                *self = Node::Branch {
                    id: branch_id,
                    split,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(old)),
                    b: Box::new(Node::Leaf(new_id)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Branch { a, b, .. } => {
                a.split_leaf(target, split, new_id, branch_id)
                    || b.split_leaf(target, split, new_id, branch_id)
            }
        }
    }

    /// Drop `target`, collapsing its parent into the surviving sibling.
    /// Returns `None` if the whole subtree was the target.
    fn without(self, target: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(id) if id == target => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Branch { id, split, ratio, a, b } => {
                match (a.without(target), b.without(target)) {
                    (Some(a), Some(b)) => Some(Node::Branch {
                        id,
                        split,
                        ratio,
                        a: Box::new(a),
                        b: Box::new(b),
                    }),
                    (Some(n), None) | (None, Some(n)) => Some(n),
                    (None, None) => None,
                }
            }
        }
    }

    fn first_leaf(&self) -> PaneId {
        match self {
            Node::Leaf(id) => *id,
            Node::Branch { a, .. } => a.first_leaf(),
        }
    }

    fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(i) => *i == id,
            Node::Branch { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Branch { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
        }
    }
}

pub struct Tab {
    pub title: String,
    pub layout: Node,
    pub focus: PaneId,
}

pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_id: PaneId,
}

impl Workspace {
    pub fn new() -> Self {
        let home = 0;
        Self {
            tabs: vec![Tab {
                title: "1".into(),
                layout: Node::Leaf(home),
                focus: home,
            }],
            active: 0,
            next_id: 1,
        }
    }

    fn alloc(&mut self) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn leaf_rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.active_tab().layout.leaf_rects(area, &mut out);
        out
    }

    /// The active tab's draggable split dividers.
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut out = Vec::new();
        self.active_tab().layout.dividers(area, &mut out);
        out
    }

    /// Resize a split by setting its branch ratio (drag-to-resize).
    pub fn set_ratio(&mut self, id: u64, ratio: f32) {
        self.tabs[self.active].layout.set_ratio(id, ratio);
    }

    /// Every pane id across all tabs — the set the app keeps a live terminal for.
    pub fn all_leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            tab.layout.collect_leaves(&mut out);
        }
        out
    }

    /// Forcibly drop a pane wherever it lives (its terminal's shell exited), collapsing its
    /// parent into the sibling and removing the tab if it was the last pane. Unlike
    /// `close_focused`, this keeps no minimum — emptying the workspace is allowed (the app
    /// then exits). No-op if the id is unknown (already gone).
    pub fn remove_pane(&mut self, id: PaneId) {
        let Some(ti) = self.tabs.iter().position(|t| t.layout.contains(id)) else {
            return;
        };
        let taken = std::mem::replace(&mut self.tabs[ti].layout, Node::Leaf(id));
        match taken.without(id) {
            Some(remaining) => {
                let tab = &mut self.tabs[ti];
                if tab.focus == id {
                    tab.focus = remaining.first_leaf();
                }
                tab.layout = remaining;
            }
            None => {
                self.tabs.remove(ti);
                if self.active >= self.tabs.len() {
                    self.active = self.tabs.len().saturating_sub(1);
                }
            }
        }
    }

    pub fn split(&mut self, split: Split) {
        let id = self.alloc();
        let branch = self.alloc();
        let focus = self.tabs[self.active].focus;
        if self.tabs[self.active].layout.split_leaf(focus, split, id, branch) {
            // Focus the new pane so repeated splits chain naturally.
            self.tabs[self.active].focus = id;
        }
    }

    pub fn focus(&mut self, pane: PaneId) {
        self.tabs[self.active].focus = pane;
    }

    pub fn close_focused(&mut self) {
        let tab = &mut self.tabs[self.active];
        let focus = tab.focus;
        let taken = std::mem::replace(&mut tab.layout, Node::Leaf(focus));
        match taken.without(focus) {
            Some(remaining) => {
                let new_focus = remaining.first_leaf();
                tab.layout = remaining;
                tab.focus = new_focus;
            }
            None => {
                // Closed the last pane in the tab → close the tab.
                self.close_tab(self.active);
            }
        }
    }

    pub fn new_tab(&mut self) {
        let id = self.alloc();
        let title = (self.tabs.len() + 1).to_string();
        self.tabs.push(Tab {
            title,
            layout: Node::Leaf(id),
            focus: id,
        });
        self.active = self.tabs.len() - 1;
    }

    pub fn close_tab(&mut self, idx: usize) {
        if self.tabs.len() <= 1 {
            return; // keep at least one tab
        }
        self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }
}
