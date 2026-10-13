//! The node tree and box-model layout.
//!
//! An arena-allocated tree rebuilt from scratch each frame: the
//! immediate-mode API in [`super::Context`] appends nodes as it walks the
//! caller's UI code, and the previous frame's tree is kept alongside so
//! layout and input can be resolved against where things actually were.
//!
//! Nothing here knows about widgets. Node content is a plain enum and the
//! layout is generic box-model work -- sizing, padding, positioning --
//! which is why it separates cleanly from the widget API that builds it.

use stdext::arena::Arena;
use stdext::collections::BString;

use crate::cell::SemiRefCell;

use super::*;

/// See [`Tree::visit_all`].
#[derive(Clone, Copy)]
pub(super) enum VisitControl {
    Continue,
    SkipChildren,
    Stop,
}

/// Stores the root of the "DOM" tree of the UI.
pub(super) struct Tree<'a> {
    pub(super) tail: &'a NodeCell<'a>,
    pub(super) root_first: &'a NodeCell<'a>,
    pub(super) root_last: &'a NodeCell<'a>,
    pub(super) last_node: &'a NodeCell<'a>,
    pub(super) current_node: &'a NodeCell<'a>,

    pub(super) count: usize,
    pub(super) checksum: u64,
}

impl<'a> Tree<'a> {
    /// Creates a new tree inside the given arena.
    /// A single root node is added for the main contents.
    pub(super) fn new(arena: &'a Arena) -> Self {
        let root = Self::alloc_node(arena);
        {
            let mut r = root.borrow_mut();
            r.id = ROOT_ID;
            r.classname = "root";
            r.attributes.focusable = true;
            r.attributes.focus_well = true;
        }
        Self {
            tail: root,
            root_first: root,
            root_last: root,
            last_node: root,
            current_node: root,
            count: 1,
            checksum: ROOT_ID,
        }
    }

    pub(super) fn alloc_node(arena: &'a Arena) -> &'a NodeCell<'a> {
        arena.alloc_uninit().write(Default::default())
    }

    /// Appends a child node to the current node.
    pub(super) fn push_child(&mut self, node: &'a NodeCell<'a>) {
        let mut n = node.borrow_mut();
        n.parent = Some(self.current_node);
        n.stack_parent = Some(self.current_node);

        {
            let mut p = self.current_node.borrow_mut();
            n.siblings.prev = p.children.last;
            n.depth = p.depth + 1;

            if let Some(child_last) = p.children.last {
                let mut child_last = child_last.borrow_mut();
                child_last.siblings.next = Some(node);
            }
            if p.children.first.is_none() {
                p.children.first = Some(node);
            }
            p.children.last = Some(node);
            p.child_count += 1;
        }

        n.prev = Some(self.tail);
        {
            let mut tail = self.tail.borrow_mut();
            tail.next = Some(node);
        }
        self.tail = node;

        self.last_node = node;
        self.current_node = node;
        self.count += 1;
        // wymix is weak, but both checksum and node.id are proper random, so... it's not *that* bad.
        self.checksum = wymix(self.checksum, n.id);
    }

    /// Removes the current node from its parent and appends it as a new root.
    /// Used for [`Context::attr_float`].
    pub(super) fn move_node_to_root(
        &mut self,
        node: &'a NodeCell<'a>,
        anchor: Option<&'a NodeCell<'a>>,
    ) {
        let mut n = node.borrow_mut();
        let Some(parent) = n.parent else {
            return;
        };

        if let Some(sibling_prev) = n.siblings.prev {
            let mut sibling_prev = sibling_prev.borrow_mut();
            sibling_prev.siblings.next = n.siblings.next;
        }
        if let Some(sibling_next) = n.siblings.next {
            let mut sibling_next = sibling_next.borrow_mut();
            sibling_next.siblings.prev = n.siblings.prev;
        }

        {
            let mut p = parent.borrow_mut();
            if opt_ptr_eq(p.children.first, Some(node)) {
                p.children.first = n.siblings.next;
            }
            if opt_ptr_eq(p.children.last, Some(node)) {
                p.children.last = n.siblings.prev;
            }
            p.child_count -= 1;
        }

        n.parent = anchor;
        n.depth = anchor.map_or(0, |n| n.borrow().depth + 1);
        n.siblings.prev = Some(self.root_last);
        n.siblings.next = None;

        self.root_last.borrow_mut().siblings.next = Some(node);
        self.root_last = node;
    }

