//! UNDO control: marks, BEgin/End groups and the Control/Auto settings of one drawing, kept on top
//! of its undo stack ([`DocState::undo`]). Every command records its step through
//! [`DocState::push_undo`], so marks, groups, Control and the stack limit apply to all of them
//! alike; a command never needs to know about them.

use crate::{DocState, Snapshot};

/// The most undo steps a drawing keeps; the oldest are dropped first.
pub const MAX_UNDO: usize = 20;
/// Additional storage retained by undo and redo, beyond live and saved geometry.
pub const DEFAULT_MEMORY: usize = 32 << 20;
/// The most marks a drawing keeps; the oldest are dropped first.
const MAX_MARKS: usize = 1000;

/// UNDO Control: how much undo information is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Control {
    /// Full undo (the default).
    #[default]
    All,
    /// Undo is off: nothing is recorded.
    None,
    /// Only the last step can be undone.
    One,
}

/// The UNDO settings and markers of one drawing.
#[derive(Clone, Debug)]
pub struct UndoCtl {
    pub memory_limit: usize,
    pub retained_bytes: usize,
    ledger: cadcraft_doc::memory::MemoryLedger,
    group_lost: bool,
    pub control: Control,
    /// UNDO Auto. Every command already records one undo step for all it does, so this is kept and
    /// reported (UNDOCTL) only.
    pub auto: bool,
    /// UNDO Control Combine: consecutive zooms and pans as one step. View changes record no undo
    /// steps, so this is kept and reported only.
    pub combine_view: bool,
    /// UNDO Control Layer: layer property changes as one step. Kept and reported only.
    pub combine_layer: bool,
    /// Undo-stack depths at each UNDO Mark, oldest first.
    pub marks: Vec<usize>,
    /// Undo-stack depth when the open BEgin group started.
    pub group: Option<usize>,
}

impl Default for UndoCtl {
    fn default() -> Self {
        UndoCtl {
            memory_limit: DEFAULT_MEMORY,
            retained_bytes: 0,
            ledger: Default::default(),
            group_lost: false,
            control: Control::All,
            auto: true,
            combine_view: true,
            combine_layer: true,
            marks: Vec::new(),
            group: None,
        }
    }
}

impl UndoCtl {
    /// The UNDOCTL bit code: 1 undo on, 2 only one step, 4 Auto, 8 a group is open, 16 combine
    /// zooms and pans, 32 combine layer changes.
    pub fn bits(&self) -> i64 {
        let mut b = 0;
        if self.control != Control::None {
            b |= 1;
        }
        if self.control == Control::One {
            b |= 2;
        }
        if self.auto {
            b |= 4;
        }
        if self.group.is_some() {
            b |= 8;
        }
        if self.combine_view {
            b |= 16;
        }
        if self.combine_layer {
            b |= 32;
        }
        b
    }
}

impl DocState {
    /// Record an undo step (`snap` holds the drawing as it was before the change) and clear redo.
    /// UNDO Control None records nothing and One keeps just this step; the stack holds at most
    /// [`MAX_UNDO`] steps.
    pub fn push_undo(&mut self, snap: Snapshot) {
        self.redo.clear();
        match self.undo_ctl.control {
            Control::None => {
                self.undo.clear();
                self.undo_ctl.retained_bytes = 0;
                self.undo_ctl.ledger = Default::default();
                return;
            }
            Control::One => {
                self.undo.clear();
                self.undo.push(snap);
                self.trim_undo_memory();
                return;
            }
            Control::All => {}
        }
        // Marks above the current depth were undone; a new step makes them unreachable for good.
        let depth = self.undo.len();
        self.undo_ctl.marks.retain(|m| *m <= depth);
        if let Some(g) = self.undo_ctl.group
            && g > depth
        {
            self.undo_ctl.group = Some(depth);
        }
        // An open group is one logical action, even when a script executes hundreds of
        // commands. Keep its starting snapshot instead of letting the 20-step cap evict it.
        if !self.undo_ctl.group_lost && self.undo_ctl.group.is_none_or(|start| self.undo.len() <= start) {
            self.undo.push(snap);
        }
        self.trim_undo_memory();
    }

