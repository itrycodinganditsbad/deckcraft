//! The two-way map between DeckCraft's local `u32` ids and the live document's `ElemId`s, kept per session.
//!
//! Slides, shapes, layouts, masters and media have `u32` ids from [`Presentation::next_id`], so two machines adding a
//! shape at once would pick the same one. The live document keys them by random `ElemId`s instead; each session maps
//! its own local ids to them, gives elements another machine added fresh local ids, and translates references both
//! ways. Table rows and columns, comments, sections and custom shows have no ids at all: they get `ElemId`s by
//! position, carried from one model to the next by [`align`]. The map is rebuilt at open and never saved.

use std::collections::HashMap;

use craft_cloud_kit::ElemId;
use deckcraft_model::{Presentation, Shape, ShapeKind};

/// The ids of one session.
#[derive(Debug, Clone, Default)]
pub struct Ids {
    to_elem: HashMap<u32, ElemId>,
    to_local: HashMap<ElemId, u32>,
    /// A table shape's rows and columns, by the shape's element.
    pub(crate) rows: HashMap<ElemId, Vec<ElemId>>,
    pub(crate) cols: HashMap<ElemId, Vec<ElemId>>,
    /// A slide's comments, by the slide's element, in order.
    pub(crate) comments: HashMap<ElemId, Vec<ElemId>>,
    pub(crate) sections: Vec<ElemId>,
    pub(crate) shows: Vec<ElemId>,
}

impl Ids {
    pub fn elem(&self, local: u32) -> Option<ElemId> {
        self.to_elem.get(&local).copied()
    }

    pub fn local(&self, elem: ElemId) -> Option<u32> {
        self.to_local.get(&elem).copied()
    }

    /// The element of `local`, made if it has none yet (a local element the live document doesn't know).
    pub(crate) fn elem_or_new(&mut self, local: u32) -> ElemId {
        if let Some(e) = self.to_elem.get(&local) {
            return *e;
        }
        let e = ElemId::new();
        self.bind(local, e);
        e
    }

    /// The local id of `elem`, given a fresh one from `p` if it has none (an element another machine added).
    pub(crate) fn local_or_new(&mut self, elem: ElemId, p: &mut Presentation) -> Option<u32> {
        if let Some(l) = self.to_local.get(&elem) {
            return Some(*l);
        }
        let l = p.alloc_id()?;
        self.bind(l, elem);
        Some(l)
    }

    pub(crate) fn bind(&mut self, local: u32, elem: ElemId) {
        if let Some(old) = self.to_elem.insert(local, elem) {
            self.to_local.remove(&old);
        }
        if let Some(old) = self.to_local.insert(elem, local)
            && old != local
        {
            self.to_elem.remove(&old);
        }
    }

    pub fn len(&self) -> usize {
        self.to_elem.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_elem.is_empty()
    }

    /// Elements for every id of `shapes` (groups included).
    pub(crate) fn cover_shapes(&mut self, shapes: &[Shape]) {
        deckcraft_model::walk(shapes, &mut |s, _| {
            self.elem_or_new(s.id.0);
        });
    }

    /// Elements for every id in `p`.
    pub(crate) fn cover_all(&mut self, p: &Presentation) {
        for s in &p.slides {
            self.elem_or_new(s.id.0);
            self.cover_shapes(&s.shapes);
        }
        for m in p.masters.iter().chain(p.notes_master.iter()).chain(p.handout_master.iter()) {
            self.elem_or_new(m.id.0);
            self.cover_shapes(&m.shapes);
            for l in &m.layouts {
                self.elem_or_new(l.id.0);
                self.cover_shapes(&l.shapes);
            }
        }
        for m in &p.media {
            self.elem_or_new(m.id.0);
        }
    }
}

/// The elements of `new`'s items, carried over from `old`'s (`old_ids`, one per item) by position: the same count
/// keeps them in place; otherwise the longest run of equal items keeps theirs, and the rest get new ones.
pub(crate) fn align<T: PartialEq>(old: &[T], old_ids: &[ElemId], new: &[T]) -> Vec<ElemId> {
    let fresh = |i: usize| old_ids.get(i).copied().unwrap_or_else(ElemId::new);
    if old.len() == new.len() && old_ids.len() == old.len() {
        return old_ids.to_vec();
    }
    if old_ids.len() != old.len() || old.len().saturating_mul(new.len()) > 1_000_000 {
        // Out of step, or too big to compare: by position.
        return (0..new.len()).map(fresh).collect();
    }
    // The longest common subsequence of equal items.
    let (n, m) = (old.len(), new.len());
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let v = if old.get(i) == new.get(j) {
                dp.get(at(i + 1, j + 1)).copied().unwrap_or(0) + 1
            } else {
                dp.get(at(i + 1, j)).copied().unwrap_or(0).max(dp.get(at(i, j + 1)).copied().unwrap_or(0))
            };
            if let Some(slot) = dp.get_mut(at(i, j)) {
                *slot = v;
            }
        }
    }
    let mut out = vec![None; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old.get(i) == new.get(j) {
            if let Some(slot) = out.get_mut(j) {
                *slot = old_ids.get(i).copied();
            }
            i += 1;
            j += 1;
        } else if dp.get(at(i + 1, j)).copied().unwrap_or(0) >= dp.get(at(i, j + 1)).copied().unwrap_or(0) {
            i += 1;
        } else {
            j += 1;
        }
    }
    out.into_iter().map(|e| e.unwrap_or_else(ElemId::new)).collect()
}

/// A column of a table, for aligning columns: its width and its cells.
#[derive(PartialEq)]
pub(crate) struct Column<'a> {
    pub width: Option<f64>,
    pub cells: Vec<Option<&'a deckcraft_model::Cell>>,
}

pub(crate) fn columns(t: &deckcraft_model::Table) -> Vec<Column<'_>> {
    let n = t.cols.len().max(t.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0));
    (0..n).map(|c| Column { width: t.cols.get(c).copied(), cells: t.rows.iter().map(|r| r.cells.get(c)).collect() }).collect()
}

/// The table of a shape, if it is one.
pub(crate) fn table_of(s: &Shape) -> Option<&deckcraft_model::Table> {
    match &s.kind {
        ShapeKind::Table(t) => Some(t),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_keep_their_elements_through_inserts_and_deletes() {
        let ids: Vec<ElemId> = (0..4).map(|_| ElemId::new()).collect();
        let old = ["a", "b", "c", "d"];
        assert_eq!(align(&old, &ids, &["a", "b", "x", "d"]), ids, "the same count: in place");
        let got = align(&old, &ids, &["a", "new", "b", "c", "d"]);
        assert_eq!(got.len(), 5);
        assert_eq!((got[0], got[2], got[3], got[4]), (ids[0], ids[1], ids[2], ids[3]));
        assert!(!ids.contains(&got[1]));
        let got = align(&old, &ids, &["a", "c", "d"]);
        assert_eq!(got, vec![ids[0], ids[2], ids[3]]);
    }
}
