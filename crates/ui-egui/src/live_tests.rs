//! The live window on the kit's fake agent: the badge in the status bar, the "changed outside Craft" banner, Craft
//! Cloud in Backstage, and another machine's selection outlined on the slide. `DECKCRAFT_SHOTS=<dir>` saves what the
//! window looked like.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deckcraft_engine::live::craft_cloud_kit::Agent;
use deckcraft_engine::live::craft_cloud_kit::testing::{Fake, Hub};
use deckcraft_engine::live::session::{Banner, open_at};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use crate::{Services, Session, SlideApp};

fn agent(f: &Fake) -> Agent {
    let c = f.client();
    Agent::from_fn(move || Ok(c.clone()))
}

struct Folder(PathBuf);

impl Folder {
    fn new(name: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("deckcraft-live-ui-{}-{name}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn deck() -> deckcraft_model::Presentation {
    let mut s = Session::new();
    deckcraft_engine::sample::open_sample(&mut s).unwrap();
    (*s.doc().unwrap().doc).clone()
}

/// Opens `rel` live in `session` on `f`'s machine, its file in `dir` being `bytes`.
fn open_live(session: &mut Session, f: &Fake, dir: &Path, rel: &str, bytes: &[u8], machine: &str) {
    let abs = dir.join(rel);
    std::fs::write(&abs, bytes).unwrap();
    let doc = deckcraft_engine::cmd::file::open_presentation(rel, bytes).unwrap();
    let open = open_at(&agent(f), rel, &doc, bytes, machine.into()).unwrap().unwrap();
    session.enable_live(agent(f), None);
    let path = abs.to_string_lossy().into_owned();
    session.stage_live(&path, open, Some(doc));
    session.execute("file.adopt", &json!({"path": path})).unwrap();
    assert!(session.is_live());
}

fn harness(app: SlideApp) -> Harness<'static, SlideApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 900.0)).with_step_dt(1.0 / 30.0).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h
}

/// Steps the window (and `other`, polled) until `done` (20 s at most).
fn until(h: &mut Harness<'static, SlideApp>, mut other: Option<&mut Session>, what: &str, done: impl Fn(&Harness<'static, SlideApp>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(h) {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        if let Some(o) = other.as_deref_mut() {
            o.poll_live();
        }
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Saves what the window looks like (with `DECKCRAFT_SHOTS`); the pixels' RGB, when it could be drawn.
fn shot(h: &mut Harness<'static, SlideApp>, name: &str) -> Option<Vec<[u8; 3]>> {
    let img = h.render().ok()?;
    if let Some(dir) = std::env::var_os("DECKCRAFT_SHOTS") {
        let _ = img.save(Path::new(&dir).join(format!("{name}.png")));
    }
    Some(img.pixels().map(|p| [p.0[0], p.0[1], p.0[2]]).collect())
}

#[test]
fn the_status_bar_shows_the_live_badge() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let dir = Folder::new("pc");
    let mut session = Session::new();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&deck(), "deckcraft").unwrap();
    open_live(&mut session, &pc, &dir.0, "badge.deckcraft", &bytes, "pc");
    let mut h = harness(SlideApp::new(session, Services::default()));
    until(&mut h, None, "the Live badge", |h| h.query_by_label("Live").is_some());
    shot(&mut h, "live-badge");
    pc.set_online(false);
    until(&mut h, None, "the Offline badge", |h| h.query_by_label("Offline").is_some());
    shot(&mut h, "offline-badge");
    pc.set_online(true);
    until(&mut h, None, "Live again", |h| h.query_by_label("Live").is_some());
}

#[test]
fn the_banner_brings_the_file_in() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let (da, db) = (Folder::new("pc"), Folder::new("laptop"));
    let mut first = Session::new();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&deck(), "deckcraft").unwrap();
    open_live(&mut first, &pc, &da.0, "outside.deckcraft", &bytes, "pc");
    first.poll_live();
    drop(first);
    let mut edited = deck();
    edited.props.title = "Edited outside".into();
    let mut session = Session::new();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&edited, "deckcraft").unwrap();
    open_live(&mut session, &laptop, &db.0, "outside.deckcraft", &bytes, "laptop");
    assert_eq!(session.live_banner(), Some(Banner::ChangedOutside));
    let mut h = harness(SlideApp::new(session, Services::default()));
    shot(&mut h, "changed-outside-banner");
    h.get_by_label("Bring it in").click();
    h.run_steps(3);
    assert_eq!(h.state().session.active().unwrap().doc.props.title, "Edited outside");
    assert!(h.query_by_label("Bring it in").is_none(), "the banner went");
}

#[test]
fn backstage_shows_craft_cloud() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let mut h = harness(SlideApp::new(Session::new(), Services::default()));
    let ctx = h.ctx.clone();
    h.state_mut().enable_live_with(agent(&pc), &ctx);
    h.run_steps(3);
    until(&mut h, None, "Craft Cloud in Backstage", |h| h.query_all_by_label("Craft Cloud").next().is_some());
    shot(&mut h, "backstage-craft-cloud");
}

