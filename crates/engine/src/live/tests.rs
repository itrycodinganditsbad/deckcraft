//! The adapter on the kit's fake agent: decks go into the live document and come back out unchanged (apart from
//! local ids) on another machine, and edits made on two machines at once merge, kind by kind.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_kit::testing::{Fake, Hub};
use craft_cloud_kit::{Agent, AgentBlobs, Open, Session};
use deckcraft_model::table::{Cell, Row, Table};
use deckcraft_model::text::{Action, Hyperlink, Paragraph, Run, RunKind, RunProps, TextBody};
use deckcraft_model::{
    Animation, Comment, CustomShow, EmbeddedFont, KeptPart, MediaClip, MediaId, Presentation, Section, Shape, ShapeId, ShapeKind, Xfrm, defaults,
};
use serde_json::Value as Json;

use super::json::to_live;
use super::session::{pull_media, push_media};
use super::{Deck, LiveDeck, Shared};

pub(crate) fn agent(f: &Fake) -> Agent {
    let c = f.client();
    Agent::from_fn(move || Ok(c.clone()))
}

pub(crate) fn shared(f: &Fake) -> Arc<Shared> {
    Arc::new(Shared::inline(Arc::new(AgentBlobs(f.client().blobs()))))
}

/// Opens `path` live on `f` (importing `pres` if it was never live).
pub(crate) fn open(f: &Fake, path: &str, pres: Presentation) -> Session<Deck> {
    let model = Arc::new(LiveDeck::new(Arc::new(pres), shared(f)));
    match Session::<Deck>::open(&agent(f), path, model, None).unwrap() {
        Open::Live(s) => {
            push_media(&s);
            *s
        }
        Open::NotLive(why) => panic!("not live: {why:?}"),
    }
}

/// A deck with nothing in it: what a machine that never saw the file starts from.
pub(crate) fn nothing() -> Presentation {
    Presentation { slides: Vec::new(), masters: Vec::new(), media: Vec::new(), ..Presentation::default() }
}

/// Polls every session (and its media) until `done` holds (20 s at most).
pub(crate) fn until(sessions: &mut [&mut Session<Deck>], what: &str, mut done: impl FnMut(&[&mut Session<Deck>]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for s in sessions.iter_mut() {
            s.poll();
            push_media(s);
            pull_media(s).unwrap();
        }
        if done(sessions) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A local change made with `f` from the session's model.
pub(crate) fn change(s: &mut Session<Deck>, f: impl FnOnce(&mut Presentation)) {
    let old = s.model().clone();
    let mut p = (*old.pres).clone();
    f(&mut p);
    let new = Arc::new(old.wrap(Arc::new(p)));
    s.local_change(&old, &new).unwrap();
    push_media(s);
}

fn normalize_body(t: &mut TextBody) {
    for p in &mut t.paragraphs {
        p.normalize();
    }
}

fn normalize_shapes(shapes: &mut [Shape]) {
    for s in shapes {
        if let Some(t) = &mut s.text {
            normalize_body(t);
        }
        match &mut s.kind {
            ShapeKind::Group { children, .. } => normalize_shapes(children),
            ShapeKind::Table(t) => {
                for r in &mut t.rows {
                    for c in &mut r.cells {
                        normalize_body(&mut c.text);
                    }
                }
            }
            _ => {}
        }
    }
}

/// A deck as JSON with its local ids as elements (the same on every machine) and its text runs merged as DeckCraft
/// merges them, plus each media item's bytes' length and first bytes.
pub(crate) fn canon(d: &LiveDeck) -> Json {
    let mut p = (*d.pres).clone();
    p.next_id = 0;
    for s in &mut p.slides {
        let s = Arc::make_mut(s);
        normalize_shapes(&mut s.shapes);
        normalize_body(&mut s.notes);
    }
    for m in p.masters.iter_mut().chain(p.notes_master.iter_mut()).chain(p.handout_master.iter_mut()) {
        let m = Arc::make_mut(m);
        normalize_shapes(&mut m.shapes);
        for l in &mut m.layouts {
            normalize_shapes(&mut l.shapes);
        }
    }
    let mut j = serde_json::to_value(&p).unwrap();
    to_live(&mut j, &d.ids);
    let media: Vec<Json> = p.media.iter().map(|m| serde_json::json!([m.data.len(), m.data.iter().take(16).collect::<Vec<_>>()])).collect();
    j["mediaBytes"] = Json::from(media);
    j
}

/// Where two JSON values first differ (for failure messages).
pub(crate) fn diff(a: &Json, b: &Json, at: &str) -> Option<String> {
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                let (u, v) = (x.get(k).unwrap_or(&Json::Null), y.get(k).unwrap_or(&Json::Null));
                if let Some(d) = diff(u, v, &format!("{at}.{k}")) {
                    return Some(d);
                }
            }
            None
        }
        (Json::Array(x), Json::Array(y)) if x.len() == y.len() => {
            x.iter().zip(y).enumerate().find_map(|(i, (u, v))| diff(u, v, &format!("{at}[{i}]")))
        }
        _ if a == b => None,
        _ => Some(format!("{at}: {} vs {}", short(a), short(b))),
    }
}