    /// Completes the current node and moves focus to the parent.
    pub(super) fn pop_stack(&mut self) {
        let current_node = self.current_node.borrow();
        if let Some(stack_parent) = current_node.stack_parent {
            self.last_node = self.current_node;
            self.current_node = stack_parent;
        }
    }

    pub(super) fn iterate_siblings(
        mut node: Option<&'a NodeCell<'a>>,
    ) -> impl Iterator<Item = &'a NodeCell<'a>> + use<'a> {
        iter::from_fn(move || {
            let n = node?;
            node = n.borrow().siblings.next;
            Some(n)
        })
    }

    pub(super) fn iterate_siblings_rev(
        mut node: Option<&'a NodeCell<'a>>,
    ) -> impl Iterator<Item = &'a NodeCell<'a>> + use<'a> {
        iter::from_fn(move || {
            let n = node?;
            node = n.borrow().siblings.prev;
            Some(n)
        })
    }

    pub(super) fn iterate_roots(&self) -> impl Iterator<Item = &'a NodeCell<'a>> + use<'a> {
        Self::iterate_siblings(Some(self.root_first))
    }

    pub(super) fn iterate_roots_rev(&self) -> impl Iterator<Item = &'a NodeCell<'a>> + use<'a> {
        Self::iterate_siblings_rev(Some(self.root_last))
    }

    /// Visits all nodes under and including `root` in depth order.
    /// Starts with node `start`.
    ///
    /// WARNING: Breaks in hilarious ways if `start` is not within `root`.
    pub(super) fn visit_all<T: FnMut(&'a NodeCell<'a>) -> VisitControl>(
        root: &'a NodeCell<'a>,
        start: &'a NodeCell<'a>,
        forward: bool,
        mut cb: T,
    ) {
        let root_depth = root.borrow().depth;
        let mut node = start;
        let children_idx = if forward { NodeChildren::FIRST } else { NodeChildren::LAST };
        let siblings_idx = if forward { NodeSiblings::NEXT } else { NodeSiblings::PREV };

        while {
            'traverse: {
                match cb(node) {
                    VisitControl::Continue => {
                        // Depth first search: It has a child? Go there.
                        if let Some(child) = node.borrow().children.get(children_idx) {
                            node = child;
                            break 'traverse;
                        }
                    }
                    VisitControl::SkipChildren => {}
                    VisitControl::Stop => return,
                }

                loop {
                    // If we hit the root while going up, we restart the traversal at
                    // `root` going down again until we hit `start` again.
                    let n = node.borrow();
                    if n.depth <= root_depth {
                        break 'traverse;
                    }

                    // Go to the parent's next sibling. --> Next subtree.
                    if let Some(sibling) = n.siblings.get(siblings_idx) {
                        node = sibling;
                        break;
                    }

                    // Out of children? Go back to the parent.
                    node = n.parent.unwrap();
                }
            }

            // We're done once we wrapped around to the `start`.
            !ptr::eq(node, start)
        } {}
    }
}

/// A hashmap of node IDs to nodes.
///
/// This map uses a simple open addressing scheme with linear probing.
/// It's fast, simple, and sufficient for the small number of nodes we have.
pub(super) struct NodeMap<'a> {
    pub(super) slots: &'a [Option<&'a NodeCell<'a>>],
    pub(super) shift: usize,
    pub(super) mask: u64,
}

impl Default for NodeMap<'static> {
    fn default() -> Self {
        Self { slots: &[None, None], shift: 63, mask: 0 }
    }
}

impl<'a> NodeMap<'a> {
    /// Creates a new node map for the given tree.
    pub(super) fn new(arena: &'a Arena, tree: &Tree<'a>) -> Self {
        // Since we aren't expected to have millions of nodes,
        // we allocate 4x the number of slots for a 25% fill factor.
        let width = (4 * tree.count + 1).ilog2().max(1) as usize;
        let slots = 1 << width;
        let shift = 64 - width;
        let mask = (slots - 1) as u64;

        let slots = arena.alloc_slice(slots, None);
        let mut node = tree.root_first;

        loop {
            let n = node.borrow();
            let mut slot = n.id >> shift;

            loop {
                if slots[slot as usize].is_none() {
                    slots[slot as usize] = Some(node);
                    break;
                }
                slot = (slot + 1) & mask;
            }

            node = match n.next {
                Some(node) => node,
                None => break,
            };
        }

        Self { slots, shift, mask }
    }