#[test]
fn another_machines_selection_is_outlined() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let (da, db) = (Folder::new("pc"), Folder::new("laptop"));
    let mut session = Session::new();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&deck(), "deckcraft").unwrap();
    open_live(&mut session, &pc, &da.0, "presence.deckcraft", &bytes, "pc");
    session.execute("file.save", &json!({})).unwrap();
    let saved = std::fs::read(da.0.join("presence.deckcraft")).unwrap();
    let mut other = Session::new();
    open_live(&mut other, &laptop, &db.0, "presence.deckcraft", &saved, "laptop");
    // The laptop selects the title on the first slide.
    let title = other.active().unwrap().doc.slides[0].shapes[0].id;
    other.execute("edit.select", &json!({"ids": [title.0]})).unwrap();
    other.poll_live();
    let mut h = harness(SlideApp::new(session, Services::default()));
    until(&mut h, Some(&mut other), "the laptop's selection", |h| h.state().session.live_presence().iter().any(|p| !p.shapes.is_empty()));
    h.run_steps(2);
    let p = h.state().session.live_presence();
    assert_eq!(p[0].machine, "laptop");
    let mine = h.state().session.active().unwrap().doc.slides[0].shapes[0].id;
    assert_eq!(p[0].shapes, vec![mine], "the laptop's shape, by the pc's own id");
    // The outline goes round the shape's box on screen, in the laptop's colour, with its name above it.
    let xf = crate::canvas::Xf { origin: egui::pos2(10.0, 20.0), scale: 0.5 };
    let marks = crate::live::presence_marks(h.state(), xf);
    assert_eq!(marks.len(), 1, "{marks:?}");
    let box_ = {
        let st = h.state().session.active().unwrap();
        deckcraft_engine::cmd::xfrm_of(&st.doc, &st.selection, st.shape(mine).unwrap())
    };
    assert_eq!(marks[0].colour, crate::live::peer_colour(p[0].slot));
    assert_eq!(marks[0].outlines.len(), 1);
    let o = marks[0].outlines[0];
    let at = |x: f64, y: f64| xf.to_screen(deckcraft_geom::Point::new(x, y));
    assert!((o[0] - at(box_.x, box_.y)).length() < 0.5, "top left: {o:?}");
    assert!((o[2] - at(box_.x + box_.w, box_.y + box_.h)).length() < 0.5, "bottom right: {o:?}");
    assert_eq!(marks[0].tag, Some((o[0], "laptop".to_owned())), "the name tag");
    shot(&mut h, "presence-outline");
}

/// Closing the window while a picture this computer added hasn't gone to Craft Cloud yet: it waits, saying so (and
/// the other computers' badge would say they wait for media from this one), and "Close now" closes it.
#[test]
fn closing_waits_a_little_for_media_not_sent_yet() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let dir = Folder::new("pc");
    let mut session = Session::new();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&deck(), "deckcraft").unwrap();
    open_live(&mut session, &pc, &dir.0, "close.deckcraft", &bytes, "pc");
    // Its agent can't take the bytes (a blob store from before blobs): they stay here.
    pc.too_old();
    const HEX: &str =
        "89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de0000000c49444154789c63f8cfc0000003010100c9fe92ef0000000049454e44ae426082";
    let png: Vec<u8> = (0..HEX.len()).step_by(2).map(|i| u8::from_str_radix(&HEX[i..i + 2], 16).unwrap()).collect();
    session.execute("insert.picture", &json!({"data": deckcraft_engine::cmd::base64_encode(&png), "name": "c.png"})).unwrap();
    // (Saved: no question about unsaved changes comes first.)
    session.execute("file.save", &json!({})).unwrap();
    assert_eq!(session.live_unsent_media(), 1);
    let mut h = harness(SlideApp::new(session, Services::default()));
    h.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    h.run_steps(3);
    assert!(matches!(h.state().closing, Some((_, false))), "waiting");
    assert!(h.query_by_label("Close now").is_some(), "it says so");
    shot(&mut h, "closing-waits-for-media");
    h.get_by_label("Close now").click();
    h.run_steps(2);
    assert!(matches!(h.state().closing, Some((_, true))), "closing now");
}

/// A `.deckcraft` file handed over by the system (double-click, command line) opens on the background thread when
/// live documents are on: the window doesn't wait for Craft Cloud.
#[test]
fn files_from_the_system_open_in_the_background() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let dir = Folder::new("pc");
    let bytes = deckcraft_engine::cmd::file::save_bytes(&deck(), "deckcraft").unwrap();
    let (one, two) = (dir.0.join("system.deckcraft"), dir.0.join("second.deckcraft"));
    std::fs::write(&one, &bytes).unwrap();
    std::fs::write(&two, &bytes).unwrap();
    let mut session = Session::new();
    session.enable_live(agent(&pc), None);
    let mut app = SlideApp::new(session, Services::default());
    let failed = app.open_from_system(&[one.to_string_lossy().into_owned(), two.to_string_lossy().into_owned()]);
    assert!(failed.is_empty());
    assert!(app.opening.is_some(), "opening on its thread");
    assert_eq!(app.open_queue.len(), 1, "the second waits for the first, never opened on the UI thread");
    assert!(app.session.documents().is_empty());
    let mut h = harness(app);
    until(&mut h, None, "both presentations", |h| h.state().session.documents().len() == 2);
}