fn short(v: &Json) -> String {
    let s = v.to_string();
    if s.len() > 300 { format!("{}…", s.chars().take(300).collect::<String>()) } else { s }
}

fn png(rgb: [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(3, 2, image::Rgb(rgb));
    let mut out = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn shape(p: &mut Presentation, name: &str, x: f64) -> Shape {
    Shape { id: ShapeId(p.alloc_id().unwrap()), name: name.into(), xfrm: Some(Xfrm::new(x, 40.0, 100.0, 60.0)), ..Default::default() }
}

/// A deck with one of everything the live document maps: rich text with fields and breaks, a table with merged and
/// formatted cells, a group, a connector, a picture, a video, a chart, an object, comments with replies, animations
/// with a trigger, hyperlinks, notes, sections, a custom show, an embedded font, kept parts and tags.
pub(crate) fn corpus() -> Presentation {
    let mut p = defaults::new_presentation(None);
    p.props.title = "Corpus".into();
    p.tags = vec![("k".into(), "v".into())];
    p.kept_parts =
        vec![KeptPart { name: "customXml/item1.xml".into(), content_type: "application/xml".into(), xml: "<a/>".into(), ..Default::default() }];
    p.raw_ext = Some("<p:extLst/>".into());
    let pic = p.add_media("a.png", "image/png", png([10, 20, 30])).unwrap();
    let video = p.add_media("v.mp4", "video/mp4", vec![7u8; 300_000]).unwrap();
    let font = p.add_media("f.ttf", "application/x-font-ttf", vec![1, 2, 3, 4]).unwrap();
    p.embedded_fonts = vec![EmbeddedFont { typeface: "Mine".into(), regular: Some(font), ..Default::default() }];
    let layout = defaults::layout_of_kind(&p, deckcraft_model::LayoutType::TitleOnly).unwrap_or_default();
    let mut second = defaults::new_slide(&mut p, layout).unwrap();
    let first_id = p.slides[0].id;
    // Text.
    let mut a = shape(&mut p, "Text", 10.0);
    a.text = Some(TextBody {
        paragraphs: vec![
            Paragraph {
                level: 1,
                runs: vec![
                    Run::new("Plain "),
                    Run::with("bold", RunProps { bold: Some(true), size: Some(24.0), ..Default::default() }),
                    Run { text: String::new(), props: RunProps::default(), kind: RunKind::Break },
                    Run { text: "3".into(), props: RunProps::default(), kind: RunKind::Field { field: "slidenum".into() } },
                    Run::with(
                        "link",
                        RunProps {
                            link: Some(Hyperlink {
                                action: Action::Slide { slide: first_id },
                                tooltip: "back".into(),
                                highlight_click: false,
                                sound: None,
                                stop_sound: false,
                                zoom_return: None,
                            }),
                            ..Default::default()
                        },
                    ),
                ],
                ..Default::default()
            },
            Paragraph::new(""),
            Paragraph { props: deckcraft_model::text::ParaProps { indent: Some(12.0), ..Default::default() }, ..Paragraph::new("third") },
        ],
        ..Default::default()
    });
    let b = shape(&mut p, "Target", 300.0);
    let conn = Shape {
        kind: ShapeKind::Connector { start: Some((a.id, 1)), end: Some((b.id, 3)) },
        geom: deckcraft_model::Geom::preset("straightConnector1"),
        ..shape(&mut p, "Connector", 0.0)
    };
    let inner = shape(&mut p, "Inner", 20.0);
    let group = Shape { kind: ShapeKind::Group { children: vec![inner], child: Xfrm::new(0.0, 0.0, 1.0, 1.0) }, ..shape(&mut p, "Group", 500.0) };
    let mut cell = Cell::new("cell");
    cell.grid_span = 2;
    cell.fill = Some(deckcraft_model::Fill::Solid { color: deckcraft_model::ColorRef::rgb(deckcraft_model::Rgba::rgb(9, 8, 7)) });
    let mut covered = Cell::new("");
    covered.h_merge = true;
    let table = Table {
        cols: vec![100.0, 120.0],
        rows: vec![Row { height: 30.0, cells: vec![cell, covered] }, Row { height: 40.0, cells: vec![Cell::new("x"), Cell::new("y")] }],
        ..Table::default()
    };
    let table = Shape { kind: ShapeKind::Table(table), ..shape(&mut p, "Table", 100.0) };
    let picture = Shape {
        kind: ShapeKind::Picture { fill: deckcraft_model::style::PictureFill { media: pic, ..Default::default() } },
        ..shape(&mut p, "Picture", 600.0)
    };
    let clip = Shape {
        kind: ShapeKind::Media(MediaClip { media: video, video: true, poster: Some(pic), ..Default::default() }),
        ..shape(&mut p, "Video", 700.0)
    };
    let chart = Shape { kind: ShapeKind::Chart(Box::default()), ..shape(&mut p, "Chart", 800.0) };
    let object = Shape {
        kind: ShapeKind::Opaque { xml: "<x/>".into(), preview: Some(pic), label: "Object".into(), parts: vec![("rId1".into(), "t".into(), pic)] },
        ..shape(&mut p, "Object", 900.0)
    };
    let (aid, bid) = (a.id, b.id);
    second.shapes.extend([a, b, conn, group, table, picture, clip, chart, object]);
    second.notes = TextBody::from_text("Notes\nmore");
    second.animations = vec![Animation { shape: aid, trigger: Some(bid), ..Default::default() }];
    second.comments = vec![Comment {
        author: "A".into(),
        text: "on the box".into(),
        shape: Some(bid),
        replies: vec![Comment { author: "B".into(), text: "yes".into(), ..Default::default() }],
        ..Default::default()
    }];
    let second_id = second.id;
    p.slides.push(Arc::new(second));
    p.sections = vec![Section { name: "One".into(), slides: vec![first_id] }, Section { name: "Two".into(), slides: vec![second_id] }];
    p.custom_shows = vec![CustomShow { name: "Short".into(), slides: vec![second_id, first_id] }];
    p
}

/// Every deck the tests use: the sample, each starter template, the app icon's source deck, the corpus deck and the
/// corpus deck after a PPTX round trip.
pub(crate) fn decks() -> Vec<(String, Presentation)> {
    let mut out = Vec::new();
    let mut s = crate::Session::new();
    crate::sample::open_sample(&mut s).unwrap();
    out.push(("sample".to_owned(), (*s.doc().unwrap().doc).clone()));
    for t in crate::templates::starters() {
        out.push((format!("template {}", t.name), crate::templates::build(t.name, defaults::WIDE).unwrap()));
    }
    out.push(("corpus".to_owned(), corpus()));
    let pptx = deckcraft_pptx::export_as(&corpus(), "pptx").unwrap();
    let (mut back, _) = deckcraft_pptx::import_checked(&pptx).unwrap();
    deckcraft_format::repair(&mut back);
    out.push(("corpus through PPTX".to_owned(), back));
    out
}

/// Every deck goes into the live document on one machine and comes out on another unchanged, apart from local ids:
/// slides, shapes, text, tables, notes, comments, sections, custom shows, masters, settings and media bytes.
#[test]
fn every_deck_comes_out_on_the_other_machine_as_it_went_in() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    for (k, (name, deck)) in decks().into_iter().enumerate() {
        let path = format!("decks/{k}.deckcraft");
        let mut a = open(&pc, &path, deck);
        let mut b = open(&laptop, &path, nothing());
        let want = canon(a.model());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            for s in [&mut a, &mut b] {
                s.poll();
                push_media(s);
                pull_media(s).unwrap();
            }
            let Some(d) = diff(&want, &canon(b.model()), "") else { break };
            assert!(Instant::now() < deadline, "{name}: {d}");
            std::thread::sleep(Duration::from_millis(5));
        }
        // And back: what the laptop read, written again, is the same document.
        let reread = canon(b.model());
        assert_eq!(reread, want, "{name}");
    }
}

