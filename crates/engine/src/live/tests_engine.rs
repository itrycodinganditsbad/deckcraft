//! Two DeckCraft sessions (the engine's, as the window drives it) on the kit's fake agent: the hook, undo, open,
//! autosave and the banners, and the review's cases: shapes added at once, typing in one box then undo, a remote
//! change mid-gesture and mid-composition, a slide deleted while edited, and media.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_kit::testing::{Fake, Hub};
use deckcraft_model::{Presentation, ShapeId};
use serde_json::{Value, json};

use super::session::{AUTOSAVE_AFTER, Banner, open_at};
use super::tests::{agent, canon, corpus, diff};
use crate::tools::{Mods, PointerEvent, PointerKind};
use crate::{Session, cmd};

/// A folder of its own for one machine's files.
pub(crate) struct Folder(pub(crate) PathBuf);

impl Folder {
    pub(crate) fn new(name: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("deckcraft-live-{}-{name}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens `rel` in a new engine session on `f`'s machine, live (the file in `dir` is written from `pres` first, as
/// the file sync would have brought it).
pub(crate) fn engine_open(f: &Fake, dir: &Folder, rel: &str, pres: &Presentation, machine: &str) -> Session {
    engine_join(f, dir, rel, &deckcraft_format::save(pres).unwrap(), machine)
}

/// Opens `rel` live, its file in `dir` being `bytes` (another machine's save, as the file sync brought it).
pub(crate) fn engine_join(f: &Fake, dir: &Folder, rel: &str, bytes: &[u8], machine: &str) -> Session {
    let abs = dir.0.join(rel);
    std::fs::write(&abs, bytes).unwrap();
    let mut s = Session::new();
    s.enable_live(agent(f), None);
    let doc = deckcraft_format::load(bytes).unwrap();
    let open = open_at(&agent(f), rel, &doc, bytes, machine.into()).unwrap().unwrap();
    let path = abs.to_string_lossy().into_owned();
    s.stage_live(&path, open, Some(doc));
    s.execute("file.adopt", &json!({"path": path})).unwrap();
    assert!(s.is_live(), "opened live");
    s
}

fn deck(s: &Session) -> Arc<Presentation> {
    s.active().unwrap().doc.clone()
}

/// The two sessions polled until their presentations are the same (20 s at most).
fn converge(a: &mut Session, b: &mut Session, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        a.poll_live();
        b.poll_live();
        let (x, y) = (a.live_kit().unwrap().model().clone(), b.live_kit().unwrap().model().clone());
        // The engine's documents are the kits' models.
        if Arc::ptr_eq(&x.pres, &deck(a)) && Arc::ptr_eq(&y.pres, &deck(b)) {
            match diff(&canon(&x), &canon(&y), "") {
                None => break,
                Some(d) => assert!(Instant::now() < deadline, "{what}: never the same: {d}"),
            }
        }
        assert!(Instant::now() < deadline, "{what}: the documents never caught up with the kits");
        std::thread::sleep(Duration::from_millis(5));
    }
    for s in [&*a, &*b] {
        let problems = super::tests::valid(&deck(s));
        assert!(problems.is_empty(), "{what}: {problems:?}");
    }
}

fn pair(hub: &Hub, rel: &str) -> (Session, Session, Folder, Folder, Fake, Fake) {
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let (da, db) = (Folder::new("pc"), Folder::new("laptop"));
    let mut a = engine_open(&pc, &da, rel, &corpus(), "pc");
    // The pc saves; the laptop's file is that save (synced). Its ids are its own.
    a.execute("file.save", &json!({})).unwrap();
    let saved = std::fs::read(da.0.join(rel)).unwrap();
    let mut b = engine_join(&laptop, &db, rel, &saved, "laptop");
    assert_eq!(b.live_banner(), None, "a Craft save isn't an outside edit");
    converge(&mut a, &mut b, "in step");
    (a, b, da, db, pc, laptop)
}

fn insert(s: &mut Session, x: f64) -> ShapeId {
    s.execute("slide.go", &json!({"index": 1})).unwrap();
    let v = s.execute("shape.insert", &json!({"preset": "rect", "rect": [x, 300, 80, 60]})).unwrap();
    ShapeId(u32::try_from(v["id"].as_u64().unwrap()).unwrap())
}

fn text(s: &Session, id: ShapeId) -> String {
    s.active().unwrap().shape(id).and_then(|sh| sh.text.clone()).map(|t| t.text()).unwrap_or_default()
}

fn named(s: &Session, name: &str) -> Option<ShapeId> {
    let p = deck(s);
    let mut found = None;
    for sl in &p.slides {
        deckcraft_model::walk(&sl.shapes, &mut |sh, _| {
            if sh.name == name && found.is_none() {
                found = Some(sh.id);
            }
        });
    }
    found
}

/// Review focus 1, through commands: a shape inserted on each machine at once; both survive, with distinct local ids
/// on each machine; a connector the pc then draws between them, and a comment the laptop puts on the pc's, point at
/// the right shapes on both.
#[test]
fn shapes_inserted_at_once_both_survive_and_connect() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "focus1.deckcraft");
    let xa = insert(&mut a, 10.0);
    a.execute("shape.rename", &json!({"id": xa.0, "name": "pc's"})).unwrap();
    let xb = insert(&mut b, 400.0);
    b.execute("shape.rename", &json!({"id": xb.0, "name": "laptop's"})).unwrap();
    converge(&mut a, &mut b, "two inserts");
    let (pa, pb) = (named(&a, "pc's").unwrap(), named(&a, "laptop's").unwrap());
    assert_ne!(pa, pb);
    a.execute("shape.connect", &json!({"from": pa.0, "to": pb.0})).unwrap();
    let on_b = named(&b, "pc's").unwrap();
    b.execute("comment.add", &json!({"text": "nice", "id": on_b.0})).unwrap();
    converge(&mut a, &mut b, "a connector and a comment");
    for s in [&a, &b] {
        let p = deck(s);
        let slide = &p.slides[1];
        let (mine, theirs) = (named(s, "pc's").unwrap(), named(s, "laptop's").unwrap());
        let ends: Vec<(Option<ShapeId>, Option<ShapeId>)> = slide
            .shapes
            .iter()
            .filter_map(|sh| match &sh.kind {
                deckcraft_model::ShapeKind::Connector { start, end } => Some((start.map(|x| x.0), end.map(|x| x.0))),
                _ => None,
            })
            .collect();
        assert!(ends.contains(&(Some(mine), Some(theirs))), "{ends:?}");
        assert_eq!(slide.comments.last().and_then(|c| c.shape), Some(mine));
    }
}

