//! The model as the live document holds it: the sources the kit diffs a change through.
//!
//! - `slides` (tree): one root node per slide (sorted by element, so reordering slides moves nothing here), its
//!   shapes under it (groups are nodes). A slide's fields, a shape's fields, its text (a nested story) and its table
//!   (a nested table whose cells are maps with stories) are properties of the node.
//! - `order` (table, one column): the slides in order, a movable list.
//! - `deck` (tree): the presentation's settings node (each setting a property), then the masters, each with its
//!   layouts under it; a master or layout is one property (`json`): last writer wins for the whole item.
//! - `comments`, `sections`, `shows` (tables, one column): one row each, its JSON in the cell.
//! - `media` (table): one row per media item, its name and type (`meta`) and its chunk reference (`chunk`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use craft_cloud_kit::{ElemId, KitError, Props, TableSource, TreeSource, Value};
use deckcraft_model::{Shape, ShapeKind, Slide, Table};
use serde_json::Value as Json;

use super::ids::Ids;
use super::json::{live_text, object, put, put_fields};
use super::text::story;
use super::{CHUNK, LiveDeck, META, ONE, SETTINGS};

/// Presentation fields that aren't settings (they have parts of their own, or are this machine's).
pub(crate) const NOT_SETTINGS: &[&str] = &["slides", "masters", "media", "sections", "customShows", "nextId"];
/// Slide fields that aren't the slide node's properties.
const NOT_SLIDE: &[&str] = &["id", "shapes", "notes", "comments"];
/// In a body's properties: it has no paragraph.
pub(crate) const NO_PARAGRAPHS: &str = "noParagraphs";
/// Shape fields written apart.
const NOT_SHAPE: &[&str] = &["id", "text", "kind"];

/// The slides tree.
pub(crate) struct SlidesSrc<'a> {
    d: &'a LiveDeck,
    slides: HashMap<ElemId, &'a Arc<Slide>>,
    roots: Vec<ElemId>,
    shapes: RefCell<HashMap<ElemId, &'a Shape>>,
}

impl<'a> SlidesSrc<'a> {
    pub fn new(d: &'a LiveDeck) -> Self {
        let mut slides = HashMap::new();
        for s in &d.pres.slides {
            if let Some(e) = d.ids.elem(s.id.0) {
                slides.insert(e, s);
            }
        }
        let mut roots: Vec<ElemId> = slides.keys().copied().collect();
        roots.sort();
        Self { d, slides, roots, shapes: RefCell::new(HashMap::new()) }
    }

    fn elems(&self, shapes: &'a [Shape]) -> Vec<ElemId> {
        let mut out = Vec::with_capacity(shapes.len());
        let mut cache = self.shapes.borrow_mut();
        for s in shapes {
            if let Some(e) = self.d.ids.elem(s.id.0) {
                cache.insert(e, s);
                out.push(e);
            }
        }
        out
    }
}

impl TreeSource for SlidesSrc<'_> {
    fn roots(&self) -> Vec<ElemId> {
        self.roots.clone()
    }

    fn children(&self, id: ElemId) -> Vec<ElemId> {
        if let Some(slide) = self.slides.get(&id) {
            return self.elems(&slide.shapes);
        }
        let shape = self.shapes.borrow().get(&id).copied();
        match shape {
            Some(s) => self.elems(s.children()),
            None => Vec::new(),
        }
    }

    fn subtree_key(&self, id: ElemId) -> Option<usize> {
        self.slides.get(&id).map(|s| Arc::as_ptr(s) as usize)
    }

    fn write(&self, id: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        let ids = &self.d.ids;
        if let Some(slide) = self.slides.get(&id) {
            put_fields(props, object(&***slide), NOT_SLIDE, ids)?;
            props.text("notes", &story(&slide.notes, ids))?;
            put(props, "notesBody", body_text(&slide.notes, ids))?;
            return Ok(());
        }
        let shape = self.shapes.borrow().get(&id).copied();
        if let Some(s) = shape {
            write_shape(s, id, props, ids)?;
        }
        Ok(())
    }
}