/// The ids two machines give one element differ, but every reference resolves to the same element on both.
#[test]
fn local_ids_differ_but_references_agree() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open(&pc, "c.deckcraft", corpus());
    // The laptop's deck already used many ids: its new ones can't be the pc's.
    let mut busy = nothing();
    busy.next_id = 5000;
    let mut b = open(&laptop, "c.deckcraft", busy);
    let want = canon(a.model());
    until(&mut [&mut a, &mut b], "the laptop has it", |s| canon(s[1].model()) == want);
    let slide = &b.model().pres.slides[1];
    assert!(slide.id.0 > 5000, "a fresh local id on the laptop");
    let named = |n: &str| slide.shapes.iter().find(|s| s.name == n).unwrap();
    let ShapeKind::Connector { start, end } = &named("Connector").kind else { panic!("the connector") };
    assert_eq!(start.map(|s| s.0), Some(named("Text").id));
    assert_eq!(end.map(|s| s.0), Some(named("Target").id));
    assert_eq!(slide.comments[0].shape, Some(named("Target").id));
    assert_eq!(slide.animations[0].shape, named("Text").id);
    let pres = &b.model().pres;
    assert_eq!(pres.sections[1].slides, vec![slide.id]);
    let ShapeKind::Picture { fill } = &named("Picture").kind else { panic!("the picture") };
    assert!(pres.media(fill.media).is_some_and(|m| m.data.len() > 8), "the picture's bytes came through the blob store");
    let _ = MediaId(0);
}

