//! Tabs and the pane (panel) tree — the model the visual menu manipulates.
//!
//! This is structure only. There is exactly ONE live terminal for now; it lives in
//! `home_pane` (pane 0 of tab 0). The menu can create tabs and split panes freely,
//! and the terminal renders wherever its home pane currently sits. Every other pane
//! is an honest empty placeholder until each pane gets its own PTY — the next milestone.

use egui::{pos2, vec2, Rect};

pub type PaneId = u64;

/// How a pane is split. `Cols` puts children side-by-side (a vertical divider);
/// `Rows` stacks them (a horizontal divider).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Cols,
    Rows,
}

/// A binary layout tree. Leaves are panes; internal nodes are splits.
pub enum Node {
    Leaf(PaneId),
    Branch {
        split: Split,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// Gap between panes, in points — leaves room to draw the divider.
const GAP: f32 = 3.0;

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
            Node::Branch { split, ratio, a, b } => {
                let (ra, rb) = cut(area, *split, *ratio);
                a.leaf_rects(ra, out);
                b.leaf_rects(rb, out);
            }
        }
    }

    /// Replace leaf `target` with a split of itself and a fresh `new_id` pane.
    fn split_leaf(&mut self, target: PaneId, split: Split, new_id: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                let old = *id;
                *self = Node::Branch {
                    split,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(old)),
                    b: Box::new(Node::Leaf(new_id)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Branch { a, b, .. } => {
                a.split_leaf(target, split, new_id) || b.split_leaf(target, split, new_id)
            }
        }
    }

    /// Drop `target`, collapsing its parent into the surviving sibling.
    /// Returns `None` if the whole subtree was the target.
    fn without(self, target: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(id) if id == target => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Branch { split, ratio, a, b } => {
                match (a.without(target), b.without(target)) {
                    (Some(a), Some(b)) => Some(Node::Branch {
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
}

pub struct Tab {
    pub title: String,
    pub layout: Node,
    pub focus: PaneId,
}

pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active: usize,
    /// The single pane that currently owns the live terminal.
    pub home_pane: PaneId,
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
            home_pane: home,
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

    pub fn split(&mut self, split: Split) {
        let id = self.alloc();
        let focus = self.tabs[self.active].focus;
        if self.tabs[self.active].layout.split_leaf(focus, split, id) {
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
        // Refuse to close the live terminal's pane until panes own their own PTY.
        if focus == self.home_pane && self.active == 0 {
            return;
        }
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