/// A body's properties and list style (its paragraphs are the story).
fn body_text(t: &deckcraft_model::TextBody, ids: &Ids) -> String {
    let mut o = object(t);
    o.remove("paragraphs");
    if t.paragraphs.is_empty() {
        // A body with no paragraph at all (an empty story has one, empty).
        o.insert(NO_PARAGRAPHS.into(), Json::Bool(true));
    }
    live_text(&Json::Object(o), ids)
}

fn write_shape(s: &Shape, elem: ElemId, props: &mut Props<'_, '_>, ids: &Ids) -> Result<(), KitError> {
    put_fields(props, object(s), NOT_SHAPE, ids)?;
    let mut kind = object(&s.kind);
    match &s.kind {
        ShapeKind::Group { .. } => {
            kind.remove("children");
        }
        ShapeKind::Table(_) => {
            kind.remove("rows");
            kind.remove("cols");
        }
        _ => {}
    }
    put(props, "kind", live_text(&Json::Object(kind), ids))?;
    // The text is always there (empty when the shape has none), so typing that arrives in it is never lost.
    let empty = deckcraft_model::TextBody::default();
    props.text("text", &story(s.text.as_ref().unwrap_or(&empty), ids))?;
    if let Some(t) = &s.text {
        put(props, "body", body_text(t, ids))?;
    }
    if let ShapeKind::Table(t) = &s.kind {
        let rows = ids.rows.get(&elem).cloned().unwrap_or_default();
        let cols = ids.cols.get(&elem).cloned().unwrap_or_default();
        for (c, w) in cols.iter().zip(&t.cols) {
            props.set(&format!("w:{}", c.hex()), *w)?;
        }
        for (r, row) in rows.iter().zip(&t.rows) {
            props.set(&format!("h:{}", r.hex()), row.height)?;
        }
        props.table("table", &TableSrc { t, rows, cols, ids })?;
    }
    Ok(())
}

/// A table shape's rows, columns and cells.
struct TableSrc<'a> {
    t: &'a Table,
    rows: Vec<ElemId>,
    cols: Vec<ElemId>,
    ids: &'a Ids,
}

impl TableSource for TableSrc<'_> {
    fn rows(&self) -> Vec<ElemId> {
        self.rows.clone()
    }
    fn columns(&self) -> Vec<ElemId> {
        self.cols.clone()
    }
    fn cell(&self, _: ElemId, _: ElemId) -> Option<Value> {
        None
    }
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        Vec::new()
    }
    fn cell_maps(&self) -> bool {
        true
    }
    fn write_cell(&self, row: ElemId, column: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        let (Some(r), Some(c)) = (self.rows.iter().position(|x| *x == row), self.cols.iter().position(|x| *x == column)) else { return Ok(()) };
        let blank = deckcraft_model::Cell::new("");
        let cell = self.t.rows.get(r).and_then(|row| row.cells.get(c)).unwrap_or(&blank);
        put_fields(props, object(cell), &["text"], self.ids)?;
        props.text("text", &story(&cell.text, self.ids))?;
        put(props, "body", body_text(&cell.text, self.ids))
    }
}

/// The deck tree: the settings node, then the masters with their layouts.
pub(crate) struct DeckSrc<'a> {
    d: &'a LiveDeck,
    masters: HashMap<ElemId, &'a Arc<deckcraft_model::Master>>,
    layouts: HashMap<ElemId, &'a deckcraft_model::Layout>,
}

impl<'a> DeckSrc<'a> {
    pub fn new(d: &'a LiveDeck) -> Self {
        let mut masters = HashMap::new();
        let mut layouts = HashMap::new();
        for m in &d.pres.masters {
            if let Some(e) = d.ids.elem(m.id.0) {
                masters.insert(e, m);
            }
            for l in &m.layouts {
                if let Some(e) = d.ids.elem(l.id.0) {
                    layouts.insert(e, l);
                }
            }
        }
        Self { d, masters, layouts }
    }
}

