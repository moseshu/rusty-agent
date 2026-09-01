//! Anchor retention of `preserved_segment { head, anchor, tail }`.
//!
//! A summary is useful only if it is surrounded by the interactions that make the current task
//! actionable. This selector keeps a bounded head, a bounded set of recent caller-selected
//! anchors, and a bounded tail. It never truncates each region independently and then concatenates
//! duplicates: every retained item has one source index and belongs to exactly one segment.

use std::collections::BTreeSet;

use ra_core::error::{Error, Result};

/// Bounded retention policy for a compacted history.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorRetention {
    head_items: usize,
    max_anchor_items: usize,
    tail_items: usize,
}

impl AnchorRetention {
    /// Creates a policy that keeps at least one source region.
    pub fn new(head_items: usize, max_anchor_items: usize, tail_items: usize) -> Result<Self> {
        if head_items == 0 && max_anchor_items == 0 && tail_items == 0 {
            return Err(Error::config(
                "anchor retention must keep a head, anchor, or tail item",
            ));
        }
        Ok(Self {
            head_items,
            max_anchor_items,
            tail_items,
        })
    }

    /// Number of earliest history items retained without an anchor selection.
    #[must_use]
    pub const fn head_items(self) -> usize {
        self.head_items
    }

    /// Maximum number of caller-selected anchor items retained from the middle.
    #[must_use]
    pub const fn max_anchor_items(self) -> usize {
        self.max_anchor_items
    }

    /// Number of latest history items retained without an anchor selection.
    #[must_use]
    pub const fn tail_items(self) -> usize {
        self.tail_items
    }

    /// Largest number of items any history can retain under this policy.
    ///
    /// The three regions are disjoint, so their capacities add. This is what a compacted history
    /// costs before its summary item, and therefore what
    /// [`CompactionLimits::ensure_converges_with`](crate::compaction::CompactionLimits::ensure_converges_with)
    /// weighs against the item-count trigger.
    #[must_use]
    pub const fn max_retained_items(self) -> usize {
        self.head_items
            .saturating_add(self.max_anchor_items)
            .saturating_add(self.tail_items)
    }

    /// Selects head, recent anchor, and tail segments from one ordered history.
    ///
    /// `anchor_indices` are source indices chosen by product policy, such as the turns containing
    /// an unresolved user instruction or a still-relevant tool result. Repeated indices are
    /// harmless. If there are more middle anchors than the configured capacity, the newest ones
    /// win; head and tail coverage never consumes that capacity.
    pub fn preserve<T: Clone>(
        self,
        items: &[T],
        anchor_indices: impl IntoIterator<Item = usize>,
    ) -> Result<PreservedSegment<T>> {
        let mut requested = BTreeSet::new();
        for index in anchor_indices {
            if index >= items.len() {
                return Err(Error::caller(format!(
                    "anchor index {index} is outside a history of {} items",
                    items.len()
                )));
            }
            requested.insert(index);
        }

        let head_end = items.len().min(self.head_items);
        let tail_start = items.len().saturating_sub(self.tail_items).max(head_end);
        let anchors: Vec<usize> = requested
            .into_iter()
            .filter(|index| (*index >= head_end) && (*index < tail_start))
            .collect();
        let first_selected_anchor = anchors.len().saturating_sub(self.max_anchor_items);
        let selected_anchors = anchors.into_iter().skip(first_selected_anchor);

        Ok(PreservedSegment {
            head: (0..head_end)
                .map(|index| PreservedItem::new(index, items[index].clone()))
                .collect(),
            anchor: selected_anchors
                .map(|index| PreservedItem::new(index, items[index].clone()))
                .collect(),
            tail: (tail_start..items.len())
                .map(|index| PreservedItem::new(index, items[index].clone()))
                .collect(),
        })
    }
}

/// One retained item and its location in the pre-compaction history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedItem<T> {
    source_index: usize,
    item: T,
}

impl<T> PreservedItem<T> {
    fn new(source_index: usize, item: T) -> Self {
        Self { source_index, item }
    }

    /// Index in the original ordered history.
    #[must_use]
    pub const fn source_index(&self) -> usize {
        self.source_index
    }

    /// Retained value.
    #[must_use]
    pub const fn item(&self) -> &T {
        &self.item
    }
}

/// Ordered head, selected anchors, and tail retained across compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedSegment<T> {
    head: Vec<PreservedItem<T>>,
    anchor: Vec<PreservedItem<T>>,
    tail: Vec<PreservedItem<T>>,
}

impl<T> PreservedSegment<T> {
    /// Earliest retained history items.
    #[must_use]
    pub fn head(&self) -> &[PreservedItem<T>] {
        &self.head
    }

    /// Caller-selected middle items retained after head coverage.
    #[must_use]
    pub fn anchor(&self) -> &[PreservedItem<T>] {
        &self.anchor
    }

    /// Latest retained history items.
    #[must_use]
    pub fn tail(&self) -> &[PreservedItem<T>] {
        &self.tail
    }

    /// Number of unique source items retained by all three segments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.head.len() + self.anchor.len() + self.tail.len()
    }

    /// Whether every source segment is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterates retained items in their original chronological order.
    pub fn iter(&self) -> impl Iterator<Item = &PreservedItem<T>> {
        self.head.iter().chain(&self.anchor).chain(&self.tail)
    }
}
