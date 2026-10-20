//! What a merge can break, repaired after every remote change (the kit writes the repair back): each repair is
//! decided from the model alone, ordered by element, so every machine makes the same one.
//!
//! - references to what a merge deleted: a connector's end, an animation's shape or trigger, a comment's shape, a
//!   section's or custom show's slide (dropped, as DeckCraft does on a local delete);
//! - two placeholders in one slot on a slide (two machines added the slide's title at once): the second by element is
//!   kept as an ordinary shape where its layout's placeholder was;
//! - a table cell a merge left out (a row and a column added at once), made when the table was read: its slide is
//!   written back;
//! - a slide whose place in the order hadn't come yet: the order is written back;
//! - `next_id` above every local id.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use craft_cloud_kit::ElemId;
use deckcraft_model::{PhType, Presentation, ShapeId, ShapeKind, Slide};

use super::ids::Ids;
use super::{LiveDeck, fresh_key};

pub(crate) fn run(d: &mut LiveDeck) {
    let repair = std::mem::take(&mut d.repair);
    let ids = d.ids.clone();
    let pres = Arc::make_mut(&mut d.pres);
    let slide_ids: HashSet<u32> = pres.slides.iter().map(|s| s.id.0).chain(ids.pending_ids()).collect();
    let mut comments_changed = false;
    // A group whose children all went (each machine deleted one at once) goes too, as DeckCraft does.
    for s in &mut pres.slides {
        if has_empty_group(&s.shapes, 0) {
            let mut fixed = (**s).clone();
            while drop_empty_groups(&mut fixed.shapes, 0) {}
            *s = Arc::new(fixed);
        }
    }
    for s in &mut pres.slides {
        if let Some(fixed) = fix_slide(s, &ids) {
            comments_changed |= fixed.comments != s.comments;
            *s = Arc::new(fixed);
        }
    }
    let placeholders: Vec<(usize, Slide)> =
        pres.slides.iter().enumerate().filter_map(|(i, s)| fix_placeholders(s, &ids, pres).map(|f| (i, f))).collect();
    for (i, fixed) in placeholders {
        if let Some(s) = pres.slides.get_mut(i) {
            *s = Arc::new(fixed);
        }
    }
    // Slides a merge left incomplete: a new allocation, so the kit writes them back.
    for e in &repair.slides {
        if let Some(local) = ids.local(*e)
            && let Some(s) = pres.slides.iter_mut().find(|s| s.id.0 == local)
        {
            *s = Arc::new((**s).clone());
        }
    }
    if repair.order {
        d.keys.order = fresh_key();
    }
    if comments_changed {
        d.keys.comments = fresh_key();
    }
    let gone = |list: &[deckcraft_model::SlideId]| list.iter().any(|id| !slide_ids.contains(&id.0));
    if pres.sections.iter().any(|s| gone(&s.slides)) {
        for s in &mut pres.sections {
            s.slides.retain(|id| slide_ids.contains(&id.0));
        }
        d.keys.sections = fresh_key();
    }
    if partition_sections(pres) {
        d.keys.sections = fresh_key();
    }
    if pres.custom_shows.iter().any(|s| gone(&s.slides)) {
        for s in &mut pres.custom_shows {
            s.slides.retain(|id| slide_ids.contains(&id.0));
        }
        d.keys.shows = fresh_key();
    }
    // A slide whose layout a merge deleted takes the first one (as `deckcraft_format::repair` does on opening; its
    // other repairs copy every slide, too slow after each remote keystroke).
    let first_layout = pres.masters.first().and_then(|m| m.layouts.first()).map(|l| l.id);
    let missing: Vec<usize> =
        pres.slides.iter().enumerate().filter(|(_, s)| pres.layout(s.layout).is_none() && !ids.is_pending(s.layout.0)).map(|(i, _)| i).collect();
    if let Some(first) = first_layout {
        for i in missing {
            if let Some(s) = pres.slides.get_mut(i) {
                Arc::make_mut(s).layout = first;
            }
        }
    }
    pres.slide_size = deckcraft_format::sane_slide_size(pres.slide_size);
    pres.fix_next_id();
}

fn has_empty_group(shapes: &[deckcraft_model::Shape], depth: usize) -> bool {
    depth <= 64 && shapes.iter().any(|s| (s.is_group() && s.children().is_empty()) || has_empty_group(s.children(), depth + 1))
}

/// Removes empty groups (one level of them); true if it removed any.
fn drop_empty_groups(shapes: &mut Vec<deckcraft_model::Shape>, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    let before = shapes.len();
    shapes.retain(|s| !(s.is_group() && s.children().is_empty()));
    let mut changed = shapes.len() != before;
    for s in shapes.iter_mut() {
        if let Some(children) = s.children_mut() {
            changed |= drop_empty_groups(children, depth + 1);
        }
    }
    changed
}