/// Review focus 2: typing into one text box on both machines at once, keystroke by keystroke through `text.insert`,
/// then the pc undoes: only the pc's typing goes, on both machines.
#[test]
fn typing_in_one_box_on_both_then_undo_takes_back_only_ones_own() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "focus2.deckcraft");
    let (ta, tb) = (named(&a, "Target").unwrap(), named(&b, "Target").unwrap());
    a.execute("slide.go", &json!({"index": 1})).unwrap();
    b.execute("slide.go", &json!({"index": 1})).unwrap();
    a.execute("text.edit", &json!({"id": ta.0, "at": [0, 0]})).unwrap();
    b.execute("text.edit", &json!({"id": tb.0, "at": [0, 0]})).unwrap();
    for (ca, cb) in [("a", "X"), ("b", "Y"), ("c", "Z")] {
        a.execute("text.insert", &json!({"text": ca})).unwrap();
        b.execute("text.insert", &json!({"text": cb})).unwrap();
        a.poll_live();
        b.poll_live();
    }
    converge(&mut a, &mut b, "typing on both");
    let both = text(&a, ta);
    assert_eq!(both.chars().filter(|c| "abc".contains(*c)).collect::<String>(), "abc", "{both:?}");
    assert_eq!(both.chars().filter(|c| "XYZ".contains(*c)).collect::<String>(), "XYZ", "{both:?}");
    // The caret moved with the other machine's typing: the pc's next keystroke lands after its own "c".
    a.execute("text.insert", &json!({"text": "d"})).unwrap();
    converge(&mut a, &mut b, "one more");
    let t = text(&a, ta);
    assert!(t.contains("cd") || t.find('d') > t.find('c'), "{t:?}");
    for _ in 0..4 {
        a.execute("edit.undo", &json!({})).unwrap();
    }
    converge(&mut a, &mut b, "the pc's undo");
    for (s, id) in [(&a, ta), (&b, tb)] {
        let t = text(s, id);
        assert!(!t.contains(['a', 'b', 'c', 'd']), "the pc's typing went: {t:?}");
        assert_eq!(t.chars().filter(|c| "XYZ".contains(*c)).collect::<String>(), "XYZ", "the laptop's stayed: {t:?}");
    }
    // Redo brings one keystroke back.
    a.execute("edit.redo", &json!({})).unwrap();
    converge(&mut a, &mut b, "a redo");
    assert!(text(&b, tb).contains('a'));
}