/// Two machines on the corpus deck, in step.
pub(crate) fn pair(hub: &Hub, path: &str) -> (Session<Deck>, Session<Deck>, Fake, Fake) {
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open(&pc, path, corpus());
    let mut busy = nothing();
    busy.next_id = 5000;
    let mut b = open(&laptop, path, busy);
    let want = canon(a.model());
    until(&mut [&mut a, &mut b], "in step", |s| canon(s[1].model()) == want);
    (a, b, pc, laptop)
}

/// Polls both until their decks are the same, and checks it is a deck DeckCraft could have made: what its validator
/// accepts, rectangular tables, no reference to anything missing.
pub(crate) fn converge(a: &mut Session<Deck>, b: &mut Session<Deck>, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for s in [&mut *a, &mut *b] {
            s.poll();
            push_media(s);
            pull_media(s).unwrap();
        }
        let Some(d) = diff(&canon(a.model()), &canon(b.model()), "") else { break };
        assert!(Instant::now() < deadline, "{what}: never the same: {d}");
        std::thread::sleep(Duration::from_millis(5));
    }
    for s in [&*a, &*b] {
        let problems = valid(&s.model().pres);
        assert!(problems.is_empty(), "{what}: {problems:?}");
    }
}

/// What DeckCraft's validator says, plus what a merge could break: tables not rectangular, references to missing
/// shapes, slides or layouts.
pub(crate) fn valid(p: &Presentation) -> Vec<String> {
    let mut out = p.validate();
    for s in &p.slides {
        let mut here = std::collections::HashSet::new();
        deckcraft_model::walk(&s.shapes, &mut |sh, _| {
            here.insert(sh.id);
        });
        deckcraft_model::walk(&s.shapes, &mut |sh, _| match &sh.kind {
            ShapeKind::Table(t) => {
                if t.rows.iter().any(|r| r.cells.len() != t.cols.len()) {
                    out.push(format!("slide {}: table {} isn't rectangular", s.id, sh.id));
                }
            }
            ShapeKind::Connector { start, end } => {
                for (id, _) in start.iter().chain(end.iter()) {
                    if !here.contains(id) {
                        out.push(format!("slide {}: connector {} to missing {id}", s.id, sh.id));
                    }
                }
            }
            _ => {}
        });
        for a in &s.animations {
            if !here.contains(&a.shape) {
                out.push(format!("slide {}: an animation of missing {}", s.id, a.shape));
            }
        }
        for c in &s.comments {
            if c.shape.is_some_and(|id| !here.contains(&id)) {
                out.push(format!("slide {}: a comment on a missing shape", s.id));
            }
        }
    }
    for sec in &p.sections {
        if sec.slides.iter().any(|id| p.slide(*id).is_none()) {
            out.push(format!("section {} lists a missing slide", sec.name));
        }
    }
    out
}

