//! Live editing through Craft Cloud (the `live` feature): a `.deckcraft` file in the Craft Cloud folder, open on
//! several machines at once, every edit shown on the others as it is made.
//!
//! [`Deck`] is DeckCraft's adapter to the Craft Cloud kit: [`LiveDeck`] is the model the kit holds (the
//! presentation, this session's element ids, and what the mappings need), `write` lays it out in the live document,
//! `read` rebuilds what a remote change touched, `normalise` repairs what a merge can break. `session` ties it to
//! the engine's [`crate::Session`]: the hook after each command, undo, open, autosave and the banners.

pub mod ids;
mod json;
pub mod media;
mod normalise;
mod read;
pub mod session;
mod text;
mod write;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use craft_cloud_kit;
use craft_cloud_kit::{ChunkRef, ElemId, KitError, LiveDocument, Mappings, RemotePatch};
use deckcraft_model::{Presentation, ShapeKind};

pub use ids::Ids;
pub use media::Shared;

/// The settings node of the `deck` tree.
pub(crate) const SETTINGS: ElemId = ElemId(0x5e77_1265_0000_0000_0000_0000_0000_0001);
/// The one column of a table of records.
pub(crate) const ONE: ElemId = ElemId(1);
/// The media table's columns.
pub(crate) const META: ElemId = ElemId(2);
pub(crate) const CHUNK: ElemId = ElemId(3);

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// A key no other part of any model has: what tells the kit a part changed (a part with the same key in the old and
/// new model is skipped without being compared).
pub(crate) fn fresh_key() -> u64 {
    NEXT_KEY.fetch_add(1, Ordering::Relaxed)
}

/// The parts' change keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    pub settings: u64,
    pub order: u64,
    pub comments: u64,
    pub sections: u64,
    pub shows: u64,
    pub media: u64,
}

impl Keys {
    fn fresh() -> Self {
        Self { settings: fresh_key(), order: fresh_key(), comments: fresh_key(), sections: fresh_key(), shows: fresh_key(), media: fresh_key() }
    }
}

/// What a remote change left for `normalise` to write back: slides a merge left incomplete (a table cell nobody
/// made), and the slide order (a slide that came before its place in the order).
#[derive(Debug, Clone, Default)]
pub(crate) struct Repair {
    pub slides: Vec<ElemId>,
    pub order: bool,
}

/// The model the kit holds: a presentation and what this session knows about it.
#[derive(Clone)]
pub struct LiveDeck {
    pub pres: Arc<Presentation>,
    pub(crate) ids: Arc<Ids>,
    /// The chunk reference of each media item whose bytes are in the blob store.
    pub(crate) chunks: Arc<HashMap<ElemId, ChunkRef>>,
    pub(crate) keys: Keys,
    pub(crate) repair: Repair,
    pub(crate) shared: Arc<Shared>,
}

impl std::fmt::Debug for LiveDeck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveDeck").field("slides", &self.pres.slides.len()).finish_non_exhaustive()
    }
}

impl LiveDeck {
    /// A model for `pres` (a file just read), every element given an id.
    pub fn new(pres: Arc<Presentation>, shared: Arc<Shared>) -> Self {
        let mut ids = Ids::default();
        ids.cover_all(&pres);
        for s in &pres.slides {
            let Some(se) = ids.elem(s.id.0) else { continue };
            ids.comments.insert(se, s.comments.iter().map(|_| ElemId::new()).collect());
            cover_tables(&mut ids, &s.shapes, None, None);
        }
        ids.sections = pres.sections.iter().map(|_| ElemId::new()).collect();
        ids.shows = pres.custom_shows.iter().map(|_| ElemId::new()).collect();
        let chunks = Arc::new(shared.known_chunks(&ids, &pres));
        Self { pres, ids: Arc::new(ids), chunks, keys: Keys::fresh(), repair: Repair::default(), shared }
    }

    pub fn ids(&self) -> &Ids {
        &self.ids
    }

    /// The model after a local change made `pres` from this one's: elements the change added get ids, rows,
    /// columns, comments, sections and custom shows keep theirs by position, and the parts that changed get new keys.
    pub fn wrap(&self, pres: Arc<Presentation>) -> Self {
        self.wrap_with(pres, None)
    }