/// Review focus 3: a remote change arriving mid-gesture (a drag here while the laptop renames a shape) waits for the
/// gesture's end; then the drag's result is there once and the rename too. The same for an input method's
/// composition.
#[test]
fn a_remote_change_mid_gesture_waits_for_its_end() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "focus3.deckcraft");
    let id = insert(&mut a, 100.0);
    converge(&mut a, &mut b, "the shape");
    let before = a.active().unwrap().shape(id).and_then(|s| s.xfrm).unwrap();
    let ev = |kind, x: f64, y: f64| PointerEvent { kind, x, y, mods: Mods::default(), tol: 4.0, pressure: None };
    a.execute("edit.select", &json!({"ids": [id.0]})).unwrap();
    a.pointer(ev(PointerKind::Down, before.x + 10.0, before.y + 10.0)).unwrap();
    a.pointer(ev(PointerKind::Drag, before.x + 30.0, before.y + 10.0)).unwrap();
    // Meanwhile the laptop renames another shape.
    let other = named(&b, "Target").unwrap();
    b.execute("slide.go", &json!({"index": 1})).unwrap();
    b.execute("shape.rename", &json!({"id": other.0, "name": "Renamed on the laptop"})).unwrap();
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        b.poll_live();
        a.poll_live();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(named(&a, "Renamed on the laptop").is_none(), "held while the drag runs");
    a.pointer(ev(PointerKind::Drag, before.x + 60.0, before.y + 10.0)).unwrap();
    a.pointer(ev(PointerKind::Up, before.x + 60.0, before.y + 10.0)).unwrap();
    converge(&mut a, &mut b, "after the drag");
    let moved = a.active().unwrap().shape(id).and_then(|s| s.xfrm).unwrap();
    assert!((moved.x - (before.x + 50.0)).abs() < 1.0, "moved once by the drag: {} → {}", before.x, moved.x);
    assert!(named(&a, "Renamed on the laptop").is_some());
    // Undo of the drag is one step.
    a.execute("edit.undo", &json!({})).unwrap();
    converge(&mut a, &mut b, "the drag undone");
    assert!((a.active().unwrap().shape(id).and_then(|s| s.xfrm).unwrap().x - before.x).abs() < 0.01);
    // A composition: held too, and committed as one change.
    let ta = named(&a, "Text").unwrap();
    a.execute("slide.go", &json!({"index": 1})).unwrap();
    a.execute("text.edit", &json!({"id": ta.0, "at": [0, 0]})).unwrap();
    a.execute("text.compose", &json!({"text": "ni"})).unwrap();
    let tb = named(&b, "Renamed on the laptop").unwrap();
    b.execute("shape.rename", &json!({"id": tb.0, "name": "Again"})).unwrap();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        b.poll_live();
        a.poll_live();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(named(&a, "Again").is_none(), "held while composing");
    a.execute("text.compose", &json!({"text": "日本"})).unwrap();
    a.execute("text.commit", &json!({"text": "日本"})).unwrap();
    converge(&mut a, &mut b, "after the composition");
    assert!(named(&a, "Again").is_some());
    let tb_text = text(&b, named(&b, "Text").unwrap());
    assert!(tb_text.starts_with("日本"), "{tb_text:?}");
    assert!(!tb_text.contains("ni"), "the preedit never went out: {tb_text:?}");
}

/// Review focus 3, a dialog: the pc opens Size and Position on a shape, the laptop fills that shape meanwhile, then the
/// pc presses OK with the values the dialog showed and a new width. The dialog writes only the box: both the width and
/// the laptop's fill survive, on both machines.
#[test]
fn a_remote_fill_survives_an_open_size_and_position_dialog() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "dialog.deckcraft");
    let id = insert(&mut a, 100.0);
    converge(&mut a, &mut b, "the shape");
    a.execute("edit.select", &json!({"ids": [id.0]})).unwrap();
    a.execute("shape.sizePosition", &json!({})).unwrap();
    let shown = match a.ui_requests.last() {
        Some(crate::UiRequest::Dialog { id, params }) if id == "sizePosition" => params.clone(),
        other => panic!("the dialog didn't open: {other:?}"),
    };
    // Meanwhile the laptop fills the shape.
    let on_b = deck(&b).slides[1]
        .shapes
        .iter()
        .find(|sh| sh.xfrm.is_some_and(|x| (x.x - 100.0).abs() < 0.01 && (x.y - 300.0).abs() < 0.01))
        .map(|sh| sh.id)
        .unwrap();
    b.execute("slide.go", &json!({"index": 1})).unwrap();
    b.execute("shape.fill", &json!({"ids": [on_b.0], "color": "#00ff00"})).unwrap();
    let filled = b.active().unwrap().shape(on_b).and_then(|sh| sh.fill.clone());
    assert!(filled.is_some());
    converge(&mut a, &mut b, "the laptop's fill");
    assert_eq!(a.active().unwrap().shape(id).and_then(|sh| sh.fill.clone()), filled, "the fill arrived with the dialog open");
    // OK: what the dialog sends, every field, the width changed.
    let mut ok = shown.clone();
    ok["w"] = json!(200.0);
    ok.as_object_mut().unwrap().retain(|k, _| ["x", "y", "w", "h", "rot", "lockAspect"].contains(&k.as_str()));
    a.execute("shape.sizePosition", &ok).unwrap();
    converge(&mut a, &mut b, "the dialog's OK");
    for (s, sid) in [(&a, id), (&b, on_b)] {
        let sh = s.active().unwrap().shape(sid).cloned().unwrap();
        assert!((sh.xfrm.unwrap().w - 200.0).abs() < 0.01, "the new width");
        assert!((sh.xfrm.unwrap().x - 100.0).abs() < 0.01, "the box otherwise as shown");
        assert_eq!(sh.fill, filled, "the laptop's fill survived");
    }
}