/// Sections split the slides as DeckCraft keeps them: every slide in exactly one section (the first that lists it),
/// each section's slides in slide order, a slide no section has (two machines added slides at once) in the section of
/// the slide before it. True if it changed anything.
fn partition_sections(pres: &mut Presentation) -> bool {
    if pres.sections.is_empty() {
        return false;
    }
    let place: std::collections::HashMap<u32, usize> = pres.slides.iter().enumerate().map(|(i, s)| (s.id.0, i)).collect();
    let mut owner: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    let mut fixed: Vec<Vec<deckcraft_model::SlideId>> = Vec::with_capacity(pres.sections.len());
    for (k, sec) in pres.sections.iter().enumerate() {
        let mut list: Vec<deckcraft_model::SlideId> =
            sec.slides.iter().copied().filter(|id| !place.contains_key(&id.0) || owner.insert(id.0, k).is_none()).collect();
        // Slides still on their way (no place yet) keep theirs at the end.
        list.sort_by_key(|id| place.get(&id.0).copied().unwrap_or(usize::MAX));
        fixed.push(list);
    }
    let mut prev_section = 0usize;
    for s in &pres.slides {
        match owner.get(&s.id.0) {
            Some(k) => prev_section = *k,
            None => {
                if let Some(list) = fixed.get_mut(prev_section) {
                    list.push(s.id);
                    list.sort_by_key(|id| place.get(&id.0).copied().unwrap_or(usize::MAX));
                }
                owner.insert(s.id.0, prev_section);
            }
        }
    }
    let changed = pres.sections.iter().zip(&fixed).any(|(sec, list)| sec.slides != *list);
    if changed {
        for (sec, list) in pres.sections.iter_mut().zip(fixed) {
            sec.slides = list;
        }
    }
    changed
}

/// A slide with its dangling references dropped, if it had any.
fn fix_slide(s: &Slide, ids: &Ids) -> Option<Slide> {
    let mut here: HashSet<ShapeId> = HashSet::new();
    deckcraft_model::walk(&s.shapes, &mut |sh, _| {
        here.insert(sh.id);
    });
    // Only what a merge deleted is dangling: an element not seen yet (its change still on its way) isn't.
    here.extend(ids.pending_ids().map(ShapeId));
    let mut dangling = false;
    deckcraft_model::walk(&s.shapes, &mut |sh, _| {
        if let ShapeKind::Connector { start, end } = &sh.kind {
            dangling |= start.is_some_and(|(id, _)| !here.contains(&id)) || end.is_some_and(|(id, _)| !here.contains(&id));
        }
    });
    let anim = s.animations.iter().any(|a| !here.contains(&a.shape) || a.trigger.is_some_and(|t| !here.contains(&t)));
    fn bad_comment(c: &deckcraft_model::Comment, here: &HashSet<ShapeId>) -> bool {
        c.shape.is_some_and(|id| !here.contains(&id)) || c.replies.iter().any(|r| bad_comment(r, here))
    }
    let comments = s.comments.iter().any(|c| bad_comment(c, &here));
    if !dangling && !anim && !comments {
        return None;
    }
    let mut s = s.clone();
    fn ends(shapes: &mut [deckcraft_model::Shape], here: &HashSet<ShapeId>, depth: usize) {
        if depth > 64 {
            return;
        }
        for sh in shapes {
            match &mut sh.kind {
                ShapeKind::Connector { start, end } => {
                    if start.is_some_and(|(id, _)| !here.contains(&id)) {
                        *start = None;
                    }
                    if end.is_some_and(|(id, _)| !here.contains(&id)) {
                        *end = None;
                    }
                }
                ShapeKind::Group { children, .. } => ends(children, here, depth + 1),
                _ => {}
            }
        }
    }
    ends(&mut s.shapes, &here, 0);
    s.animations.retain(|a| here.contains(&a.shape));
    for a in &mut s.animations {
        if a.trigger.is_some_and(|t| !here.contains(&t)) {
            a.trigger = None;
        }
    }
    fn fix_comment(c: &mut deckcraft_model::Comment, here: &HashSet<ShapeId>) {
        if c.shape.is_some_and(|id| !here.contains(&id)) {
            c.shape = None;
        }
        for r in &mut c.replies {
            fix_comment(r, here);
        }
    }
    for c in &mut s.comments {
        fix_comment(c, &here);
    }
    Some(s)
}

/// The slot a placeholder fills on its slide: a title is one slot whatever its index.
fn slot(ph: &deckcraft_model::Placeholder) -> (u8, u32) {
    let kind = match ph.kind {
        PhType::Title | PhType::CtrTitle => 0,
        k => 1 + k as u8,
    };
    (kind, if kind == 0 { 0 } else { ph.idx })
}

/// A slide with its duplicate placeholders made ordinary shapes, if it had any.
fn fix_placeholders(s: &Slide, ids: &Ids, pres: &Presentation) -> Option<Slide> {
    let mut by_slot: HashMap<(u8, u32), Vec<(ElemId, ShapeId)>> = HashMap::new();
    for sh in &s.shapes {
        if let (Some(ph), Some(e)) = (&sh.ph, ids.elem(sh.id.0)) {
            by_slot.entry(slot(ph)).or_default().push((e, sh.id));
        }
    }
    let mut extra: HashSet<ShapeId> = HashSet::new();
    for list in by_slot.values_mut() {
        if list.len() > 1 {
            list.sort();
            extra.extend(list.iter().skip(1).map(|(_, id)| *id));
        }
    }
    if extra.is_empty() {
        return None;
    }
    let layout = pres.layout(s.layout).map(|(_, l)| l);
    let mut s = s.clone();
    for sh in &mut s.shapes {
        if !extra.contains(&sh.id) {
            continue;
        }
        if sh.xfrm.is_none()
            && let (Some(ph), Some(l)) = (&sh.ph, layout)
        {
            sh.xfrm = l.shapes.iter().find(|x| x.ph.as_ref().is_some_and(|p| slot(p) == slot(ph))).and_then(|x| x.xfrm);
        }
        sh.ph = None;
    }
    Some(s)
}