    /// [`LiveDeck::wrap`], told what a table command did (`edit`), so a table's rows and columns keep their elements
    /// exactly where the content alone can't tell (equal rows).
    pub fn wrap_with(&self, pres: Arc<Presentation>, edit: Option<ids::TableEdit>) -> Self {
        let old = &self.pres;
        let mut ids = (*self.ids).clone();
        let mut keys = self.keys;
        let old_slides: HashMap<u32, &Arc<deckcraft_model::Slide>> = old.slides.iter().map(|s| (s.id.0, s)).collect();
        let mut comments_changed = old.slides.len() != pres.slides.len();
        for s in &pres.slides {
            let before = old_slides.get(&s.id.0);
            if before.is_some_and(|b| Arc::ptr_eq(b, s)) {
                continue;
            }
            let se = ids.elem_or_new(s.id.0);
            ids.cover_shapes(&s.shapes);
            let old_list = before.map(|b| b.comments.as_slice()).unwrap_or(&[]);
            if old_list != s.comments.as_slice() || before.is_none() {
                comments_changed = true;
            }
            let old_ids = ids.comments.get(&se).cloned().unwrap_or_default();
            let new_ids = ids::align(old_list, &old_ids, &s.comments);
            ids.comments.insert(se, new_ids);
            cover_tables(&mut ids, &s.shapes, before.map(|b| b.shapes.as_slice()), edit);
        }
        if old.slides.iter().map(|s| s.id).ne(pres.slides.iter().map(|s| s.id)) {
            keys.order = fresh_key();
        }
        if comments_changed {
            keys.comments = fresh_key();
        }
        let old_masters: HashMap<u32, &Arc<deckcraft_model::Master>> = old.masters.iter().map(|m| (m.id.0, m)).collect();
        for m in &pres.masters {
            if old_masters.get(&m.id.0).is_some_and(|b| Arc::ptr_eq(b, m)) {
                continue;
            }
            ids.elem_or_new(m.id.0);
            ids.cover_shapes(&m.shapes);
            for l in &m.layouts {
                ids.elem_or_new(l.id.0);
                ids.cover_shapes(&l.shapes);
            }
        }
        for m in pres.notes_master.iter().chain(pres.handout_master.iter()) {
            ids.elem_or_new(m.id.0);
            ids.cover_shapes(&m.shapes);
        }
        let media_same = old.media.len() == pres.media.len()
            && old
                .media
                .iter()
                .zip(&pres.media)
                .all(|(a, b)| a.id == b.id && a.name == b.name && a.content_type == b.content_type && a.link == b.link);
        if !media_same {
            for m in &pres.media {
                ids.elem_or_new(m.id.0);
            }
            keys.media = fresh_key();
        }
        if old.sections != pres.sections {
            ids.sections = ids::align(&old.sections, &ids.sections, &pres.sections);
            keys.sections = fresh_key();
        }
        if old.custom_shows != pres.custom_shows {
            ids.shows = ids::align(&old.custom_shows, &ids.shows, &pres.custom_shows);
            keys.shows = fresh_key();
        }
        if settings_differ(old, &pres) {
            keys.settings = fresh_key();
        }
        Self { pres, ids: Arc::new(ids), chunks: self.chunks.clone(), keys, repair: Repair::default(), shared: self.shared.clone() }
    }

    /// The same model with `chunks` known (an upload finished), for a background change.
    pub fn with_chunks(&self, chunks: HashMap<ElemId, ChunkRef>) -> Self {
        let mut keys = self.keys;
        keys.media = fresh_key();
        Self { chunks: Arc::new(chunks), keys, ..self.clone() }
    }

    /// The same model over `pres` (the same content with bytes read back into its media), for a background change.
    pub fn with_pres(&self, pres: Arc<Presentation>) -> Self {
        Self { pres, ..self.clone() }
    }
}