/// A picture the pc added whose bytes it can't send (its agent's blob store refuses them): the laptop's badge says it
/// waits for media from the pc, and the pc counts it as not sent (closing waits a little for it).
#[test]
fn media_not_sent_yet_names_the_machine_it_waits_for() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, pc, _l) = pair(&hub, "waits.deckcraft");
    assert_eq!(b.live_media_waiting(), None);
    pc.too_old();
    let png = {
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([9, 9, 200]));
        let mut out = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    a.execute("slide.go", &json!({"index": 0})).unwrap();
    a.execute("insert.picture", &json!({"data": cmd::base64_encode(&png), "name": "w.png"})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while b.live_media_waiting().is_none() {
        a.poll_live();
        b.poll_live();
        assert!(Instant::now() < deadline, "the laptop never heard of the picture");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(b.live_media_waiting().as_deref(), Some("waiting for media from pc"));
    assert_eq!(a.live_unsent_media(), 1, "the pc hasn't sent it");
    assert_eq!(b.live_unsent_media(), 0);
}

/// Review focus 4: a slide deleted on one machine while the other types in a shape on it: gone on both, the typist's
/// selection let go of it, nothing dangles.
#[test]
fn a_slide_deleted_while_typed_on_is_gone_on_both() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "focus4.deckcraft");
    a.execute("slide.delete", &json!({"index": 1})).unwrap();
    let t = named(&b, "Text").unwrap();
    b.execute("slide.go", &json!({"index": 1})).unwrap();
    b.execute("text.edit", &json!({"id": t.0, "at": [0, 0]})).unwrap();
    for c in ["q", "r", "s"] {
        b.execute("text.insert", &json!({"text": c})).unwrap();
    }
    converge(&mut a, &mut b, "the deleted slide");
    for s in [&a, &b] {
        assert_eq!(deck(s).slides.len(), 1);
        assert!(named(s, "Text").is_none(), "no ghost shape");
    }
    let st = b.active().unwrap();
    assert!(st.selection.text.is_none() && st.selection.slide == 0);
    // Editing goes on.
    b.execute("shape.insert", &json!({"preset": "rect", "rect": [0, 0, 10, 10]})).unwrap();
    converge(&mut a, &mut b, "after");
}