    /// Gets a node by its ID.
    pub(super) fn get(&self, id: u64) -> Option<&'a NodeCell<'a>> {
        let shift = self.shift;
        let mask = self.mask;
        let mut slot = id >> shift;

        loop {
            let node = self.slots[slot as usize]?;
            if node.borrow().id == id {
                return Some(node);
            }
            slot = (slot + 1) & mask;
        }
    }
}

pub(super) struct FloatAttributes {
    // Specifies the origin of the container relative to the container size. [0, 1]
    pub(super) gravity_x: f32,
    pub(super) gravity_y: f32,
    // Specifies an offset from the origin in cells.
    pub(super) offset_x: f32,
    pub(super) offset_y: f32,
}

/// NOTE: Must not contain items that require drop().
#[derive(Default)]
pub(super) struct NodeAttributes {
    pub(super) float: Option<FloatAttributes>,
    pub(super) position: Position,
    pub(super) padding: Rect,
    pub(super) bg: StraightRgba,
    pub(super) fg: StraightRgba,
    pub(super) reverse: bool,
    pub(super) bordered: bool,
    pub(super) focusable: bool,
    pub(super) focus_well: bool, // Prevents focus from leaving via Tab
    pub(super) focus_void: bool, // Prevents focus from entering via Tab
    pub(super) clickable: bool,  // Gets a hover highlight and a pointer-hand mouse cursor
    /// When set, the node's visible height grows from 0 to its full layout
    /// height over a short window when it first appears. Used for menubar
    /// dropdowns. The clip is applied recursively to descendants at render
    /// time so children peeking out the bottom are hidden mid-animation.
    pub(super) slide_down: bool,
    /// Like `slide_down`, but the visible band grows out from the centre
    /// (top and bottom edges expand symmetrically). Used for modals so they
    /// don't appear to slide in from above the viewport.
    pub(super) scale_in: bool,
}

/// NOTE: Must not contain items that require drop().
pub(super) struct ListContent<'a> {
    pub(super) selected: u64,
    // Points to the Node that holds this ListContent instance, if any>.
    pub(super) selected_node: Option<&'a NodeCell<'a>>,
}

/// NOTE: Must not contain items that require drop().
pub(super) struct TableContent<'a> {
    pub(super) columns: BVec<'a, CoordType>,
    pub(super) cell_gap: Size,
}

/// NOTE: Must not contain items that require drop().
pub(super) struct StyledTextChunk {
    pub(super) offset: usize,
    pub(super) fg: StraightRgba,
    pub(super) attr: Attributes,
}

pub(super) const INVALID_STYLED_TEXT_CHUNK: StyledTextChunk =
    StyledTextChunk { offset: usize::MAX, fg: StraightRgba::zero(), attr: Attributes::None };

/// NOTE: Must not contain items that require drop().
pub(super) struct TextContent<'a> {
    pub(super) text: BString<'a>,
    pub(super) chunks: BVec<'a, StyledTextChunk>,
    pub(super) overflow: Overflow,
}

/// NOTE: Must not contain items that require drop().
pub(super) struct TextareaContent<'a> {
    pub(super) buffer: &'a TextBufferCell,

    // Carries over between frames.
    pub(super) scroll_offset: Point,
    pub(super) scroll_offset_y_drag_start: CoordType,
    pub(super) scroll_offset_x_max: CoordType,
    pub(super) thumb_height: CoordType,

    pub(super) single_line: bool,
    pub(super) has_focus: bool,
}

/// NOTE: Must not contain items that require drop().
#[derive(Clone)]
pub(super) struct ScrollareaContent {
    pub(super) scroll_offset: Point,
    pub(super) scroll_offset_y_drag_start: CoordType,
    pub(super) thumb_height: CoordType,
}

