//! A remote change into the model: only the parts it touched are read back. A slide is rebuilt when anything in it
//! changed (its untouched shapes are kept as they were); slides nothing touched stay the same allocation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use craft_cloud_kit::{ElemId, KitError, PropsRead, RemotePatch, TableView, TreeView};
use deckcraft_model::table::{Cell, Row};
use deckcraft_model::text::TextBody;
use deckcraft_model::{Comment, CustomShow, Layout, Master, MediaItem, Presentation, Section, Shape, ShapeKind, Slide};
use serde_json::{Map, Value as Json};

use super::ids::Ids;
use super::json::{Read, to_local};
use super::text::paragraphs;
use super::write::{NO_PARAGRAPHS, NOT_SETTINGS};
use super::{CHUNK, LiveDeck, META, ONE, Repair, SETTINGS, fresh_key};

fn elem_json(e: ElemId) -> Json {
    Json::String(format!("@{}", e.hex()))
}

/// `obj` (element references) as a `T` with local ids.
fn decode<T: serde::de::DeserializeOwned>(obj: Map<String, Json>, ids: &mut Ids, p: &mut Presentation) -> Option<T> {
    let mut j = Json::Object(obj);
    to_local(&mut j, ids, p);
    serde_json::from_value(j).ok()
}

/// A text body: its properties and list style (`body`) and its paragraphs (the story `story` of `props`).
fn body(r: &Read, props: &PropsRead, key: &str, story: &str, ids: &mut Ids, p: &mut Presentation) -> Option<TextBody> {
    let Json::Object(o) = r.json(key)? else { return None };
    let none = o.get(NO_PARAGRAPHS).and_then(Json::as_bool).unwrap_or(false);
    let mut t: TextBody = decode(o, ids, p).unwrap_or_default();
    let rt = props.text(story).unwrap_or_default();
    t.paragraphs = if none && rt.text.is_empty() { Vec::new() } else { paragraphs(&rt, ids, p) };
    Some(t)
}

pub(crate) fn apply(old: &Arc<LiveDeck>, patch: &RemotePatch<'_>) -> Result<LiveDeck, KitError> {
    let mut ids = (*old.ids).clone();
    let mut pres = (*old.pres).clone();
    pres.fix_next_id();
    let mut keys = old.keys;
    let mut repair = Repair::default();
    let mut chunks = old.chunks.clone();
    if let Some(view) = patch.table("media") {
        chunks = Arc::new(media(&view, &mut pres, &mut ids, old));
        keys.media = fresh_key();
    }
    if let Some(view) = patch.tree("deck")
        && deck(&view, &mut pres, &mut ids, old)
    {
        keys.settings = fresh_key();
    }
    let tree = patch.tree("slides");
    let order = patch.table("order");
    let rebuilt = if tree.is_some() || order.is_some() {
        keys.order = fresh_key();
        slides(tree.as_ref(), order.as_ref(), &mut pres, &mut ids, old, &mut repair)
    } else {
        false
    };
    if let Some(view) = patch.table("comments") {
        comments(&view, &mut pres, &mut ids);
        keys.comments = fresh_key();
    } else if rebuilt {
        keys.comments = fresh_key();
    }
    if let Some(view) = patch.table("sections") {
        let (list, elems) = records::<Section>(&view, &mut ids, &mut pres);
        pres.sections = list;
        ids.sections = elems;
        keys.sections = fresh_key();
    }
    if let Some(view) = patch.table("shows") {
        let (list, elems) = records::<CustomShow>(&view, &mut ids, &mut pres);
        pres.custom_shows = list;
        ids.shows = elems;
        keys.shows = fresh_key();
    }
    pres.fix_next_id();
    Ok(LiveDeck { pres: Arc::new(pres), ids: Arc::new(ids), chunks, keys, repair, shared: old.shared.clone() })
}