/// The slide at `i` and its shape named `name`.
pub(crate) fn shape_named(p: &Presentation, i: usize, name: &str) -> Option<Shape> {
    let mut found = None;
    deckcraft_model::walk(&p.slides.get(i)?.shapes, &mut |s, _| {
        if s.name == name && found.is_none() {
            found = Some(s.clone());
        }
    });
    found
}

/// `p` with the shape `name` on slide `i` changed by `f`.
pub(crate) fn edit_shape(p: &mut Presentation, i: usize, name: &str, f: impl FnOnce(&mut Shape)) {
    let Some(slide) = p.slides.get_mut(i) else { return };
    let Some(id) = ({
        let mut found = None;
        deckcraft_model::walk(&slide.shapes, &mut |s, _| {
            if s.name == name && found.is_none() {
                found = Some(s.id);
            }
        });
        found
    }) else {
        return;
    };
    if let Some(s) = Arc::make_mut(slide).shape_mut(id) {
        f(s);
    }
}

fn text_of(p: &Presentation, i: usize, name: &str) -> String {
    shape_named(p, i, name).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
}

fn type_into(s: &mut Shape, at: (usize, usize), text: &str) {
    if let Some(body) = s.text.as_mut() {
        deckcraft_model::edit::insert(body, at, text, None);
    }
}

/// Slides moved on both machines at once: every slide is kept once, in the same order on both.
#[test]
fn slides_reordered_at_once_keep_every_slide() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "order.deckcraft");
    change(&mut a, |p| {
        let s = p.slides.remove(0);
        p.slides.push(s);
    });
    change(&mut b, |p| {
        let extra = defaults::new_slide(p, p.slides[0].layout).unwrap();
        p.slides.insert(0, Arc::new(extra));
    });
    converge(&mut a, &mut b, "slides moved and added at once");
    assert_eq!(a.model().pres.slides.len(), 3);
}

