//! The live simulation: two DeckCraft sessions on the kit's fake agent, each running random commands from
//! DeckCraft's own command list (with plausible and hostile parameters, as the editor soak makes them), with undo
//! and redo, saves, dropped connections and time offline, and now and then a paste of tens of KB into a new text box
//! (in fragments: the simulation's frames are 16 KiB), with fragments on their way to the windows dropped or held back
//! and an agent sometimes stopping part of the way through taking one. No window ever holds part of a paste. Healed,
//! both presentations are the same, and they are presentations DeckCraft's validator accepts, with nothing pointing
//! at what a merge deleted.
//!
//! `LIVE_SIM_SEEDS=1000 LIVE_SIM_FIRST=0 cargo test --release -p deckcraft-engine --features live live_sim` runs
//! seeds 0 to 999; `LIVE_SIM_SEED=17` runs one. A seed's history is random but not replayed exactly: the fake agent's
//! threads run on the clock.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use craft_cloud_kit::testing::{FragmentFaults, Hub};
use serde_json::{Value, json};

use super::tests::{canon, corpus, diff, valid};
use super::tests_engine::{Folder, engine_join, engine_open};
use crate::{EngineError, Session, command_specs};

/// Commands that touch the file system, other windows or real time; the simulation drives the rest.
fn skipped(id: &str) -> bool {
    matches!(id, "file.close" | "file.open" | "file.adopt" | "file.saveAs" | "file.export" | "file.saveTemplate" | "file.openBytes" | "file.new")
        || id.starts_with("window.")
        || id.starts_with("show.")
        || id.starts_with("media.")
        || id.starts_with("file.recovery")
        || id.starts_with("app.")
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    fn f(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() >> 11) as f64 / (1u64 << 53) as f64 * (hi - lo)
    }
}

fn params(r: &mut Rng, s: &Session) -> Value {
    let ids: Vec<u64> = s.active().map(|d| d.shapes().iter().map(|x| u64::from(x.id.0)).collect()).unwrap_or_default();
    let id = ids.get(r.below(ids.len())).copied().unwrap_or(1);
    let slides = s.active().map(|d| d.doc.slides.len()).unwrap_or(1);
    let index = r.below(slides + 1);
    match r.below(11) {
        0 => json!({}),
        1 => json!({"ids": [id]}),
        2 => json!({"id": id, "text": "Live ✓ שלום 漢字", "name": "S"}),
        3 => json!({"index": index, "to": r.below(slides + 1)}),
        4 => json!({"preset": "rect", "rect": [r.f(0.0, 600.0), r.f(0.0, 400.0), r.f(1.0, 300.0), r.f(1.0, 200.0)]}),
        5 => json!({"ids": [id], "color": "#3366CC", "size": r.f(1.0, 96.0), "value": r.f(-10.0, 10.0)}),
        6 => json!({"ids": ids.iter().take(3).collect::<Vec<_>>()}),
        7 => json!({"effect": "fade", "start": "afterPrevious", "duration": r.below(5000), "delay": r.below(2000)}),
        8 => json!({"layout": "titleAndContent", "title": "T", "body": "B"}),
        9 => json!({"rows": 1 + r.below(3), "cols": 1 + r.below(3), "text": "c"}),
        _ => json!({"index": u64::MAX, "ids": [u64::MAX], "id": -1, "size": 1e308, "rect": [f64::MAX, 0, -1, 0]}),
    }
}

/// A paste of 30 to 100 KB, its text made of the seed's own characters (so its start and end are found nowhere
/// else): with frames of 16 KiB here it goes in fragments, as a paste of megabytes does for real.
fn paste_text(r: &mut Rng) -> String {
    let len = 30_000 + r.below(70_000);
    (0..len).map(|_| char::from(b'a' + u8::try_from(r.below(26)).unwrap_or(0))).collect()
}