fn media(view: &TableView<'_>, pres: &mut Presentation, ids: &mut Ids, old: &LiveDeck) -> HashMap<ElemId, craft_cloud_kit::ChunkRef> {
    let before: HashMap<u32, &MediaItem> = old.pres.media.iter().map(|m| (m.id.0, m)).collect();
    let mut chunks = HashMap::new();
    let mut list = Vec::new();
    for row in view.rows() {
        let meta: Json = view.cell(row, META).and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok())).unwrap_or(Json::Null);
        let chunk = view.cell(row, CHUNK).and_then(|v| v.as_chunk());
        let Some(id) = ids.local_or_new(row, pres) else { continue };
        let was = before.get(&id);
        let data = match (was, chunk) {
            // Ours, or the same bytes: as they are.
            (Some(m), c) if !m.data.is_empty() && (c.is_none() || c == old.chunks.get(&row).copied()) => m.data.clone(),
            (_, Some(c)) => match old.shared.bytes(&c) {
                Some(b) => b,
                None => {
                    old.shared.fetch(c);
                    old.shared.bytes(&c).unwrap_or_default()
                }
            },
            (Some(m), None) => m.data.clone(),
            (None, None) => Arc::default(),
        };
        if let Some(c) = chunk {
            chunks.insert(row, c);
        }
        let text = |k: &str| meta.get(k).and_then(Json::as_str).map(str::to_owned);
        list.push(MediaItem {
            id: deckcraft_model::MediaId(id),
            name: text("name").unwrap_or_default(),
            content_type: text("contentType").unwrap_or_default(),
            data,
            link: text("link"),
        });
    }
    pres.media = list;
    chunks
}

/// Masters, layouts and settings. True if the settings changed.
fn deck(view: &TreeView<'_>, pres: &mut Presentation, ids: &mut Ids, old: &LiveDeck) -> bool {
    let touch = view.touch().clone();
    let touched = |e: &ElemId| touch.all || touch.nodes.contains(e);
    let settings = touched(&SETTINGS);
    if settings && let Some(props) = view.props(SETTINGS) {
        let r = Read::new(&props);
        let mut obj = Map::new();
        for (k, v) in r.fields(&|k| NOT_SETTINGS.contains(&k)) {
            obj.insert(k, v);
        }
        if let Some(s) = decode::<Presentation>(obj, ids, pres) {
            pres.slide_size = s.slide_size;
            pres.notes_size = s.notes_size;
            pres.notes_master = s.notes_master;
            pres.handout_master = s.handout_master;
            pres.show = s.show;
            pres.header_footer = s.header_footer;
            pres.props = s.props;
            pres.default_text_style = s.default_text_style;
            pres.first_slide_number = s.first_slide_number;
            pres.embedded_fonts = s.embedded_fonts;
            pres.table_styles = s.table_styles;
            pres.tags = s.tags;
            pres.kept_parts = s.kept_parts;
            pres.guides = s.guides;
            pres.raw_ext = s.raw_ext;
        }
    }
    let before: HashMap<u32, &Arc<Master>> = old.pres.masters.iter().map(|m| (m.id.0, m)).collect();
    let mut masters = Vec::new();
    for e in view.roots().into_iter().filter(|e| *e != SETTINGS) {
        let kids = view.children(e);
        let again = touch.structure || touched(&e) || kids.iter().any(touched);
        let was = ids.local(e).and_then(|l| before.get(&l).copied());
        if !again && let Some(m) = was {
            masters.push(m.clone());
            continue;
        }
        let Some(mut obj) = view.props(e).and_then(|p| Read::new(&p).json("json")).and_then(|j| j.as_object().cloned()) else {
            if let Some(m) = was {
                masters.push(m.clone());
            }
            continue;
        };
        obj.insert("id".into(), elem_json(e));
        let Some(mut m) = decode::<Master>(obj, ids, pres) else { continue };
        m.layouts = kids
            .into_iter()
            .filter_map(|l| {
                let mut o = view.props(l).and_then(|p| Read::new(&p).json("json")).and_then(|j| j.as_object().cloned())?;
                o.insert("id".into(), elem_json(l));
                decode::<Layout>(o, ids, pres)
            })
            .collect();
        masters.push(Arc::new(m));
    }
    if !masters.is_empty() {
        pres.masters = masters;
    }
    settings
}

/// What the slides need while being read.
struct Ctx<'a, 'v> {
    tree: &'a TreeView<'v>,
    all: bool,
    nodes: &'a std::collections::BTreeSet<ElemId>,
    ids: &'a mut Ids,
    pres: &'a mut Presentation,
    repair: &'a mut Repair,
}

impl Ctx<'_, '_> {
    fn touched(&self, e: ElemId) -> bool {
        self.all || self.nodes.contains(&e)
    }