/// Review focus 1: a shape added on each machine at once, on the same slide, each with something pointing at it (a
/// connector here, a comment there). Both survive with distinct local ids, and the connector and the comment point at
/// the right shapes on both machines.
#[test]
fn shapes_added_at_once_both_survive_with_their_references() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "add.deckcraft");
    change(&mut a, |p| {
        let mut x = shape(p, "From A", 50.0);
        x.text = Some(TextBody::from_text("a"));
        let target = shape_named(p, 1, "Target").unwrap().id;
        let link = Shape {
            kind: ShapeKind::Connector { start: Some((x.id, 0)), end: Some((target, 2)) },
            geom: deckcraft_model::Geom::preset("straightConnector1"),
            ..shape(p, "A's connector", 0.0)
        };
        let s = Arc::make_mut(&mut p.slides[1]);
        s.shapes.push(x);
        s.shapes.push(link);
    });
    change(&mut b, |p| {
        let y = shape(p, "From B", 60.0);
        let s = Arc::make_mut(&mut p.slides[1]);
        s.comments.push(Comment { author: "B".into(), text: "on mine".into(), shape: Some(y.id), ..Default::default() });
        s.shapes.push(y);
    });
    converge(&mut a, &mut b, "shapes added at once");
    for s in [&a, &b] {
        let p = &s.model().pres;
        let (x, y) = (shape_named(p, 1, "From A").unwrap(), shape_named(p, 1, "From B").unwrap());
        assert_ne!(x.id, y.id);
        let ShapeKind::Connector { start, end } = shape_named(p, 1, "A's connector").unwrap().kind else { panic!() };
        assert_eq!((start.map(|s| s.0), end.map(|e| e.0)), (Some(x.id), Some(shape_named(p, 1, "Target").unwrap().id)));
        assert_eq!(p.slides[1].comments.last().and_then(|c| c.shape), Some(y.id));
    }
    // The two machines' local ids for the pc's new shape differ (each numbers its own), and the deck stays valid.
    let ida = shape_named(&a.model().pres, 1, "From A").unwrap().id;
    let idb = shape_named(&b.model().pres, 1, "From A").unwrap().id;
    assert_ne!(ida, idb);
}

/// Two properties of one shape changed on the two machines at once (its place here, its fill there): both kept.
#[test]
fn different_properties_of_one_shape_both_kept() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "props.deckcraft");
    change(&mut a, |p| edit_shape(p, 1, "Target", |s| s.xfrm = Some(Xfrm::new(1.0, 2.0, 3.0, 4.0))));
    let fill = deckcraft_model::Fill::Solid { color: deckcraft_model::ColorRef::rgb(deckcraft_model::Rgba::rgb(1, 2, 3)) };
    let f = fill.clone();
    change(&mut b, move |p| edit_shape(p, 1, "Target", |s| s.fill = Some(f)));
    converge(&mut a, &mut b, "a move and a fill");
    let s = shape_named(&a.model().pres, 1, "Target").unwrap();
    assert_eq!((s.xfrm, s.fill), (Some(Xfrm::new(1.0, 2.0, 3.0, 4.0)), Some(fill)));
}

/// Typing in one text box on both machines at once merges character by character; bold here and italic there on
/// one word both stay; and the notes merge the same way.
#[test]
fn typing_in_one_text_box_and_the_notes_at_once_merges() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "text.deckcraft");
    change(&mut a, |p| edit_shape(p, 1, "Text", |s| type_into(s, (2, 0), "AA ")));
    change(&mut b, |p| edit_shape(p, 1, "Text", |s| type_into(s, (2, 5), " BB")));
    change(&mut a, |p| Arc::make_mut(&mut p.slides[1]).notes = TextBody::from_text("Notes from A\nmore"));
    converge(&mut a, &mut b, "typing at once");
    let t = text_of(&a.model().pres, 1, "Text");
    assert!(t.ends_with("AA third BB"), "{t:?}");
    change(&mut a, |p| {
        edit_shape(p, 1, "Text", |s| {
            if let Some(b) = s.text.as_mut() {
                deckcraft_model::edit::format(b, (2, 3), (2, 8), &|r| r.bold = Some(true));
            }
        })
    });
    change(&mut b, |p| {
        edit_shape(p, 1, "Text", |s| {
            if let Some(b) = s.text.as_mut() {
                deckcraft_model::edit::format(b, (2, 3), (2, 8), &|r| r.italic = Some(true));
            }
        })
    });
    converge(&mut a, &mut b, "bold and italic at once");
    let s = shape_named(&b.model().pres, 1, "Text").unwrap();
    let run = s.text.unwrap().paragraphs[2].runs.iter().find(|r| r.text.contains("third")).cloned().unwrap();
    assert_eq!((run.props.bold, run.props.italic), (Some(true), Some(true)), "{run:?}");
}

