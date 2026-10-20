//! The engine's side of a live document: the kit's session for it and the media steps every poll makes.

use std::sync::Arc;

use craft_cloud_kit::{KitError, Session};

use super::{Deck, LiveDeck};

/// Puts the bytes of media items the live document has no chunk reference for yet into the blob store (on the worker;
/// a failed upload is tried again at the next call).
pub fn push_media(s: &Session<Deck>) {
    let d = s.model();
    for m in &d.pres.media {
        if m.data.is_empty() {
            continue;
        }
        let Some(e) = d.ids.elem(m.id.0) else { continue };
        if !d.chunks.contains_key(&e) {
            d.shared.upload(e, m.data.clone());
        }
    }
}

/// Takes what the media worker finished: chunk references of uploads (written to the live document as a background
/// change, never an undo step) and downloaded bytes (into the model; nothing to write). True if the model changed.
pub fn pull_media(s: &mut Session<Deck>) -> Result<bool, KitError> {
    let mut changed = false;
    let d = s.model().clone();
    let uploaded = d.shared.take_uploaded();
    if !uploaded.is_empty() {
        let mut chunks = (*d.chunks).clone();
        for (e, c) in uploaded {
            if d.ids.local(e).is_some_and(|l| d.pres.media.iter().any(|m| m.id.0 == l)) {
                chunks.insert(e, c);
            }
        }
        let new = Arc::new(d.with_chunks(chunks));
        s.background_change(&d, &new)?;
        changed = true;
    }
    if d.shared.take_fetched() {
        let d = s.model().clone();
        if let Some(pres) = fill_media(&d) {
            let new = Arc::new(d.with_pres(Arc::new(pres)));
            s.background_change(&d, &new)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// `d`'s presentation with the bytes that came down filled in, if any did.
fn fill_media(d: &LiveDeck) -> Option<deckcraft_model::Presentation> {
    let mut pres = None::<deckcraft_model::Presentation>;
    for (i, m) in d.pres.media.iter().enumerate() {
        if !m.data.is_empty() {
            continue;
        }
        let Some(c) = d.ids.elem(m.id.0).and_then(|e| d.chunks.get(&e)) else { continue };
        let Some(bytes) = d.shared.bytes(c) else { continue };
        let p = pres.get_or_insert_with(|| (*d.pres).clone());
        if let Some(item) = p.media.get_mut(i) {
            item.data = bytes;
        }
    }
    pres
}

/// Whether a media item still waits for its bytes (a save now would write it empty).
pub fn media_missing(d: &LiveDeck) -> bool {
    d.pres.media.iter().any(|m| m.data.is_empty() && m.link.is_none())
}