fn settings_differ(a: &Presentation, b: &Presentation) -> bool {
    let ptr = |x: &Option<Arc<deckcraft_model::Master>>, y: &Option<Arc<deckcraft_model::Master>>| match (x, y) {
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        (None, None) => true,
        _ => false,
    };
    // Every field named, so a new one doesn't compile until it's sorted: a setting here (and in `read::deck`), or
    // written elsewhere.
    let Presentation {
        slide_size,
        notes_size,
        slides: _,
        masters: _,
        notes_master,
        handout_master,
        sections: _,
        custom_shows: _,
        show,
        header_footer,
        props,
        default_text_style,
        media: _,
        first_slide_number,
        next_id: _,
        embedded_fonts,
        table_styles,
        tags,
        kept_parts,
        guides,
        raw_ext,
    } = a;
    *slide_size != b.slide_size
        || *notes_size != b.notes_size
        || !ptr(notes_master, &b.notes_master)
        || !ptr(handout_master, &b.handout_master)
        || *show != b.show
        || *header_footer != b.header_footer
        || *props != b.props
        || *default_text_style != b.default_text_style
        || *first_slide_number != b.first_slide_number
        || *embedded_fonts != b.embedded_fonts
        || *table_styles != b.table_styles
        || *tags != b.tags
        || *kept_parts != b.kept_parts
        || *guides != b.guides
        || *raw_ext != b.raw_ext
}

/// Rows and columns for the table shapes of `shapes`, kept from `old` (the same shapes before the change) by
/// position where they were there.
fn cover_tables(ids: &mut Ids, shapes: &[deckcraft_model::Shape], old: Option<&[deckcraft_model::Shape]>, edit: Option<ids::TableEdit>) {
    let mut found = Vec::new();
    deckcraft_model::walk(shapes, &mut |s, _| {
        if let ShapeKind::Table(t) = &s.kind {
            found.push((s.id, t));
        }
    });
    for (id, t) in found {
        let Some(e) = ids.elem(id.0) else { continue };
        let before = old.and_then(|o| deckcraft_model::find_shape(o, id)).and_then(ids::table_of);
        let (old_rows, old_cols) = (ids.rows.get(&e).cloned().unwrap_or_default(), ids.cols.get(&e).cloned().unwrap_or_default());
        let (rows, cols) = match before {
            Some(b) if b == t && old_rows.len() == t.rows.len() && old_cols.len() == ids::columns(t).len() => continue,
            // The command said what it did (a row inserted or deleted at an index): taken as it is.
            Some(_) if let Some(done) = edit.and_then(|e| e.apply(&old_rows, &old_cols, t.rows.len(), ids::columns(t).len())) => done,
            Some(b) => (ids::align(&b.rows, &old_rows, &t.rows), ids::align(&ids::columns(b), &old_cols, &ids::columns(t))),
            None if old_rows.len() == t.rows.len() && old_cols.len() == ids::columns(t).len() => (old_rows, old_cols),
            None => (t.rows.iter().map(|_| ElemId::new()).collect(), ids::columns(t).iter().map(|_| ElemId::new()).collect()),
        };
        ids.rows.insert(e, rows);
        ids.cols.insert(e, cols);
    }
}

/// DeckCraft's live documents.
pub struct Deck;

impl LiveDocument for Deck {
    type Doc = LiveDeck;
    const APP: &'static str = "deckcraft";
    const SCHEMA: u32 = 1;

    fn mappings(d: &LiveDeck) -> Mappings<'_> {
        Mappings::new()
            .tree("slides", write::SlidesSrc::new(d))
            .table("order", write::order(d))
            .tree("deck", write::DeckSrc::new(d))
            .table("comments", write::comments(d))
            .table(
                "sections",
                write::lists(d, d.keys.sections, d.pres.sections.iter().map(|s| (s.name.as_str(), s.slides.as_slice())), &d.ids.sections),
            )
            .table("shows", write::lists(d, d.keys.shows, d.pres.custom_shows.iter().map(|s| (s.name.as_str(), s.slides.as_slice())), &d.ids.shows))
            .table("media", write::MediaSrc { d })
    }

    fn apply_remote(old: &Arc<LiveDeck>, patch: &RemotePatch<'_>) -> Result<Arc<LiveDeck>, KitError> {
        read::apply(old, patch).map(Arc::new)
    }

    fn normalise(d: &mut LiveDeck) {
        normalise::run(d);
    }

    fn refresh_derived(_d: &mut LiveDeck) {}
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_engine;
#[cfg(test)]
mod tests_sim;
#[cfg(test)]
mod tests_speed;