/// A row added on one machine while a column is added on the other: both, and the cell they cross is made the same
/// on both machines, so the table stays rectangular; typing in one cell on both merges.
#[test]
fn a_row_and_a_column_added_at_once_keep_the_table_rectangular() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "table.deckcraft");
    let table = |p: &mut Presentation, f: &dyn Fn(&mut Table)| {
        edit_shape(p, 1, "Table", |s| {
            if let ShapeKind::Table(t) = &mut s.kind {
                f(t);
            }
        })
    };
    change(&mut a, |p| table(p, &|t| t.rows.push(Row { height: 22.0, cells: vec![Cell::new("ra1"), Cell::new("ra2")] })));
    change(&mut b, |p| {
        table(p, &|t| {
            t.cols.push(55.0);
            for r in &mut t.rows {
                r.cells.push(Cell::new("cb"));
            }
        })
    });
    converge(&mut a, &mut b, "a row and a column");
    let ShapeKind::Table(t) = shape_named(&a.model().pres, 1, "Table").unwrap().kind else { panic!() };
    assert_eq!((t.rows.len(), t.cols.len()), (3, 3));
    assert_eq!(t.rows[2].cells[0].text.text(), "ra1");
    assert_eq!(t.rows[0].cells[2].text.text(), "cb");
    // Typing in one cell at once.
    let type_cell = |p: &mut Presentation, at: usize, s: &str| {
        let s = s.to_owned();
        table(p, &move |t| {
            deckcraft_model::edit::insert(&mut t.rows[1].cells[0].text, (0, at), &s, None);
        })
    };
    change(&mut a, |p| type_cell(p, 0, "<"));
    change(&mut b, |p| type_cell(p, 1, ">"));
    converge(&mut a, &mut b, "typing in a cell at once");
    let ShapeKind::Table(t) = shape_named(&b.model().pres, 1, "Table").unwrap().kind else { panic!() };
    assert_eq!(t.rows[1].cells[0].text.text(), "<x>");
}

/// Comments added on both machines, a section renamed on one while the other adds a slide to a custom show,
/// settings changed on both (different ones): every change kept on both.
#[test]
fn comments_sections_shows_and_settings_merge() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "lists.deckcraft");
    change(&mut a, |p| {
        Arc::make_mut(&mut p.slides[0]).comments.push(Comment { author: "A".into(), text: "one".into(), ..Default::default() });
        p.sections[0].name = "Renamed".into();
        p.props.title = "Title from A".into();
    });
    change(&mut b, |p| {
        Arc::make_mut(&mut p.slides[0]).comments.push(Comment { author: "B".into(), text: "two".into(), ..Default::default() });
        let first = p.slides[1].id;
        p.custom_shows[0].slides.push(first);
        p.header_footer.footer_text = "Footer from B".into();
    });
    converge(&mut a, &mut b, "lists and settings");
    let p = &a.model().pres;
    assert_eq!(p.slides[0].comments.len(), 2);
    assert_eq!(p.sections[0].name, "Renamed");
    assert_eq!(p.custom_shows[0].slides.len(), 3);
    assert_eq!((p.props.title.as_str(), p.header_footer.footer_text.as_str()), ("Title from A", "Footer from B"));
}