    fn slide(&mut self, e: ElemId, before: Option<&Slide>) -> Option<Slide> {
        let props = self.tree.props(e)?;
        // Local ids for everything on the slide first: its fields and shapes refer to each other.
        let mut stack = vec![(e, 0usize)];
        while let Some((n, depth)) = stack.pop() {
            self.ids.local_or_new(n, self.pres);
            if depth < 64 {
                stack.extend(self.tree.children(n).into_iter().map(|c| (c, depth + 1)));
            }
        }
        let r = Read::new(&props);
        let mut obj = r.fields(&|k| k == "notesBody");
        obj.insert("id".into(), elem_json(e));
        let mut slide: Slide = match decode(obj, self.ids, self.pres) {
            Some(s) => s,
            None => before.cloned()?,
        };
        slide.notes = body(&r, &props, "notesBody", "notes", self.ids, self.pres).unwrap_or_default();
        let old: HashMap<u32, &Shape> = {
            let mut m = HashMap::new();
            if let Some(b) = before {
                deckcraft_model::walk(&b.shapes, &mut |s, _| {
                    m.insert(s.id.0, s);
                });
            }
            m
        };
        slide.shapes = self.tree.children(e).into_iter().filter_map(|c| self.shape(c, e, &old, 0)).collect();
        slide.comments = before.map(|b| b.comments.clone()).unwrap_or_default();
        Some(slide)
    }

    fn shape(&mut self, e: ElemId, slide: ElemId, old: &HashMap<u32, &Shape>, depth: usize) -> Option<Shape> {
        if depth > 64 {
            return None;
        }
        let was = self.ids.local(e).and_then(|l| old.get(&l).copied());
        if !self.touched(e)
            && let Some(s) = was
            && !s.is_group()
        {
            return Some(s.clone());
        }
        let props = self.tree.props(e)?;
        let r = Read::new(&props);
        let mut obj = r.fields(&|k| k == "kind" || k == "body" || k.starts_with("w:") || k.starts_with("h:"));
        obj.insert("id".into(), elem_json(e));
        let mut kind = r.json("kind").unwrap_or(Json::Null);
        if let Json::Object(k) = &mut kind {
            match k.get("type").and_then(Json::as_str) {
                Some("group") => {
                    k.insert("children".into(), Json::Array(Vec::new()));
                }
                Some("table") => {
                    k.insert("rows".into(), Json::Array(Vec::new()));
                    k.insert("cols".into(), Json::Array(Vec::new()));
                }
                _ => {}
            }
            obj.insert("kind".into(), kind);
        }
        let mut s: Shape = match decode(obj, self.ids, self.pres) {
            Some(s) => s,
            None => was.cloned()?,
        };
        s.text = body(&r, &props, "body", "text", self.ids, self.pres);
        match &mut s.kind {
            ShapeKind::Group { children, .. } => {
                *children = self.tree.children(e).into_iter().filter_map(|c| self.shape(c, slide, old, depth + 1)).collect();
            }
            ShapeKind::Table(t) => {
                if let Some(view) = props.table("table") {
                    let (rows, cols) = (view.rows(), view.columns());
                    t.cols = cols.iter().map(|c| r.map.get(&format!("w:{}", c.hex())).and_then(|v| v.as_f64()).unwrap_or(72.0)).collect();
                    let mut missing = false;
                    t.rows = rows
                        .iter()
                        .map(|row| Row {
                            height: r.map.get(&format!("h:{}", row.hex())).and_then(|v| v.as_f64()).unwrap_or(0.0),
                            cells: cols
                                .iter()
                                .map(|c| match view.cell_props(*row, *c) {
                                    Some(cp) => self.cell(&cp),
                                    None => {
                                        missing = true;
                                        Cell::new("")
                                    }
                                })
                                .collect(),
                        })
                        .collect();
                    if missing {
                        self.repair.slides.push(slide);
                    }
                    self.ids.rows.insert(e, rows);
                    self.ids.cols.insert(e, cols);
                }
            }
            _ => {}
        }
        Some(s)
    }

    fn cell(&mut self, props: &PropsRead) -> Cell {
        let r = Read::new(props);
        let obj = r.fields(&|k| k == "body");
        let mut c: Cell = decode(obj, self.ids, self.pres).unwrap_or_else(|| Cell::new(""));
        c.text = body(&r, props, "body", "text", self.ids, self.pres).unwrap_or_default();
        c
    }
}