impl TreeSource for DeckSrc<'_> {
    fn roots(&self) -> Vec<ElemId> {
        let mut out = vec![SETTINGS];
        out.extend(self.d.pres.masters.iter().filter_map(|m| self.d.ids.elem(m.id.0)));
        out
    }

    fn children(&self, id: ElemId) -> Vec<ElemId> {
        match self.masters.get(&id) {
            Some(m) => m.layouts.iter().filter_map(|l| self.d.ids.elem(l.id.0)).collect(),
            None => Vec::new(),
        }
    }

    fn subtree_key(&self, id: ElemId) -> Option<usize> {
        if id == SETTINGS {
            return Some(self.d.keys.settings as usize);
        }
        self.masters.get(&id).map(|m| Arc::as_ptr(m) as usize)
    }

    fn write(&self, id: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        let ids = &self.d.ids;
        if id == SETTINGS {
            let mut view = (*self.d.pres).clone();
            view.slides.clear();
            view.masters.clear();
            view.media.clear();
            return put_fields(props, object(&view), NOT_SETTINGS, ids);
        }
        if let Some(m) = self.masters.get(&id) {
            let mut o = object(&***m);
            o.remove("id");
            o.remove("layouts");
            return put(props, "json", live_text(&Json::Object(o), ids));
        }
        if let Some(l) = self.layouts.get(&id) {
            let mut o = object(*l);
            o.remove("id");
            return put(props, "json", live_text(&Json::Object(o), ids));
        }
        Ok(())
    }
}

/// A one-column table of records, each its JSON.
pub(crate) struct Records {
    pub key: u64,
    pub rows: Vec<ElemId>,
    pub cells: RefCell<Option<HashMap<ElemId, String>>>,
    pub make: Box<dyn Fn() -> HashMap<ElemId, String> + 'static>,
}

impl Records {
    fn with<R>(&self, f: impl FnOnce(&HashMap<ElemId, String>) -> R) -> R {
        let mut c = self.cells.borrow_mut();
        let map = c.get_or_insert_with(|| (self.make)());
        f(map)
    }
}

impl TableSource for Records {
    fn key(&self) -> Option<usize> {
        Some(self.key as usize)
    }
    fn rows(&self) -> Vec<ElemId> {
        self.rows.clone()
    }
    fn columns(&self) -> Vec<ElemId> {
        vec![ONE]
    }
    fn cell(&self, row: ElemId, column: ElemId) -> Option<Value> {
        if column != ONE {
            return None;
        }
        self.with(|m| m.get(&row).map(|s| Value::Str(s.clone())))
    }
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        let rows = self.rows.clone();
        self.with(|m| rows.iter().filter_map(|r| m.get(r).map(|s| (*r, ONE, Value::Str(s.clone())))).collect())
    }
}

/// The slides in order.
pub(crate) fn order(d: &LiveDeck) -> Records {
    let rows = d.pres.slides.iter().filter_map(|s| d.ids.elem(s.id.0)).collect();
    Records { key: d.keys.order, rows, cells: RefCell::new(Some(HashMap::new())), make: Box::new(HashMap::new) }
}

/// Every slide's comments, each with its slide.
pub(crate) fn comments(d: &LiveDeck) -> Records {
    let mut rows = Vec::new();
    for s in &d.pres.slides {
        let Some(se) = d.ids.elem(s.id.0) else { continue };
        let Some(list) = d.ids.comments.get(&se) else { continue };
        rows.extend(list.iter().take(s.comments.len()));
    }
    let deck = d.clone();
    Records {
        key: d.keys.comments,
        rows,
        cells: RefCell::new(None),
        make: Box::new(move || {
            let mut out = HashMap::new();
            for s in &deck.pres.slides {
                let Some(se) = deck.ids.elem(s.id.0) else { continue };
                let Some(list) = deck.ids.comments.get(&se) else { continue };
                for (e, c) in list.iter().zip(&s.comments) {
                    let mut o = object(c);
                    o.insert("slide".into(), Json::String(format!("@{}", se.hex())));
                    out.insert(*e, live_text(&Json::Object(o), &deck.ids));
                }
            }
            out
        }),
    }
}