    /// Keep the newest reachable steps within both limits. Shared COW entities, chunks,
    /// indexes, blocks and foreign data are counted once, including live/saved baselines.
    pub fn trim_undo_memory(&mut self) {
        if self.undo.is_empty() && self.redo.is_empty() {
            self.undo_ctl.retained_bytes = 0;
            self.undo_ctl.ledger = Default::default();
            return;
        }
        let mut usage = self.undo_ctl.ledger.begin();
        usage.exclude(&self.doc);
        usage.exclude(&self.saved);
        let mut retained = 0usize;
        let mut keep_undo = 0;
        let mut keep_redo = 0;
        // Redo's nearest state is its last element, as is undo's. Keep the next available
        // action in either direction first, then walk outwards without leaving gaps.
        'steps: for i in 0..MAX_UNDO {
            for (stack, keep) in [(&self.undo, &mut keep_undo), (&self.redo, &mut keep_redo)] {
                if i != *keep {
                    continue;
                }
                let Some(snap) = stack.len().checked_sub(i + 1).and_then(|at| stack.get(at)) else { continue };
                let before = usage.bytes;
                usage.drawing(&snap.doc);
                let extra = usage
                    .bytes
                    .saturating_sub(before)
                    .saturating_add(snap.label.capacity())
                    .saturating_add(snap.selection.capacity() * std::mem::size_of::<cadcraft_doc::Handle>())
                    .saturating_add(std::mem::size_of::<Snapshot>());
                if extra <= self.undo_ctl.memory_limit.saturating_sub(retained) {
                    retained = retained.saturating_add(extra);
                    *keep += 1;
                } else {
                    // Once a snapshot cannot fit, its older neighbours cannot be reached.
                    // Stop this measurement rather than allowing its visited allocations to
                    // make the other stack look free.
                    break 'steps;
                }
            }
            if i >= keep_undo && i >= keep_redo {
                break;
            }
        }
        self.undo_ctl.retained_bytes = retained;
        let extra = self.undo.len().saturating_sub(keep_undo);
        if extra > 0 {
            self.undo.drain(..extra);
            for m in &mut self.undo_ctl.marks {
                *m = m.saturating_sub(extra);
            }
            if let Some(g) = &mut self.undo_ctl.group {
                self.undo_ctl.group_lost |= *g < extra;
                *g = g.saturating_sub(extra);
            }
        }
        let extra = self.redo.len().saturating_sub(keep_redo);
        self.redo.drain(..extra);
        self.undo_ctl.ledger.prune();
    }

    /// UNDO Mark at the current depth. Returns the number of marks.
    pub fn undo_mark(&mut self) -> usize {
        let depth = self.undo.len();
        let marks = &mut self.undo_ctl.marks;
        marks.retain(|m| *m <= depth);
        marks.push(depth);
        if marks.len() > MAX_MARKS {
            marks.remove(0);
        }
        marks.len()
    }

    /// The marks still reachable from the current depth.
    pub fn undo_marks(&self) -> usize {
        let depth = self.undo.len();
        self.undo_ctl.marks.iter().filter(|m| **m <= depth).count()
    }

    /// UNDO BEgin: the steps from here to UNDO End become one. BEgin while a group is open ends that
    /// group first (returns true then).
    pub fn begin_undo_group(&mut self) -> bool {
        let ended = self.end_undo_group();
        self.undo_ctl.group_lost = false;
        self.undo_ctl.group = Some(self.undo.len());
        ended
    }

    /// UNDO End (also on save): collapse the steps since BEgin into one undo step. Returns whether
    /// a group was open; End with no open group does nothing.
    pub fn end_undo_group(&mut self) -> bool {
        let Some(start) = self.undo_ctl.group.take() else { return false };
        self.undo_ctl.group_lost = false;
        // The first step of the group holds the drawing as it was at BEgin.
        if self.undo.len() > start {
            self.undo.truncate(start + 1);
            if let Some(first) = self.undo.get_mut(start) {
                first.label = "Group".into();
            }
        }
        // States inside the group no longer exist.
        self.undo_ctl.marks.retain(|m| *m <= start);
        self.trim_undo_memory();
        true
    }

    /// UNDO Control: All, None (drops all undo information) or One (keeps only the last step).
    /// Any open group and all marks are dropped when leaving All.
    pub fn set_undo_control(&mut self, c: Control) {
        self.undo_ctl.control = c;
        if c == Control::All {
            return;
        }
        self.undo_ctl.group = None;
        self.undo_ctl.group_lost = false;
        self.undo_ctl.marks.clear();
        self.redo.clear();
        let keep = if c == Control::One { 1 } else { 0 };
        let drop = self.undo.len().saturating_sub(keep);
        self.undo.drain(..drop);
        self.trim_undo_memory();
    }
}