/// NOTE: Must not contain items that require drop().
#[derive(Default)]
pub(super) enum NodeContent<'a> {
    #[default]
    None,
    List(ListContent<'a>),
    Modal(BString<'a>), // title
    Table(TableContent<'a>),
    Text(TextContent<'a>),
    Textarea(TextareaContent<'a>),
    Scrollarea(ScrollareaContent),
}

/// NOTE: Must not contain items that require drop().
#[derive(Default)]
pub(super) struct NodeSiblings<'a> {
    pub(super) prev: Option<&'a NodeCell<'a>>,
    pub(super) next: Option<&'a NodeCell<'a>>,
}

impl<'a> NodeSiblings<'a> {
    pub(super) const PREV: usize = 0;
    pub(super) const NEXT: usize = 1;

    pub(super) fn get(&self, off: usize) -> Option<&'a NodeCell<'a>> {
        match off & 1 {
            0 => self.prev,
            1 => self.next,
            _ => unreachable!(),
        }
    }
}

/// NOTE: Must not contain items that require drop().
#[derive(Default)]
pub(super) struct NodeChildren<'a> {
    pub(super) first: Option<&'a NodeCell<'a>>,
    pub(super) last: Option<&'a NodeCell<'a>>,
}

impl<'a> NodeChildren<'a> {
    pub(super) const FIRST: usize = 0;
    pub(super) const LAST: usize = 1;

    pub(super) fn get(&self, off: usize) -> Option<&'a NodeCell<'a>> {
        match off & 1 {
            0 => self.first,
            1 => self.last,
            _ => unreachable!(),
        }
    }
}

pub(super) type NodeCell<'a> = SemiRefCell<Node<'a>>;

/// A node in the UI tree.
///
/// NOTE: Must not contain items that require drop().
#[derive(Default)]
pub(super) struct Node<'a> {
    pub(super) prev: Option<&'a NodeCell<'a>>,
    pub(super) next: Option<&'a NodeCell<'a>>,
    pub(super) stack_parent: Option<&'a NodeCell<'a>>,

    pub(super) id: u64,
    pub(super) classname: &'static str,
    pub(super) parent: Option<&'a NodeCell<'a>>,
    pub(super) depth: usize,
    pub(super) siblings: NodeSiblings<'a>,
    pub(super) children: NodeChildren<'a>,
    pub(super) child_count: usize,

    pub(super) attributes: NodeAttributes,
    pub(super) content: NodeContent<'a>,

    pub(super) intrinsic_size: Size,
    pub(super) intrinsic_size_set: bool,
    pub(super) outer: Rect, // in screen-space, calculated during layout
    pub(super) inner: Rect, // in screen-space, calculated during layout
    pub(super) outer_clipped: Rect, // in screen-space, calculated during layout, restricted to the viewport
    pub(super) inner_clipped: Rect, // in screen-space, calculated during layout, restricted to the viewport
}

impl<'a> Node<'a> {
    /// Given an outer rectangle (including padding and borders) of this node,
    /// this returns the inner rectangle (excluding padding and borders).
    pub(super) fn outer_to_inner(&self, mut outer: Rect) -> Rect {
        let l = self.attributes.bordered;
        let t = self.attributes.bordered;
        let r = self.attributes.bordered || matches!(self.content, NodeContent::Scrollarea(..));
        let b = self.attributes.bordered;

        outer.left += self.attributes.padding.left + l as CoordType;
        outer.top += self.attributes.padding.top + t as CoordType;
        outer.right -= self.attributes.padding.right + r as CoordType;
        outer.bottom -= self.attributes.padding.bottom + b as CoordType;

        // Nothing clamps this subtraction, so a node laid out narrower than its
        // own padding + border comes out inside-out. `Rect::intersect` would
        // then quietly flatten it to empty, and the widget draws into a
        // degenerate rect -- the zero-height-track class of bug.
        crate::sanity_check!(
            node_inner_rect_not_inverted,
            outer.left <= outer.right && outer.top <= outer.bottom,
            "inner={outer:?} padding={:?} bordered={}",
            self.attributes.padding,
            self.attributes.bordered
        );

        outer
    }

    /// Given an intrinsic size (excluding padding and borders) of this node,
    /// this returns the outer size (including padding and borders).
    pub(super) fn intrinsic_to_outer(&self) -> Size {
        let l = self.attributes.bordered;
        let t = self.attributes.bordered;
        let r = self.attributes.bordered || matches!(self.content, NodeContent::Scrollarea(..));
        let b = self.attributes.bordered;

        let mut size = self.intrinsic_size;
        size.width += self.attributes.padding.left
            + self.attributes.padding.right
            + l as CoordType
            + r as CoordType;
        size.height += self.attributes.padding.top
            + self.attributes.padding.bottom
            + t as CoordType
            + b as CoordType;
        size
    }

    /// Computes the intrinsic size of this node and its children.
    pub(super) fn compute_intrinsic_size(&mut self, arena: &'a Arena) {
        match &mut self.content {
            NodeContent::Table(spec) => {
                // Calculate each row's height and the maximum width of each of its columns.
                for row in Tree::iterate_siblings(self.children.first) {
                    let mut row = row.borrow_mut();
                    let mut row_height = 0;

                    for (column, cell) in Tree::iterate_siblings(row.children.first).enumerate() {
                        let mut cell = cell.borrow_mut();
                        cell.compute_intrinsic_size(arena);

                        let size = cell.intrinsic_to_outer();

                        // If the spec.columns[] value is positive, it's an absolute width.
                        // Otherwise, it's a fraction of the remaining space.
                        //
                        // TODO: The latter is computed incorrectly.
                        // Example: If the items are "a","b","c" then the intrinsic widths are [1,1,1].
                        // If the column spec is [0,-3,-1], then this code assigns an intrinsic row
                        // width of 3, but it should be 5 (1+1+3), because the spec says that the
                        // last column (flexible 1/1) must be 3 times as wide as the 2nd one (1/3rd).
                        // It's not a big deal yet, because such functionality isn't needed just yet.
                        if column >= spec.columns.len() {
                            spec.columns.push(arena, 0);
                        }
                        spec.columns[column] = spec.columns[column].max(size.width);

                        row_height = row_height.max(size.height);
                    }

                    row.intrinsic_size.height = row_height;
                }

                // Assuming each column has the width of the widest cell in that column,
                // calculate the total width of the table.
                let total_gap_width =
                    spec.cell_gap.width * spec.columns.len().saturating_sub(1) as CoordType;
                let total_inner_width = spec.columns.iter().sum::<CoordType>() + total_gap_width;
                let mut total_width = 0;
                let mut total_height = 0;

                // Assign the total width to each row.
                for row in Tree::iterate_siblings(self.children.first) {
                    let mut row = row.borrow_mut();
                    row.intrinsic_size.width = total_inner_width;
                    row.intrinsic_size_set = true;

                    let size = row.intrinsic_to_outer();
                    total_width = total_width.max(size.width);
                    total_height += size.height;
                }

                let total_gap_height =
                    spec.cell_gap.height * self.child_count.saturating_sub(1) as CoordType;
                total_height += total_gap_height;

                // Assign the total width/height to the table.
                if !self.intrinsic_size_set {
                    self.intrinsic_size.width = total_width;
                    self.intrinsic_size.height = total_height;
                    self.intrinsic_size_set = true;
                }
            }
            _ => {
                let mut max_width = 0;
                let mut total_height = 0;

                for child in Tree::iterate_siblings(self.children.first) {
                    let mut child = child.borrow_mut();
                    child.compute_intrinsic_size(arena);

                    let size = child.intrinsic_to_outer();
                    max_width = max_width.max(size.width);
                    total_height += size.height;
                }

                if !self.intrinsic_size_set {
                    self.intrinsic_size.width = max_width;
                    self.intrinsic_size.height = total_height;
                    self.intrinsic_size_set = true;
                }
            }
        }
    }

    /// Lays out the children of this node.
    /// The clip rect restricts "rendering" to a certain area (the viewport).
    pub(super) fn layout_children(&mut self, clip: Rect) {
        if self.children.first.is_none() || self.inner.is_empty() {
            return;
        }

        match &mut self.content {
            NodeContent::Table(spec) => {
                let width = self.inner.right - self.inner.left;
                let mut x = self.inner.left;
                let mut y = self.inner.top;

                for row in Tree::iterate_siblings(self.children.first) {
                    let mut row = row.borrow_mut();
                    let mut size = row.intrinsic_to_outer();
                    size.width = width;
                    row.outer.left = x;
                    row.outer.top = y;
                    row.outer.right = x + size.width;
                    row.outer.bottom = y + size.height;
                    row.outer = row.outer.intersect(self.inner);
                    row.inner = row.outer_to_inner(row.outer);
                    row.outer_clipped = row.outer.intersect(clip);
                    row.inner_clipped = row.inner.intersect(clip);

                    let mut row_height = 0;

                    for (column, cell) in Tree::iterate_siblings(row.children.first).enumerate() {
                        let mut cell = cell.borrow_mut();
                        let mut size = cell.intrinsic_to_outer();
                        size.width = spec.columns[column];
                        cell.outer.left = x;
                        cell.outer.top = y;
                        cell.outer.right = x + size.width;
                        cell.outer.bottom = y + size.height;
                        cell.outer = cell.outer.intersect(self.inner);
                        cell.inner = cell.outer_to_inner(cell.outer);
                        cell.outer_clipped = cell.outer.intersect(clip);
                        cell.inner_clipped = cell.inner.intersect(clip);

                        x += size.width + spec.cell_gap.width;
                        row_height = row_height.max(size.height);

                        cell.layout_children(clip);
                    }

                    x = self.inner.left;
                    y += row_height + spec.cell_gap.height;
                }
            }
            NodeContent::Scrollarea(sc) => {
                let mut content = self.children.first.unwrap().borrow_mut();

                // content available viewport size (-1 for the track)
                let sx = self.inner.right - self.inner.left;
                let sy = self.inner.bottom - self.inner.top;
                // actual content size
                let cx = sx;
                let cy = content.intrinsic_size.height.max(sy);
                // scroll offset
                let ox = 0;
                let oy = sc.scroll_offset.y.clamp(0, cy - sy);

                sc.scroll_offset.x = ox;
                sc.scroll_offset.y = oy;

                content.outer.left = self.inner.left - ox;
                content.outer.top = self.inner.top - oy;
                content.outer.right = content.outer.left + cx;
                content.outer.bottom = content.outer.top + cy;
                content.inner = content.outer_to_inner(content.outer);
                content.outer_clipped = content.outer.intersect(self.inner_clipped);
                content.inner_clipped = content.inner.intersect(self.inner_clipped);

                let clip = content.inner_clipped;
                content.layout_children(clip);
            }
            _ => {
                let width = self.inner.right - self.inner.left;
                let x = self.inner.left;
                let mut y = self.inner.top;

                for child in Tree::iterate_siblings(self.children.first) {
                    let mut child = child.borrow_mut();
                    let size = child.intrinsic_to_outer();
                    let remaining = (width - size.width).max(0);

                    child.outer.left = x + match child.attributes.position {
                        Position::Stretch | Position::Left => 0,
                        Position::Center => remaining / 2,
                        Position::Right => remaining,
                    };
                    child.outer.right = child.outer.left
                        + match child.attributes.position {
                            Position::Stretch => width,
                            _ => size.width,
                        };
                    child.outer.top = y;
                    child.outer.bottom = y + size.height;

                    child.outer = child.outer.intersect(self.inner);
                    child.inner = child.outer_to_inner(child.outer);
                    child.outer_clipped = child.outer.intersect(clip);
                    child.inner_clipped = child.inner.intersect(clip);

                    y += size.height;
                }

                for child in Tree::iterate_siblings(self.children.first) {
                    let mut child = child.borrow_mut();
                    child.layout_children(clip);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "sanity"))]
mod tests {
    use stdext::sanity::capture;

    use super::*;

    #[test]
    fn padding_wider_than_the_node_trips_the_inverted_rect_check() {
        let mut node = Node::default();
        node.attributes.padding = Rect::one(4);

        // 3 columns of room, 4 of padding either side.
        let (inner, msgs) =
            capture::trips(|| node.outer_to_inner(Rect { left: 0, top: 0, right: 3, bottom: 3 }));

        assert!(capture::fired(&msgs, "node_inner_rect_not_inverted"), "{msgs:?}");
        assert!(inner.right < inner.left, "expected an inside-out rect, got {inner:?}");
    }

    #[test]
    fn padding_that_fits_stays_quiet() {
        let mut node = Node::default();
        node.attributes.padding = Rect::one(1);

        let (inner, msgs) =
            capture::trips(|| node.outer_to_inner(Rect { left: 0, top: 0, right: 10, bottom: 10 }));

        assert!(msgs.is_empty(), "{msgs:?}");
        assert_eq!(inner, Rect { left: 1, top: 1, right: 9, bottom: 9 });
    }
}