/// Sections, or custom shows, mapped in full: one row each (a movable list, so two machines adding one at once keep
/// both), its name a property of its cell, and its slides a movable list of slide elements nested in the cell, so two
/// machines adding a slide to one section at once keep both.
pub(crate) struct SlideLists {
    key: u64,
    rows: Vec<ElemId>,
    items: Vec<(String, Vec<ElemId>)>,
}

pub(crate) fn lists<'a>(
    d: &LiveDeck,
    key: u64,
    items: impl Iterator<Item = (&'a str, &'a [deckcraft_model::SlideId])>,
    elems: &[ElemId],
) -> SlideLists {
    let items: Vec<(String, Vec<ElemId>)> =
        items.map(|(name, slides)| (name.to_owned(), slides.iter().filter_map(|s| d.ids.elem(s.0)).collect())).collect();
    let rows = elems.iter().take(items.len()).copied().collect();
    SlideLists { key, rows, items }
}

impl TableSource for SlideLists {
    fn key(&self) -> Option<usize> {
        Some(self.key as usize)
    }
    fn rows(&self) -> Vec<ElemId> {
        self.rows.clone()
    }
    fn columns(&self) -> Vec<ElemId> {
        vec![ONE]
    }
    fn cell(&self, _: ElemId, _: ElemId) -> Option<Value> {
        None
    }
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        Vec::new()
    }
    fn cell_maps(&self) -> bool {
        true
    }
    fn write_cell(&self, row: ElemId, _: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        let Some((name, slides)) = self.rows.iter().position(|r| *r == row).and_then(|i| self.items.get(i)) else { return Ok(()) };
        props.set("name", name.as_str())?;
        props.table("slides", &IdList(slides))
    }
}

/// A list of elements as a table of rows alone.
struct IdList<'a>(&'a [ElemId]);

impl TableSource for IdList<'_> {
    fn rows(&self) -> Vec<ElemId> {
        // Each once (a list naming one twice keeps its first place).
        let mut seen = std::collections::HashSet::new();
        self.0.iter().copied().filter(|e| seen.insert(*e)).collect()
    }
    fn columns(&self) -> Vec<ElemId> {
        Vec::new()
    }
    fn cell(&self, _: ElemId, _: ElemId) -> Option<Value> {
        None
    }
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        Vec::new()
    }
}

/// The media items: name and type, and the chunk reference of their bytes once they are in the blob store.
pub(crate) struct MediaSrc<'a> {
    pub d: &'a LiveDeck,
}

impl TableSource for MediaSrc<'_> {
    fn key(&self) -> Option<usize> {
        Some(self.d.keys.media as usize)
    }
    fn rows(&self) -> Vec<ElemId> {
        self.d.pres.media.iter().filter_map(|m| self.d.ids.elem(m.id.0)).collect()
    }
    fn columns(&self) -> Vec<ElemId> {
        vec![META, CHUNK]
    }
    fn cell(&self, row: ElemId, column: ElemId) -> Option<Value> {
        let local = self.d.ids.local(row)?;
        let m = self.d.pres.media.iter().find(|m| m.id.0 == local)?;
        if column == META {
            let meta = serde_json::json!({"name": m.name, "contentType": m.content_type, "link": m.link});
            Some(Value::Str(meta.to_string()))
        } else if column == CHUNK {
            self.d.chunks.get(&row).map(|c| Value::Chunk(*c))
        } else {
            None
        }
    }
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        let mut out = Vec::new();
        for r in self.rows() {
            for c in [META, CHUNK] {
                if let Some(v) = self.cell(r, c) {
                    out.push((r, c, v));
                }
            }
        }
        out
    }
}