/// The texts of every shape in the deck.
fn texts(s: &Session) -> Vec<String> {
    let mut out = Vec::new();
    for sl in &s.active().unwrap().doc.slides {
        deckcraft_model::walk(&sl.shapes, &mut |sh, _| {
            if let Some(t) = &sh.text {
                out.push(t.text());
            }
        });
    }
    out
}

fn run_seed(seed: u64, steps: usize) {
    // A set of fragments that stopped coming is dropped soon (the session catches up by opening the document again);
    // frames of 16 KiB, so pastes of tens of KB go in fragments.
    craft_cloud_app::live::set_fragment_patience_for_tests(Duration::from_millis(300));
    craft_cloud_app::live::set_frame_limit_for_tests(16 * 1024);
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    // Fragments on their way to the windows: some dropped, some held back a few steps (out of order).
    for (i, f) in [&pc, &laptop].into_iter().enumerate() {
        f.set_fragment_faults(Some(FragmentFaults { seed: seed.wrapping_mul(5).wrapping_add(i as u64), drop: 150, delay: 300, max_delay: 4 }));
    }
    // The pastes made: their first 64 characters and their length. A text that holds a paste's start holds it whole.
    let mut pastes: Vec<(String, usize)> = Vec::new();
    let (da, db) = (Folder::new("pc"), Folder::new("laptop"));
    let rel = format!("sim-{seed}.deckcraft");
    let mut a = engine_open(&pc, &da, &rel, &corpus(), "pc");
    a.execute("file.save", &json!({})).unwrap();
    let saved = std::fs::read(da.0.join(&rel)).unwrap();
    let mut b = engine_join(&laptop, &db, &rel, &saved, "laptop");
    let fakes = [&pc, &laptop];
    let ids: Vec<&'static str> = command_specs().iter().map(|c| c.id).filter(|id| !skipped(id)).collect();
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut log: VecDeque<String> = VecDeque::new();
    let mut offline = [false, false];
    for step in 0..steps {
        let who = r.below(2);
        let s = if who == 0 { &mut a } else { &mut b };
        match r.below(20) {
            0..=12 => {
                let id = ids[r.below(ids.len())];
                let p = params(&mut r, s);
                log.push_back(format!("{who}: {id} {p}"));
                if let Err(EngineError::Internal(..)) = s.execute(id, &p) {
                    panic!("seed {seed} step {step}: a command panicked: {id} {p}; {log:?}");
                }
            }
            13 | 14 => {
                log.push_back(format!("{who}: undo"));
                let _ = s.execute("edit.undo", &json!({}));
            }
            15 => {
                log.push_back(format!("{who}: redo"));
                let _ = s.execute("edit.redo", &json!({}));
            }
            16 => {
                log.push_back(format!("{who}: save"));
                let _ = s.execute("file.save", &json!({}));
            }
            17 => {
                offline[who] = !offline[who];
                log.push_back(format!("{who}: online {}", !offline[who]));
                fakes[who].set_online(!offline[who]);
            }
            18 => {
                log.push_back(format!("{who}: connections dropped"));
                fakes[who].drop_connections();
            }
            // A big paste into a new text box (sometimes with the agent stopping part of the way through taking it).
            _ if r.below(3) == 0 => {
                if r.below(4) == 0 {
                    fakes[who].drop_connections_after(r.below(3));
                }
                let n = s.active().unwrap().doc.slides.len();
                let _ = s.execute("slide.go", &json!({"index": r.below(n)}));
                if let Ok(v) = s.execute("shape.insert", &json!({"preset": "rect", "rect": [20, 20, 300, 200]}))
                    && let Some(id) = v["id"].as_u64()
                {
                    let text = paste_text(&mut r);
                    log.push_back(format!("{who}: paste of {} bytes into {id}", text.len()));
                    let _ = s.execute("text.edit", &json!({"id": id, "at": [0, 0]}));
                    if s.execute("text.insert", &json!({"text": text})).is_ok() {
                        pastes.push((text[..64].to_owned(), text.len()));
                    }
                }
            }
            _ => {}
        }
        if log.len() > 30 && std::env::var_os("LIVE_SIM_SEED").is_none() {
            log.pop_front();
        }
        // Each machine polls now and then (a window draws when it can).
        for s in [&mut a, &mut b] {
            if r.below(3) > 0 {
                s.poll_live();
            }
        }
        // DeckCraft's own validator after every step (the stricter checks wait for the end: a local command can leave
        // what only a merge repairs; `slide.duplicate` keeps a comment on the original's shape).
        for (n, s) in [("pc", &a), ("laptop", &b)] {
            let problems = s.active().unwrap().doc.validate();
            assert!(problems.is_empty(), "seed {seed} step {step}: the {n}'s deck: {problems:?}; {log:?}");
            // No paste is ever seen in part (a change comes whole or not at all).
            // (A part of one would lack whole slices or fragments, thousands of characters; the commands only ever take
            // a few characters from a text, or all of it.)
            for t in texts(s) {
                for (start, len) in &pastes {
                    assert!(
                        !t.contains(start.as_str()) || t.len() + 4096 >= *len,
                        "seed {seed} step {step}: the {n} holds part of a paste (a text of {} bytes, the paste {len}); {log:?}",
                        t.len()
                    );
                }
            }
        }
        hub.tick();
    }
    for f in fakes {
        f.set_fragment_faults(None);
        f.set_online(true);
    }
    for _ in 0..8 {
        hub.tick();
    }
    // Healed: both the same, both valid.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        a.poll_live();
        b.poll_live();
        let (ka, kb) = (a.live_kit().unwrap().model().clone(), b.live_kit().unwrap().model().clone());
        let in_step = std::sync::Arc::ptr_eq(&ka.pres, &a.active().unwrap().doc) && std::sync::Arc::ptr_eq(&kb.pres, &b.active().unwrap().doc);
        let d = diff(&canon(&ka), &canon(&kb), "");
        if in_step && d.is_none() {
            break;
        }
        if Instant::now() >= deadline {
            // Which model isn't its own live document?
            let own = |s: &mut Session| {
                let kit = s.live_kit().unwrap();
                let model = kit.model().clone();
                let mut r = craft_cloud_kit::raw::Replica::default();
                r.import(&[kit.snapshot().unwrap()], &["slides", "deck"]).unwrap();
                let base = std::sync::Arc::new(super::LiveDeck::new(std::sync::Arc::new(super::tests::nothing()), model.shared.clone()));
                let back = r.read::<super::Deck>(&base).unwrap();
                diff(&canon(&model), &canon(&back), "")
            };
            panic!(
                "seed {seed}: never the same: {d:?}; in step: {in_step}; the pc against its document: {:?} ({:?}); the laptop: {:?} ({:?}); {log:?}",
                own(&mut a),
                a.live_status().and_then(|s| s.error),
                own(&mut b),
                b.live_status().and_then(|s| s.error)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    for s in [&a, &b] {
        let p = &s.active().unwrap().doc;
        let problems = valid(p);
        assert!(problems.is_empty(), "seed {seed}: after healing: {problems:?}; {log:?}");
        for sl in &p.slides {
            assert!(p.layout(sl.layout).is_some(), "seed {seed}: a slide on a missing layout");
        }
    }
}

#[test]
fn live_sim() {
    let one = std::env::var("LIVE_SIM_SEED").ok().and_then(|v| v.parse::<u64>().ok());
    let first: u64 = std::env::var("LIVE_SIM_FIRST").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let count: u64 = std::env::var("LIVE_SIM_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let steps: usize = std::env::var("LIVE_SIM_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let seeds: Vec<u64> = match one {
        Some(s) => vec![s],
        None => (first..first + count).collect(),
    };
    let jobs: usize = std::env::var("LIVE_SIM_JOBS").ok().and_then(|v| v.parse().ok()).unwrap_or(4).max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(seed) = seeds.get(i) else { break };
                    run_seed(*seed, steps);
                }
            });
        }
    });
}