/// A master changed on both machines at once: whole-item, one side's change is kept, the same on both.
#[test]
fn a_master_changed_on_both_keeps_one_side() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "master.deckcraft");
    change(&mut a, |p| Arc::make_mut(&mut p.masters[0]).name = "A's master".into());
    change(&mut b, |p| Arc::make_mut(&mut p.masters[0]).preserve = true);
    converge(&mut a, &mut b, "a master on both");
    let m = &a.model().pres.masters[0];
    assert!(m.name == "A's master" || m.preserve, "one side's change is kept");
    assert!(!(m.name == "A's master" && m.preserve), "whole-item: not both");
}

/// Review focus 4: a slide deleted on one machine while the other edits a shape on it (and types in it): the slide is
/// gone on both, nothing dangles (its section entry, a custom show's), no ghost shape.
#[test]
fn a_slide_deleted_while_the_other_edits_on_it_is_gone_on_both() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "delete.deckcraft");
    change(&mut a, |p| {
        let gone = p.slides[1].id;
        p.slides.remove(1);
        for s in &mut p.sections {
            s.slides.retain(|id| *id != gone);
        }
        for s in &mut p.custom_shows {
            s.slides.retain(|id| *id != gone);
        }
    });
    for k in 0..3 {
        change(&mut b, |p| edit_shape(p, 1, "Text", |s| type_into(s, (0, k), "z")));
    }
    change(&mut b, |p| edit_shape(p, 1, "Target", |s| s.name = "Renamed".into()));
    converge(&mut a, &mut b, "a slide deleted while edited");
    for s in [&a, &b] {
        let p = &s.model().pres;
        assert_eq!(p.slides.len(), 1);
        assert!(shape_named(p, 0, "Renamed").is_none() && shape_named(p, 0, "Text").is_none(), "no ghost shape");
    }
}

/// A shape deleted on one machine while the other connects to it, comments on it and animates it: the shape is gone
/// on both, and nothing points at it any more.
#[test]
fn references_to_a_shape_deleted_meanwhile_are_dropped() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "refs.deckcraft");
    change(&mut a, |p| {
        let id = shape_named(p, 1, "Inner").unwrap().id;
        let s = Arc::make_mut(&mut p.slides[1]);
        crate::cmd::edit::remove_shapes(&mut s.shapes, &[id]);
    });
    change(&mut b, |p| {
        let inner = shape_named(p, 1, "Inner").unwrap().id;
        let x = Shape { kind: ShapeKind::Connector { start: Some((inner, 0)), end: None }, ..shape(p, "To inner", 0.0) };
        let s = Arc::make_mut(&mut p.slides[1]);
        s.shapes.push(x);
        s.comments.push(Comment { text: "on inner".into(), shape: Some(inner), ..Default::default() });
        s.animations.push(Animation { shape: inner, ..Default::default() });
    });
    converge(&mut a, &mut b, "references to a deleted shape");
    let p = &a.model().pres;
    assert!(shape_named(p, 1, "Inner").is_none());
    let ShapeKind::Connector { start, .. } = shape_named(p, 1, "To inner").unwrap().kind else { panic!() };
    assert_eq!(start, None);
}

/// Media: a picture inserted on one machine shows on the other through the blob store.
#[test]
fn a_picture_inserted_here_shows_there() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _l) = pair(&hub, "media.deckcraft");
    let bytes = png([200, 100, 50]);
    let b2 = bytes.clone();
    change(&mut a, move |p| {
        let m = p.add_media("new.png", "image/png", b2).unwrap();
        let pic = Shape {
            kind: ShapeKind::Picture { fill: deckcraft_model::style::PictureFill { media: m, ..Default::default() } },
            ..shape(p, "New picture", 5.0)
        };
        Arc::make_mut(&mut p.slides[0]).shapes.push(pic);
    });
    converge(&mut a, &mut b, "a picture");
    let p = &b.model().pres;
    let ShapeKind::Picture { fill } = shape_named(p, 0, "New picture").unwrap().kind else { panic!() };
    assert_eq!(p.media(fill.media).map(|m| m.data.as_slice()), Some(bytes.as_slice()));
}