/// Review focus 5: a picture inserted on one machine shows on the other (its bytes through the blob store), also
/// when the inserting machine's agent can't reach the server at first (the upload goes later); a 50 MB video
/// freezes neither window (each poll and each command returns at once while it uploads).
#[test]
fn media_through_the_blob_store_without_freezing() {
    let hub = Hub::new();
    let (mut a, mut b, _da, db, pc, _l) = pair(&hub, "focus5.deckcraft");
    let png = {
        let img = image::RgbImage::from_pixel(5, 5, image::Rgb([1, 200, 3]));
        let mut out = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    pc.set_online(false);
    a.execute("slide.go", &json!({"index": 0})).unwrap();
    let v = a.execute("insert.picture", &json!({"data": cmd::base64_encode(&png), "name": "p.png"})).unwrap();
    let pic = ShapeId(u32::try_from(v["id"].as_u64().unwrap()).unwrap());
    a.execute("shape.rename", &json!({"id": pic.0, "name": "Offline picture"})).unwrap();
    for _ in 0..20 {
        a.poll_live();
        b.poll_live();
        std::thread::sleep(Duration::from_millis(5));
    }
    pc.set_online(true);
    converge(&mut a, &mut b, "the picture");
    let pb = deck(&b);
    let id = named(&b, "Offline picture").unwrap();
    let mut media = None;
    deckcraft_model::walk(&pb.slides[0].shapes, &mut |sh, _| {
        if sh.id == id
            && let deckcraft_model::ShapeKind::Picture { fill } = &sh.kind
        {
            media = Some(fill.media);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while deck(&b).media(media.unwrap()).is_none_or(|m| m.data.as_slice() != png.as_slice()) {
        b.poll_live();
        a.poll_live();
        assert!(Instant::now() < deadline, "the picture's bytes never came");
        std::thread::sleep(Duration::from_millis(5));
    }
    // 50 MB of video, added outside a command (it reaches the kit at the next poll).
    let big = vec![0x5au8; 50 * 1024 * 1024];
    a.edit(|doc, _| {
        doc.add_media("big.mp4", "video/mp4", big.clone());
        Ok(())
    })
    .unwrap();
    let mut worst = Duration::ZERO;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let t = Instant::now();
        a.poll_live();
        b.poll_live();
        a.execute("slide.go", &json!({"index": 0})).unwrap();
        worst = worst.max(t.elapsed());
        if deck(&b).media.iter().any(|m| m.data.len() == big.len()) {
            break;
        }
        assert!(Instant::now() < deadline, "the video never came");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(worst < Duration::from_millis(1500), "a poll took {worst:?} while 50 MB moved");
    drop(db);
}

/// Only the pc types (20 keystrokes) and saves after each: the laptop gets every keystroke and never saves (its file is
/// never behind the live document, and nothing changed there). Then the pc types 20 more without saving: the laptop
/// catches its file up at most once per `CATCH_UP_EVERY`, never once per keystroke.
#[test]
fn only_the_typing_machine_saves() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, pc, laptop) = pair(&hub, "onesave.deckcraft");
    let t = named(&a, "Target").unwrap();
    a.execute("slide.go", &json!({"index": 1})).unwrap();
    a.execute("text.edit", &json!({"id": t.0, "at": [0, 0]})).unwrap();
    let before = laptop.saves().len();
    for _ in 0..20 {
        a.execute("text.insert", &json!({"text": "k"})).unwrap();
        a.execute("file.save", &json!({})).unwrap();
        for _ in 0..5 {
            a.poll_live();
            b.poll_live();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    converge(&mut a, &mut b, "the keystrokes");
    for _ in 0..50 {
        a.poll_live();
        b.poll_live();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(laptop.saves().len(), before, "the laptop never saved");
    assert!(!b.active().unwrap().is_dirty(), "another machine's typing isn't unsaved here");
    // Without the pc's saves, for 12 s of steady typing (a keystroke every 100 ms, so over two CATCH_UP_EVERY
    // windows): the laptop's file falls behind, and it catches up at most once per window; the typing machine never
    // saves while it types (it waits for 5 s of quiet), and once it stops, it saves.
    let start = Instant::now();
    let before = laptop.saves().len();
    let typist = pc.saves().len();
    while start.elapsed() < Duration::from_secs(12) {
        a.execute("text.insert", &json!({"text": "m"})).unwrap();
        for _ in 0..10 {
            a.poll_live();
            b.poll_live();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let windows = start.elapsed().as_secs() / craft_cloud_kit::CATCH_UP_EVERY.as_secs() + 1;
    let saves = (laptop.saves().len() - before) as u64;
    assert!(start.elapsed() > 2 * craft_cloud_kit::CATCH_UP_EVERY, "the check ran over more than one window");
    assert!((1..=windows).contains(&saves), "the laptop saved {saves} times in {:?} ({windows} windows)", start.elapsed());
    assert_eq!(pc.saves().len(), typist, "the typing machine waits for 5 s of quiet");
    let quiet = Instant::now();
    while pc.saves().len() == typist {
        a.poll_live();
        b.poll_live();
        assert!(quiet.elapsed() < AUTOSAVE_AFTER + Duration::from_secs(5), "the typing machine never saved after it stopped");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(quiet.elapsed() >= AUTOSAVE_AFTER - Duration::from_millis(500), "it saved only after its quiet");
}

/// Both machines edit and autosave: no conflict copy is ever made (the hub's saves carry records), each file ends
/// with every edit, and the saving stops.
#[test]
fn both_autosave_and_the_files_catch_up() {
    let hub = Hub::new();
    let (mut a, mut b, da, db, _pc, _l) = pair(&hub, "save.deckcraft");
    for k in 0..3 {
        insert(&mut a, 10.0 * k as f64);
        insert(&mut b, 300.0 + 10.0 * k as f64);
    }
    converge(&mut a, &mut b, "edits");
    // Saved by hand on the pc, at once; the laptop's autosave comes after its 5 s.
    a.execute("file.save", &json!({})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        a.poll_live();
        b.poll_live();
        let (fa, fb) = (std::fs::read(da.0.join("save.deckcraft")).unwrap(), std::fs::read(db.0.join("save.deckcraft")).unwrap());
        let (pa, pb) = (deckcraft_format::load(&fa).unwrap(), deckcraft_format::load(&fb).unwrap());
        let shapes = |p: &Presentation| p.slides.get(1).map_or(0, |s| s.shapes.len());
        if shapes(&pa) == shapes(&deck(&a)) && shapes(&pb) == shapes(&deck(&b)) && !a.active().unwrap().is_dirty() && !b.active().unwrap().is_dirty()
        {
            break;
        }
        assert!(Instant::now() < deadline, "the files never caught up");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!_pc.saves().is_empty() && !_l.saves().is_empty(), "both machines' saves carried records");
    // Nothing more to save.
    let saved = std::fs::metadata(db.0.join("save.deckcraft")).unwrap().modified().unwrap();
    let until = Instant::now() + Duration::from_secs(7);
    while Instant::now() < until {
        a.poll_live();
        b.poll_live();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::metadata(db.0.join("save.deckcraft")).unwrap().modified().unwrap(), saved, "no save loop");
}

/// Media over Craft Cloud's 64 MB: a presentation holding some opens as a plain file (saying why), and adding some to
/// a live one is taken back (saying why), so no machine is left unable to save. A machine reopening while its agent is
/// offline has its pictures from its own file, and can save.
#[test]
fn media_over_the_limit_never_goes_live_and_reopening_offline_keeps_pictures() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let mut big = corpus();
    big.add_media("huge.mp4", "video/mp4", vec![1u8; super::session::MAX_MEDIA_BYTES + 1]);
    let bytes = deckcraft_format::save(&big).unwrap();
    let opened = open_at(&agent(&pc), "huge.deckcraft", &big, &bytes, "pc".into());
    assert!(matches!(&opened, Some(Err(why)) if why.contains("huge.mp4")), "not live, and why");
    // Added to a live one: taken back.
    let (mut a, mut b, _da, db, _pc, laptop) = pair(&hub, "limit.deckcraft");
    let before = deck(&a);
    a.edit(|doc, _| {
        doc.add_media("huge.mp4", "video/mp4", vec![1u8; super::session::MAX_MEDIA_BYTES + 1]);
        Ok(())
    })
    .unwrap();
    a.poll_live();
    assert!(Arc::ptr_eq(&deck(&a), &before), "the change was taken back");
    assert!(a.ui_requests.iter().any(|r| matches!(r, crate::UiRequest::Message { text } if text.contains("64 MB"))));
    // The laptop reopens with its agent offline: its pictures come from its own file.
    a.execute("file.save", &json!({})).unwrap();
    converge(&mut a, &mut b, "in step");
    let saved = std::fs::read(db.0.join("limit.deckcraft")).unwrap_or_default();
    drop(b);
    laptop.set_online(false);
    let file = if saved.is_empty() { deckcraft_format::save(&deck(&a)).unwrap() } else { saved };
    let mut again = engine_join(&laptop, &db, "limit.deckcraft", &file, "laptop");
    let deadline = Instant::now() + Duration::from_secs(10);
    while deck(&again).media.iter().any(|m| m.data.is_empty()) {
        again.poll_live();
        assert!(Instant::now() < deadline, "the pictures never came from the file");
        std::thread::sleep(Duration::from_millis(10));
    }
    again.execute("file.save", &json!({})).unwrap();
}

/// A fresh table's rows are all alike: a row inserted above row 1 on the pc while the laptop types in row 1 keeps
/// the laptop's text in that row (now row 2) on both machines, not in the new one. And a row deleted on one machine
/// while the other types in another row keeps the typing where it was.
#[test]
fn rows_inserted_in_a_table_of_equal_rows_keep_their_identity() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "rows.deckcraft");
    a.execute("slide.go", &json!({"index": 0})).unwrap();
    let v = a.execute("insert.table", &json!({"rows": 3, "cols": 2})).unwrap();
    let ta = ShapeId(u32::try_from(v["id"].as_u64().unwrap()).unwrap());
    a.execute("shape.rename", &json!({"id": ta.0, "name": "Grid"})).unwrap();
    converge(&mut a, &mut b, "the table");
    let tb = named(&b, "Grid").unwrap();
    b.execute("slide.go", &json!({"index": 0})).unwrap();
    b.execute("text.edit", &json!({"id": tb.0, "cell": [1, 0], "at": [0, 0]})).unwrap();
    b.execute("text.insert", &json!({"text": "B"})).unwrap();
    a.execute("table.insertRowAbove", &json!({"id": ta.0, "row": 1})).unwrap();
    converge(&mut a, &mut b, "a row and typing at once");
    let cells = |s: &Session, id: ShapeId| -> Vec<String> {
        let sh = s.active().unwrap().shape(id).cloned().unwrap();
        let deckcraft_model::ShapeKind::Table(t) = sh.kind else { panic!("a table") };
        t.rows.iter().map(|r| r.cells[0].text.text()).collect()
    };
    for (s, id) in [(&a, ta), (&b, tb)] {
        assert_eq!(cells(s, id), vec!["", "", "B", ""], "the typing stays in its row");
    }
    // A row deleted while the other types in a later one.
    b.execute("text.edit", &json!({"id": tb.0, "cell": [3, 0], "at": [0, 0]})).unwrap();
    b.execute("text.insert", &json!({"text": "Z"})).unwrap();
    a.execute("table.deleteRow", &json!({"id": ta.0, "row": 0})).unwrap();
    converge(&mut a, &mut b, "a deleted row and typing at once");
    for (s, id) in [(&a, ta), (&b, tb)] {
        assert_eq!(cells(s, id), vec!["", "B", "Z"]);
    }
    // A table command refused (the last column can't go) leaves no note for another command's change to pick up.
    let one = a.execute("insert.table", &json!({"rows": 2, "cols": 1})).unwrap();
    let one = one["id"].as_u64().unwrap();
    assert!(a.execute("table.deleteColumn", &json!({"id": one, "col": 0})).is_err());
    assert!(!a.table_note_waits(), "nothing noted for a command that did nothing");
}

/// Edits the kit can't take (here: one element in the tree twice, a shape copied to another slide with its id) stay
/// on screen: Undo is refused rather than showing the live document without them, and a save writes them (with no
/// record) rather than the live document without them.
#[test]
fn unsent_edits_are_never_thrown_away() {
    let hub = Hub::new();
    let (mut a, mut b, da, _db, _pc, _l) = pair(&hub, "unsent.deckcraft");
    insert(&mut a, 10.0);
    converge(&mut a, &mut b, "a first edit");
    a.edit(|doc, _| {
        let copy = doc.slides[1].shapes[0].clone();
        Arc::make_mut(&mut doc.slides[0]).shapes.push(deckcraft_model::Shape { name: "twice".into(), ..copy });
        Ok(())
    })
    .unwrap();
    a.poll_live();
    assert!(!Arc::ptr_eq(&deck(&a), &a.live_kit().unwrap().model().pres), "the edit didn't go out");
    assert!(a.execute("edit.undo", &json!({})).is_err(), "Undo waits");
    assert!(deck(&a).slides[0].shapes.iter().any(|s| s.name == "twice"), "the edit is still on screen");
    a.execute("file.save", &json!({})).unwrap();
    let file = da.0.join("unsent.deckcraft");
    let saved = deckcraft_format::load(&std::fs::read(&file).unwrap()).unwrap();
    assert!(saved.slides[0].shapes.iter().any(|s| s.name == "twice"), "the save has the edit");
    // Saved: the edits stay unsent, but the same bytes aren't written again every AUTOSAVE_AFTER (each write is an
    // edit outside Craft to the agent).
    let written = std::fs::metadata(&file).unwrap().modified().unwrap();
    let until = Instant::now() + AUTOSAVE_AFTER + Duration::from_secs(2);
    while Instant::now() < until {
        a.poll_live();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), written, "not written again");
}

/// Revert on a live document brings back the file as it is on disk now (its last save), not the content from when the
/// window was opened.
#[test]
fn revert_brings_back_the_last_save() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "revert.deckcraft");
    insert(&mut a, 11.0);
    let saved = a.active().unwrap().doc.slides[1].shapes.len();
    a.execute("file.save", &json!({})).unwrap();
    insert(&mut a, 22.0);
    converge(&mut a, &mut b, "edits");
    a.execute("file.revert", &json!({})).unwrap();
    converge(&mut a, &mut b, "the revert");
    assert_eq!(deck(&a).slides[1].shapes.len(), saved, "the last save, with the first insert, without the second");
    assert_eq!(deck(&b).slides[1].shapes.len(), saved);
}

/// Save As to another file: the window is that file from then on. Its later edits don't go live, the other machine's
/// don't arrive, and the live file isn't written from it.
#[test]
fn save_as_leaves_the_live_document() {
    let hub = Hub::new();
    let (mut a, mut b, da, _db, _pc, _l) = pair(&hub, "saveas.deckcraft");
    let copy = da.0.join("variant.deckcraft").to_string_lossy().into_owned();
    a.execute("file.saveAs", &json!({"path": copy})).unwrap();
    assert!(!a.is_live());
    let live_file = std::fs::read(da.0.join("saveas.deckcraft")).unwrap();
    insert(&mut a, 33.0);
    a.execute("shape.rename", &json!({"name": "only in the variant"})).unwrap();
    let theirs = insert(&mut b, 44.0);
    b.execute("shape.rename", &json!({"id": theirs.0, "name": "only live"})).unwrap();
    for _ in 0..50 {
        a.poll_live();
        b.poll_live();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(named(&b, "only in the variant").is_none(), "the variant's edit didn't go live");
    assert!(named(&a, "only live").is_none(), "the live edit didn't reach the variant");
    a.execute("file.save", &json!({})).unwrap();
    assert_eq!(std::fs::read(da.0.join("saveas.deckcraft")).unwrap(), live_file, "the live file wasn't written from the variant");
}

/// The guard's roll-back sends nothing: a command that panics leaves the live document as it was.
#[test]
fn a_command_the_guard_rolls_back_sends_nothing() {
    let hub = Hub::new();
    let (mut a, mut b, _da, _db, _pc, _l) = pair(&hub, "guard.deckcraft");
    let ops = |s: &mut Session| s.live_kit().unwrap().snapshot().unwrap().len();
    let before = ops(&mut a);
    // As inside a command: the shape goes in as a nested command, then the command panics.
    let r = a.guarded("test.panic", |s| {
        s.depth += 1;
        s.execute("shape.insert", &json!({"preset": "rect", "rect": [1, 1, 5, 5]}))?;
        panic!("boom");
    });
    assert!(r.is_err());
    // The inner command synced nothing (it isn't the outermost), and the roll-back is the kit's model again.
    a.poll_live();
    assert!(Arc::ptr_eq(&deck(&a), &a.live_kit().unwrap().model().pres));
    assert_eq!(ops(&mut a), before);
    converge(&mut a, &mut b, "nothing happened");
}

/// A file changed outside Craft shows the banner; "Bring it in" replaces the live content everywhere as one step that
/// Undo takes back; Revert does the same.
#[test]
fn bringing_the_file_in_and_revert_are_one_undoable_change() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let (da, db) = (Folder::new("pc"), Folder::new("laptop"));
    let mut a = engine_open(&pc, &da, "outside.deckcraft", &corpus(), "pc");
    a.poll_live();
    drop(a);
    // Edited outside Craft (no Craft save has these bytes), then opened on the laptop.
    let mut edited = corpus();
    edited.props.title = "Edited outside".into();
    let mut b = engine_open(&laptop, &db, "outside.deckcraft", &edited, "laptop");
    assert_eq!(b.live_banner(), Some(Banner::ChangedOutside));
    assert_ne!(deck(&b).props.title, "Edited outside", "the live content is shown until the user chooses");
    b.execute("file.revert", &json!({})).unwrap();
    assert_eq!(deck(&b).props.title, "Edited outside");
    assert_eq!(b.live_banner(), None);
    assert_eq!(b.live_step_name(false).as_deref(), Some(craft_cloud_kit::REPLACE_WITH_FILE));
    b.execute("edit.undo", &json!({})).unwrap();
    assert_ne!(deck(&b).props.title, "Edited outside");
}

/// "Keep the live document" is the command `file.liveKeep`: enabled only while the banner is up; it takes the banner
/// down, and the next autosave writes the live content over the outside edit.
#[test]
fn keeping_the_live_document_is_a_command() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let (da, db) = (Folder::new("pc"), Folder::new("laptop"));
    let mut a = engine_open(&pc, &da, "keep.deckcraft", &corpus(), "pc");
    assert!(a.execute("file.liveKeep", &json!({})).is_err(), "disabled without the banner");
    // The pc's media uploads finish before it closes.
    let deadline = Instant::now() + Duration::from_secs(10);
    while a.live_kit().unwrap().model().chunks.len() < corpus().media.len() {
        a.poll_live();
        assert!(Instant::now() < deadline, "the uploads never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(a);
    let mut edited = corpus();
    edited.props.title = "Edited outside".into();
    let mut b = engine_open(&laptop, &db, "keep.deckcraft", &edited, "laptop");
    assert_eq!(b.live_banner(), Some(Banner::ChangedOutside));
    b.execute("file.liveKeep", &json!({})).unwrap();
    assert_eq!(b.live_banner(), None);
    assert!(b.journal.iter().any(|(id, _)| id == "file.liveKeep"), "journaled like any command");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        b.poll_live();
        let on_disk = deckcraft_format::load(&std::fs::read(db.0.join("keep.deckcraft")).unwrap()).unwrap();
        if on_disk.props.title != "Edited outside" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the live content was never written: {:?}; empty media {}; dirty {}",
            b.live_status(),
            deck(&b).media.iter().filter(|m| m.data.is_empty()).count(),
            b.active().unwrap().is_dirty()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// AutoRecover of a live presentation reopens it as an untitled copy: saving it can't write an older snapshot over
/// the live file.
#[test]
fn a_recovered_live_presentation_has_no_path() {
    let hub = Hub::new();
    let (mut a, _b, _da, _db, _pc, _l) = pair(&hub, "recover.deckcraft");
    insert(&mut a, 5.0);
    assert!(a.active().unwrap().is_dirty());
    let dir = Folder::new("recovery");
    crate::recovery::save(&a, &dir.0).unwrap();
    let mut s = Session::new();
    let opened = crate::recovery::open(&mut s, &dir.0).unwrap();
    assert_eq!(opened.len(), 1);
    assert_eq!(s.active().unwrap().path, None, "no path: Save asks where");
}

/// A document a newer DeckCraft made opens read-only: the banner says to update, and an edit is taken back.
#[test]
fn a_newer_documents_opens_read_only() {
    use craft_cloud_kit::{KitError, LiveDocument, Mappings, Opaque, RemotePatch};
    struct Newer;
    impl LiveDocument for Newer {
        type Doc = Vec<u8>;
        const APP: &'static str = "deckcraft";
        const SCHEMA: u32 = 2;
        fn mappings(d: &Vec<u8>) -> Mappings<'_> {
            Mappings::new().opaque("x", Opaque { value: Some(d), key: None })
        }
        fn apply_remote(old: &Arc<Vec<u8>>, _p: &RemotePatch<'_>) -> Result<Arc<Vec<u8>>, KitError> {
            Ok(old.clone())
        }
        fn normalise(_d: &mut Vec<u8>) {}
        fn refresh_derived(_d: &mut Vec<u8>) {}
    }
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let newer = craft_cloud_kit::Session::<Newer>::open(&agent(&pc), "newer.deckcraft", Arc::new(vec![1]), None).unwrap();
    drop(newer);
    let dir = Folder::new("pc");
    let mut s = engine_open(&pc, &dir, "newer.deckcraft", &corpus(), "pc");
    assert!(matches!(s.live_banner(), Some(Banner::ReadOnly(_))));
    let before = deck(&s);
    s.edit(|doc, _| {
        doc.props.title = "edited".into();
        Ok(())
    })
    .unwrap();
    s.poll_live();
    assert!(Arc::ptr_eq(&deck(&s), &before), "the edit was taken back");
    // Nothing writes the newer document's file: not the autosave, not Ctrl+S.
    let file = dir.0.join("newer.deckcraft");
    let bytes = std::fs::read(&file).unwrap();
    for _ in 0..30 {
        s.poll_live();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.execute("file.save", &json!({})).is_err(), "Ctrl+S is refused");
    s.poll_live();
    assert_eq!(std::fs::read(&file).unwrap(), bytes, "the file is as it was");
    let _: Value = json!({});
}
