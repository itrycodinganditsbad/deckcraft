//! Speed on a big deck (release only: `cargo test --release -p deckcraft-engine --features live live_speed --
//! --nocapture`): the sample deck made 200 slides, a keystroke in a text box written into the live document (the
//! adapter's wrap and the kit's `local_change`, as the hook makes them), the same keystroke applied on the other
//! machine (its poll), and opening.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_kit::testing::Hub;
use craft_cloud_kit::{Open, Session};

use super::tests::{agent, nothing, shared};
use super::{Deck, LiveDeck};

fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v.get(v.len() / 2).copied().unwrap_or_default()
}

/// The sample deck, its slides copied until there are 200.
fn big_deck() -> deckcraft_model::Presentation {
    let mut s = crate::Session::new();
    crate::sample::open_sample(&mut s).unwrap();
    while s.doc().unwrap().doc.slides.len() < 200 {
        let n = s.doc().unwrap().doc.slides.len();
        s.execute("slide.duplicate", &serde_json::json!({"index": n % 8})).unwrap();
    }
    (*s.doc().unwrap().doc).clone()
}

/// The laptop's id of the pc's slide `id` (each machine numbers its own).
fn slide_of(b: &Session<Deck>, a: &Session<Deck>, id: deckcraft_model::SlideId) -> deckcraft_model::SlideId {
    let e = a.model().ids().elem(id.0).unwrap();
    deckcraft_model::SlideId(b.model().ids().local(e).unwrap())
}

fn shape_of(b: &Session<Deck>, a: &Session<Deck>, id: deckcraft_model::ShapeId) -> deckcraft_model::ShapeId {
    let e = a.model().ids().elem(id.0).unwrap();
    deckcraft_model::ShapeId(b.model().ids().local(e).unwrap())
}

#[test]
#[cfg_attr(debug_assertions, ignore = "speed is measured in release builds")]
fn live_speed() {
    let deck = big_deck();
    let shapes: usize = deck.slides.iter().map(|s| s.shapes.len()).sum();
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let t = Instant::now();
    let model = Arc::new(LiveDeck::new(Arc::new(deck), shared(&pc)));
    let Open::Live(a) = Session::<Deck>::open(&agent(&pc), "big.deckcraft", model, None).unwrap() else { panic!("not live") };
    let mut a = *a;
    let import = t.elapsed();
    let t = Instant::now();
    let base = Arc::new(LiveDeck::new(Arc::new(nothing()), shared(&laptop)));
    let Open::Live(b) = Session::<Deck>::open(&agent(&laptop), "big.deckcraft", base, None).unwrap() else { panic!("not live") };
    let mut b = *b;
    let open = t.elapsed();
    assert_eq!(b.model().pres.slides.len(), 200);
    // The text box: the subtitle on slide 100.
    let (slide, shape) = {
        let p = &a.model().pres;
        let s = &p.slides[100];
        (s.id, s.shapes.iter().find(|x| x.text.is_some()).map(|x| x.id).unwrap())
    };
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for k in 0..200 {
        let old = a.model().clone();
        let mut p = (*old.pres).clone();
        let i = p.slide_index(slide).unwrap();
        let sl = Arc::make_mut(&mut p.slides[i]);
        let body = sl.shape_mut(shape).unwrap().text.as_mut().unwrap();
        deckcraft_model::edit::insert(body, (0, k), "x", None);
        let t = Instant::now();
        let new = Arc::new(old.wrap(Arc::new(p)));
        a.local_change(&old, &new).unwrap();
        local.push(t.elapsed());
        a.poll();
        // The laptop's poll that applies it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = Instant::now();
            let polled = b.poll();
            if polled.changed {
                remote.push(t.elapsed());
                break;
            }
            assert!(Instant::now() < deadline, "the keystroke never came");
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    let want = a.model().pres.slide(slide).unwrap().shape(shape).unwrap().text.as_ref().unwrap().text();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        b.poll();
        let typed = b
            .model()
            .pres
            .slide(slide_of(&b, &a, slide))
            .and_then(|s| s.shape(shape_of(&b, &a, shape)))
            .and_then(|s| s.text.as_ref())
            .map(|t| t.text());
        if typed.as_deref() == Some(want.as_str()) {
            break;
        }
        assert!(Instant::now() < deadline, "every keystroke came: {typed:?} vs {want:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    let (lw, rw) = (local.iter().max().copied().unwrap_or_default(), remote.iter().max().copied().unwrap_or_default());
    let (lm, rm) = (median(&mut local), median(&mut remote));
    eprintln!(
        "live speed: 200 slides, {shapes} shapes; import {import:?}, open on the other machine {open:?}; keystroke local_change median {lm:?} (worst {lw:?}); remote keystroke applied median {rm:?} (worst {rw:?})"
    );
    assert!(lm < Duration::from_millis(2), "local keystroke median {lm:?}");
    assert!(rm < Duration::from_millis(5), "remote keystroke median {rm:?}");
    assert!(open < Duration::from_secs(1), "open {open:?}");
}