/// The slides, in order. True if any was rebuilt.
fn slides(
    tree: Option<&TreeView<'_>>,
    order: Option<&TableView<'_>>,
    pres: &mut Presentation,
    ids: &mut Ids,
    old: &LiveDeck,
    repair: &mut Repair,
) -> bool {
    let before: HashMap<ElemId, &Arc<Slide>> = old.pres.slides.iter().filter_map(|s| Some((old.ids.elem(s.id.0)?, s))).collect();
    let order_elems: Vec<ElemId> = match order {
        Some(v) => v.rows(),
        None => old.pres.slides.iter().filter_map(|s| old.ids.elem(s.id.0)).collect(),
    };
    let mut rebuild: HashSet<ElemId> = HashSet::new();
    let roots: HashSet<ElemId> = match tree {
        Some(t) => t.roots().into_iter().collect(),
        None => before.keys().copied().collect(),
    };
    if let Some(t) = tree {
        let touch = t.touch();
        if touch.all {
            rebuild = roots.clone();
        } else {
            for e in &touch.nodes {
                // The slide it is in now…
                let mut at = *e;
                let mut hops = 0;
                while let Some(p) = t.parent(at) {
                    at = p;
                    hops += 1;
                    if hops > 128 {
                        break;
                    }
                }
                if roots.contains(&at) && t.contains(at) {
                    rebuild.insert(at);
                }
            }
            if touch.structure {
                // …and the one it was in (a shape deleted or moved away).
                let mut owner: HashMap<u32, ElemId> = HashMap::new();
                for (se, s) in &before {
                    deckcraft_model::walk(&s.shapes, &mut |sh, _| {
                        owner.insert(sh.id.0, *se);
                    });
                }
                for e in &touch.nodes {
                    if let Some(se) = old.ids.local(*e).and_then(|l| owner.get(&l)) {
                        rebuild.insert(*se);
                    }
                }
            }
        }
    }
    let mut out: Vec<Arc<Slide>> = Vec::with_capacity(order_elems.len());
    let mut placed = HashSet::new();
    let mut rebuilt = false;
    let nodes = tree.map(|t| t.touch().nodes.clone()).unwrap_or_default();
    let all = tree.is_some_and(|t| t.touch().all);
    let mut take = |e: ElemId, out: &mut Vec<Arc<Slide>>, ids: &mut Ids, pres: &mut Presentation, repair: &mut Repair| -> bool {
        let was = before.get(&e).copied();
        if !rebuild.contains(&e)
            && let Some(s) = was
        {
            out.push(s.clone());
            return true;
        }
        let Some(t) = tree else { return false };
        let mut cx = Ctx { tree: t, all, nodes: &nodes, ids, pres, repair };
        match cx.slide(e, was.map(|s| s.as_ref())) {
            Some(s) => {
                rebuilt = true;
                out.push(Arc::new(s));
                true
            }
            None => false,
        }
    };
    for e in order_elems {
        if !roots.contains(&e) || !placed.insert(e) {
            // A row whose slide isn't (or isn't yet) in the tree, or a slide twice: the order is written again.
            repair.order = true;
            continue;
        }
        if !take(e, &mut out, ids, pres, repair) {
            repair.order = true;
        }
    }
    // A slide in the tree that the order doesn't have yet (its row comes later): kept, at the end.
    let mut orphans: Vec<ElemId> = roots.iter().filter(|e| !placed.contains(e)).copied().collect();
    orphans.sort();
    for e in orphans {
        if take(e, &mut out, ids, pres, repair) {
            repair.order = true;
        }
    }
    pres.slides = out;
    rebuilt
}

fn comments(view: &TableView<'_>, pres: &mut Presentation, ids: &mut Ids) {
    let mut by_slide: HashMap<ElemId, (Vec<ElemId>, Vec<Comment>)> = HashMap::new();
    for row in view.rows() {
        let Some(Json::Object(mut o)) = view.cell(row, ONE).and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok())) else { continue };
        let Some(slide) = o.remove("slide").and_then(|s| s.as_str().and_then(|s| s.strip_prefix('@')).and_then(ElemId::from_hex)) else { continue };
        let Some(c) = decode::<Comment>(o, ids, pres) else { continue };
        let entry = by_slide.entry(slide).or_default();
        entry.0.push(row);
        entry.1.push(c);
    }
    for s in &mut pres.slides {
        let Some(se) = ids.elem(s.id.0) else { continue };
        let (elems, list) = by_slide.remove(&se).unwrap_or_default();
        if s.comments != list {
            Arc::make_mut(s).comments = list;
        }
        ids.comments.insert(se, elems);
    }
}

fn records<T: serde::de::DeserializeOwned>(view: &TableView<'_>, ids: &mut Ids, pres: &mut Presentation) -> (Vec<T>, Vec<ElemId>) {
    let mut list = Vec::new();
    let mut elems = Vec::new();
    for row in view.rows() {
        let Some(Json::Object(o)) = view.cell(row, ONE).and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok())) else { continue };
        if let Some(item) = decode::<T>(o, ids, pres) {
            list.push(item);
            elems.push(row);
        }
    }
    (list, elems)
}
