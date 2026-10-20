//! The engine's side of a live document: the kit's session for each live presentation, the hook after each command,
//! undo through the kit, open, save and autosave, the banners, presence, and the media steps every poll makes.
//!
//! The hook compares the presentation with the one the kit last had (`base`) wherever a command may have changed it:
//! after each outermost command, and at each poll. So every way the presentation changes (a command, a drag's end, an
//! input method's text, AutoCorrect's fix-ups inside a command) reaches the kit once, as one undo step, and a
//! command the guard rolled back sends nothing (the presentation is `base` again). While a drag or a composition is
//! under way nothing is sent and remote changes wait; both go at the first poll after it ends.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_kit::{Agent, AgentBlobs, KitError, Open, Session as Kit, Status};
use deckcraft_model::{Presentation, ShapeId};
use serde_json::{Value, json};

use super::{Deck, LiveDeck, Shared};
use crate::{EngineError, Result, Session};

/// An autosave this long after the last local change.
pub const AUTOSAVE_AFTER: Duration = Duration::from_secs(5);

/// Puts the bytes of media items the live document has no chunk reference for yet into the blob store (on the worker;
/// a failed upload is tried again at the next call).
pub fn push_media(s: &Kit<Deck>) {
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
    // Bytes that didn't come down yet (a failed download is tried again, after a pause).
    for m in &d.pres.media {
        if m.data.is_empty()
            && let Some(c) = d.ids.elem(m.id.0).and_then(|e| d.chunks.get(&e))
        {
            d.shared.fetch(*c);
        }
    }
}

/// Takes what the media worker finished: chunk references of uploads (written to the live document as a background
/// change, never an undo step) and downloaded bytes (into the model; nothing to write). True if the model changed.
pub fn pull_media(s: &mut Kit<Deck>) -> std::result::Result<bool, KitError> {
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
fn fill_media(d: &LiveDeck) -> Option<Presentation> {
    let mut pres = None::<Presentation>;
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

/// What the media items still without their bytes wait for, for the badge: the machine that added one and hasn't
/// sent its bytes yet ("waiting for media from the laptop"), or a download under way.
pub fn media_waiting(d: &LiveDeck) -> Option<String> {
    let mut from = std::collections::BTreeSet::new();
    let mut downloading = false;
    for m in d.pres.media.iter().filter(|m| m.data.is_empty() && m.link.is_none()) {
        let e = d.ids.elem(m.id.0);
        if e.is_some_and(|e| d.chunks.contains_key(&e)) {
            downloading = true;
        } else {
            from.insert(e.and_then(|e| d.from.get(&e)).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| "another computer".into()));
        }
    }
    if !from.is_empty() {
        Some(format!("waiting for media from {}", from.into_iter().collect::<Vec<_>>().join(", ")))
    } else {
        downloading.then(|| "downloading media".to_owned())
    }
}

/// How many media items this machine added whose bytes haven't gone to Craft Cloud yet (the others wait for them).
pub fn media_unsent(d: &LiveDeck) -> usize {
    d.pres.media.iter().filter(|m| !m.data.is_empty() && d.ids.elem(m.id.0).is_some_and(|e| !d.chunks.contains_key(&e))).count()
}

/// A presentation opened live (off the UI thread), waiting to be adopted.
pub struct LiveOpen {
    kit: Kit<Deck>,
    machine: String,
}

impl std::fmt::Debug for LiveOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveOpen").field("path", &self.kit.path()).finish_non_exhaustive()
    }
}

/// The file `path` as the agent names it (relative to the synced folder, `/`-separated), if it is in it.
pub fn relative_path(folder: &str, path: &str) -> Option<String> {
    let (folder, path) = (std::path::Path::new(folder), std::path::Path::new(path));
    let canon = |p: &std::path::Path| crate::io::canonical(p).unwrap_or_else(|| p.to_path_buf());
    let rel = canon(path).strip_prefix(canon(folder)).ok()?.to_path_buf();
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Opens `path` (read as `pres` from `bytes`) live through `agent`, if it is a `.deckcraft` file in the Craft Cloud
/// folder and the agent runs. Blocking (up to 30 s while the agent syncs the file first): call it off the UI thread.
/// `None`: it opens as a plain file.
pub fn try_open(agent: &Agent, path: &str, pres: &Presentation, bytes: &[u8]) -> Option<std::result::Result<LiveOpen, String>> {
    if !path.to_ascii_lowercase().ends_with(".deckcraft") || !deckcraft_format::sniff(bytes) {
        return None;
    }
    let client = agent.client().ok()?;
    let status = client.status().ok()?;
    let rel = relative_path(status.folder.as_deref()?, path)?;
    open_at(agent, &rel, pres, bytes, status.machine.unwrap_or_default())
}

/// Opens the file the agent knows as `rel` live (read as `pres` from `bytes`), as `machine`. `None`: not live now.
pub fn open_at(agent: &Agent, rel: &str, pres: &Presentation, bytes: &[u8], machine: String) -> Option<std::result::Result<LiveOpen, String>> {
    if let Some(why) = too_big_for_live(pres) {
        return Some(Err(why));
    }
    let client = agent.client().ok()?;
    let shared = Arc::new(Shared::new(Arc::new(AgentBlobs(client.blobs()))));
    // The file's own media bytes are known here: the live document's chunk references find them without a download
    // (a machine reopening offline has its pictures).
    for m in &pres.media {
        if !m.data.is_empty() {
            shared.seed(m.data.clone());
        }
    }
    let model = Arc::new(LiveDeck::new(Arc::new(pres.clone()), shared).with_machine(&machine));
    match Kit::<Deck>::open(agent, rel, model, Some(bytes)) {
        Ok(Open::Live(kit)) => Some(Ok(LiveOpen { kit: *kit, machine })),
        Ok(Open::NotLive(_)) => None,
        Err(e) => Some(Err(e.to_string())),
    }
}

/// What a live open came to: the presentation opened live, or why it opens as a plain file (to tell the user).
pub type LiveTry = Option<std::result::Result<LiveOpen, String>>;

/// Reads `path` as `file.open` does, and opens it live too when it can be (see [`try_open`]). A live open that fails
/// says why; the file then opens as a plain one. Blocking: call it where files are read (off the UI thread there).
pub fn read_with_live(path: &str, agent: &Agent) -> Result<(crate::cmd::file::LoadedFile, LiveTry)> {
    let bytes = crate::io::read_file(path).map_err(EngineError::Other)?;
    let (doc, damaged) = crate::cmd::file::open_checked(path, &bytes)?;
    let native = deckcraft_format::sniff(&bytes) || deckcraft_pptx::sniff(&bytes);
    let live = if damaged.is_empty() { try_open(agent, path, &doc, &bytes) } else { None };
    if let Some(Err(e)) = &live {
        log::warn!("opening live failed, opening as a plain file: {e}");
    }
    Ok((crate::cmd::file::LoadedFile { path: path.to_owned(), doc, native, damaged }, live))
}

/// Whether paths `a` and `b` name the same file: the same once resolved (links, `..`, and on Windows case and
/// separators), or, for a file not there yet, the same spelling with separators and (on Windows) case alike.
pub(crate) fn same_file(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(x), Ok(y)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return x == y;
    }
    let plain = |p: &str| {
        let p = p.replace('\\', "/");
        if cfg!(windows) { p.to_lowercase() } else { p }
    };
    plain(a) == plain(b)
}

/// The largest media item a live presentation can hold (Craft Cloud's blob limit).
pub const MAX_MEDIA_BYTES: usize = 64 * 1024 * 1024;

/// Why `p` can't be live, if it can't: a media item over [`MAX_MEDIA_BYTES`].
pub fn too_big_for_live(p: &Presentation) -> Option<String> {
    let m = p.media.iter().find(|m| m.data.len() > MAX_MEDIA_BYTES)?;
    Some(format!(
        "{} is {} MB; Craft Cloud carries media of up to {} MB, so this presentation opens as a plain file here (edits don't go to your other computers)",
        m.name,
        m.data.len() / (1024 * 1024),
        MAX_MEDIA_BYTES / (1024 * 1024)
    ))
}

/// What the window shows over a live document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Banner {
    /// The file was changed outside Craft: bring its content in (it replaces the live edits made since it was
    /// written; Undo brings them back) or keep the live document.
    ChangedOutside,
    /// A newer DeckCraft made this document: it opens read-only.
    ReadOnly(String),
}

/// Another machine's presence: its name, a colour slot, and the shapes it has selected on the slide it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub machine: String,
    pub slot: usize,
    pub slide: Option<deckcraft_model::SlideId>,
    pub shapes: Vec<ShapeId>,
}

/// One live presentation.
pub(crate) struct LiveDoc {
    kit: Kit<Deck>,
    /// The presentation the kit has: the document is compared with it to find a local change.
    base: Arc<Presentation>,
    /// The file on disk (absolute), where autosave writes.
    path: String,
    machine: String,
    /// When the last local change was made (autosave counts from it).
    changed_at: Option<Instant>,
    /// The file's content as read at the open, for "Bring it in".
    file: Option<Presentation>,
    banner: Option<Banner>,
    /// The presence last sent.
    presence: Option<Vec<u8>>,
    /// The last error, for the badge's hover.
    error: Option<String>,
}

/// The live presentations of a session.
#[derive(Default)]
pub(crate) struct Lives {
    docs: HashMap<u64, LiveDoc>,
    /// Opened off the UI thread, waiting for `file.adopt`, by path.
    staged: Vec<(String, LiveOpen, Option<Presentation>)>,
    pub(crate) agent: Option<Agent>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    /// What the last table command did (taken by the next sync).
    table_edit: Option<super::ids::TableEdit>,
}

impl Session {
    /// Opens `.deckcraft` files in the Craft Cloud folder live through `agent` from now on (the desktop app's choice;
    /// the CLI and tests leave it off). `wake` repaints the window when something comes from the agent.
    pub fn enable_live(&mut self, agent: Agent, wake: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.live.agent = Some(agent);
        self.live.wake = wake;
    }

    pub fn live_agent(&self) -> Option<Agent> {
        self.live.agent.clone()
    }

    /// Hands over a presentation opened live off the UI thread; `file.adopt` of `path` makes it the document's.
    pub fn stage_live(&mut self, path: &str, open: LiveOpen, file: Option<Presentation>) {
        self.live.staged.retain(|(p, _, _)| p != path);
        self.live.staged.push((path.to_owned(), open, file));
        if self.live.staged.len() > 4 {
            self.live.staged.remove(0);
        }
    }

    /// A table command's edit (`rowAbove`, `rowBelow`, `colLeft`, `colRight`, `delRow`, `delColumn` at row `r`,
    /// column `c`), for the next sync to keep row and column identities exact.
    pub(crate) fn note_table_edit(&mut self, op: &str, r: usize, c: usize) {
        use super::ids::TableEdit;
        self.live.table_edit = match op {
            "rowAbove" => Some(TableEdit::InsertRow(r)),
            "rowBelow" => Some(TableEdit::InsertRow(r.saturating_add(1))),
            "colLeft" => Some(TableEdit::InsertCol(c)),
            "colRight" => Some(TableEdit::InsertCol(c.saturating_add(1))),
            "delRow" => Some(TableEdit::DeleteRow(r)),
            "delColumn" | "delCol" => Some(TableEdit::DeleteCol(c)),
            _ => None,
        };
    }

    /// For tests: whether a table command's edit waits for the next sync.
    #[cfg(test)]
    pub(crate) fn table_note_waits(&self) -> bool {
        self.live.table_edit.is_some()
    }

    /// What a live open came to: staged for `file.adopt` of `path`, or, when it failed, why it opens as a plain file
    /// (said to the user).
    pub fn stage_live_try(&mut self, path: &str, live: LiveTry, file: &Presentation) {
        match live {
            Some(Ok(open)) => self.stage_live(path, open, Some(file.clone())),
            Some(Err(why)) => self.ui_requests.push(crate::UiRequest::Message { text: format!("Opened as a plain file, not live: {why}") }),
            None => {}
        }
    }

    /// Whether any presentation is live (its autosave and presence run on time).
    pub fn has_live(&self) -> bool {
        !self.live.docs.is_empty()
    }

    /// Whether the presentation `uid` is live.
    pub fn is_live_uid(&self, uid: u64) -> bool {
        self.live.docs.contains_key(&uid)
    }

    /// Whether the active presentation is live.
    pub fn is_live(&self) -> bool {
        self.active().is_some_and(|d| self.live.docs.contains_key(&d.uid))
    }

    /// The kit's status for the active presentation, if it is live (for the badge).
    pub fn live_status(&self) -> Option<Status> {
        let d = self.active()?;
        let l = self.live.docs.get(&d.uid)?;
        let mut st = l.kit.status().clone();
        if st.error.is_none() {
            st.error = l.error.clone().or_else(|| l.kit.model().shared.error());
        }
        Some(st)
    }

    /// What the active live presentation's media waits for, if anything ("waiting for media from the laptop"), for
    /// the badge.
    pub fn live_media_waiting(&self) -> Option<String> {
        let d = self.active()?;
        media_waiting(self.live.docs.get(&d.uid)?.kit.model())
    }

    /// How many media items of every live presentation open here haven't gone to Craft Cloud yet: closing the window
    /// waits a little for them (the other computers can't have them until they have).
    pub fn live_unsent_media(&self) -> usize {
        self.live.docs.values().map(|l| media_unsent(l.kit.model())).sum()
    }

    /// The banner over the active presentation, if it has one.
    pub fn live_banner(&self) -> Option<Banner> {
        let d = self.active()?;
        self.live.docs.get(&d.uid)?.banner.clone()
    }

    /// The other machines' selections on the active presentation.
    pub fn live_presence(&self) -> Vec<Presence> {
        let Some(d) = self.active() else { return Vec::new() };
        let Some(l) = self.live.docs.get(&d.uid) else { return Vec::new() };
        let ids = l.kit.model().ids.clone();
        let mut out: Vec<Presence> = l
            .kit
            .peers()
            .iter()
            .filter_map(|(peer, p)| {
                let v: Value = serde_json::from_slice(&p.bytes).ok()?;
                let elem = |x: &Value| x.as_str().and_then(craft_cloud_kit::ElemId::from_hex).and_then(|e| ids.local(e));
                Some(Presence {
                    machine: v.get("m").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    slot: (*peer % 8) as usize,
                    slide: v.get("slide").and_then(elem).map(deckcraft_model::SlideId),
                    shapes: v.get("s").and_then(Value::as_array).map(|a| a.iter().filter_map(elem).map(ShapeId).collect()).unwrap_or_default(),
                })
            })
            .collect();
        out.sort_by(|a, b| a.machine.cmp(&b.machine).then(a.slot.cmp(&b.slot)));
        out
    }

    /// Makes the presentation just adopted from `path` live, if it was opened live (the open thread staged it).
    pub(crate) fn adopt_live(&mut self, path: &str, keep: bool) {
        let Some(i) = self.live.staged.iter().position(|(p, _, _)| p == path) else { return };
        let (_, open, file) = self.live.staged.remove(i);
        if !keep {
            return;
        }
        let LiveOpen { mut kit, machine } = open;
        if let Some(w) = self.live.wake.clone() {
            let wk = w.clone();
            kit.set_waker(Arc::new(move || wk()));
            kit.model().shared.set_wake(w);
        }
        let Some(st) = self.active_mut() else { return };
        let model = kit.model().pres.clone();
        // The live document may be ahead of the file: its content is the document's. It isn't an edit made here, so
        // it doesn't read as unsaved; if the file is behind, `file_behind` has it saved (never the other way round:
        // a file ahead of the live document is never written over by the open).
        if !Arc::ptr_eq(&model, &st.doc) {
            st.doc = model.clone();
            st.saved_doc = model.clone();
            st.revision += 1;
        }
        let banner = if let Some(ro) = &kit.status().read_only {
            Some(Banner::ReadOnly(format!(
                "This presentation was made by a newer {} (layout {}): update DeckCraft to edit it.",
                if ro.app.is_empty() { "app" } else { &ro.app },
                ro.version
            )))
        } else if kit.status().changed_outside {
            Some(Banner::ChangedOutside)
        } else {
            None
        };
        push_media(&kit);
        let uid = st.uid;
        self.live
            .docs
            .insert(uid, LiveDoc { kit, base: model, path: path.to_owned(), machine, changed_at: None, file, banner, presence: None, error: None });
    }

    /// The active presentation was saved as `path`: if it was live as another file, it stops being live (its edits
    /// no longer go to the others, nor theirs to it, and the live file is no longer written from it).
    pub(crate) fn leave_live_for(&mut self, path: &str) {
        let Some(uid) = self.active().map(|d| d.uid) else { return };
        let Some(l) = self.live.docs.get(&uid) else { return };
        if same_file(&l.path, path) {
            return;
        }
        self.drop_live(uid);
        self.ui_requests.push(crate::UiRequest::Message {
            text: "Saved as a new file: this window is that file now, no longer the presentation open on your other computers.".into(),
        });
    }

    /// Forgets the live side of a presentation being closed.
    pub(crate) fn drop_live(&mut self, uid: u64) {
        self.live.docs.remove(&uid);
    }

    /// The hook: every live presentation whose document isn't what the kit has (and that no gesture or composition
    /// holds) sends the change, as one undo step. A change the kit refuses for good (read-only) is taken back.
    pub(crate) fn sync_live(&mut self) {
        let table_edit = self.live.table_edit.take();
        let uids: Vec<u64> = self.live.docs.keys().copied().collect();
        for uid in uids {
            let Some(st) = self.docs.iter_mut().find(|d| d.uid == uid) else { continue };
            let Some(l) = self.live.docs.get_mut(&uid) else { continue };
            if Arc::ptr_eq(&st.doc, &l.base) || st.interaction.is_some() || st.composition.is_some() {
                continue;
            }
            // Media over Craft Cloud's limit can't go live: the change is taken back, with why.
            if too_big_for_live(&st.doc).is_some() && too_big_for_live(&l.base).is_none() {
                st.doc = l.base.clone();
                st.revision += 1;
                crate::clamp_selection(st);
                self.ui_requests.push(crate::UiRequest::Message {
                    text: format!(
                        "That media file is over {} MB, the most Craft Cloud carries, so it wasn't added to this live presentation. Use a smaller file, or save a copy outside the Craft Cloud folder.",
                        MAX_MEDIA_BYTES / (1024 * 1024)
                    ),
                });
                continue;
            }
            let old = l.kit.model().clone();
            if !Arc::ptr_eq(&old.pres, &l.base) {
                l.base = old.pres.clone();
            }
            let new = Arc::new(old.wrap_with(st.doc.clone(), table_edit));
            match l.kit.local_change(&old, &new) {
                Ok(()) => {
                    l.base = st.doc.clone();
                    l.changed_at = Some(Instant::now());
                    l.error = None;
                    // DeckCraft's own snapshots aren't undo on a live document: the kit's are.
                    st.history.undo.clear();
                    st.history.redo.clear();
                    push_media(&l.kit);
                }
                Err(KitError::ReadOnly { .. }) => {
                    st.doc = l.base.clone();
                    st.revision += 1;
                    self.ui_requests.push(crate::UiRequest::Message {
                        text: "This presentation was made by a newer DeckCraft: update DeckCraft to edit it.".into(),
                    });
                }
                Err(e) => {
                    // Not sent (stalled, behind, refused): still an edit made here, which autosave keeps (without
                    // a record) until it can go out.
                    l.error = Some(e.to_string());
                    l.changed_at.get_or_insert_with(Instant::now);
                }
            }
        }
    }

    /// Once per frame: sends what is unsent, applies what came from the other machines (unless a gesture or a
    /// composition is under way), moves media, shares this machine's selection, and autosaves. True if a presentation
    /// changed (redraw).
    pub fn poll_live(&mut self) -> bool {
        self.sync_live();
        let uids: Vec<u64> = self.live.docs.keys().copied().collect();
        let mut changed = false;
        for uid in uids {
            changed |= self.poll_one(uid);
        }
        changed
    }

    fn poll_one(&mut self, uid: u64) -> bool {
        let Some(st) = self.docs.iter_mut().find(|d| d.uid == uid) else { return false };
        let Some(l) = self.live.docs.get_mut(&uid) else { return false };
        let busy = st.interaction.is_some() || st.composition.is_some();
        // Unsent local edits (the kit refused them for now) are never written over by a remote change.
        let unsent = !Arc::ptr_eq(&st.doc, &l.base);
        l.kit.hold_remote(busy || unsent);
        let polled = l.kit.poll();
        let mut changed = polled.changed;
        match pull_media(&mut l.kit) {
            Ok(c) => changed |= c,
            Err(e) => l.error = Some(e.to_string()),
        }
        push_media(&l.kit);
        if changed && !busy && !unsent {
            let model = l.kit.model().pres.clone();
            follow(st, &l.base, &model);
            // Another machine's change isn't an unsaved edit here (that machine saves it; the file catching up is
            // `file_behind`'s): a clean document stays clean.
            let clean = !st.is_dirty();
            st.doc = model.clone();
            if clean {
                st.saved_doc = model.clone();
            }
            l.base = model;
            st.revision += 1;
            st.history.undo.clear();
            st.history.redo.clear();
            crate::clamp_selection(st);
        }
        if let Some(ro) = &polled.status.read_only
            && !matches!(l.banner, Some(Banner::ReadOnly(_)))
        {
            l.banner = Some(Banner::ReadOnly(format!(
                "This presentation was made by a newer DeckCraft (layout {}): update DeckCraft to edit it.",
                ro.version
            )));
        }
        // Presence: the slide shown and the shapes selected.
        let ids = l.kit.model().ids.clone();
        let slide = st.current_slide().and_then(|s| ids.elem(s.id.0));
        let shapes: Vec<String> = st.selection.shapes.iter().filter_map(|id| ids.elem(id.0)).map(|e| e.hex()).collect();
        let bytes = serde_json::to_vec(&json!({"m": l.machine, "slide": slide.map(|e| e.hex()), "s": shapes})).unwrap_or_default();
        if l.presence.as_ref() != Some(&bytes) {
            let _ = l.kit.set_presence(Some(bytes.clone()));
            l.presence = Some(bytes);
        }
        // Autosave, as docs/kit.md says: a while after a change made here, or when the file is behind the live document
        // (the kit allows that at most once per CATCH_UP_EVERY), and nothing is in the way. A change that came from
        // elsewhere is never a reason by itself: the machine that made it saves it.
        // (Edits the kit refused stay a change made here, but once saved as they are they aren't written again until
        // something changes: each write is an edit outside Craft to the agent.)
        let local_due = l.changed_at.is_some_and(|t| t.elapsed() >= AUTOSAVE_AFTER) && (!unsent || st.is_dirty());
        // A newer DeckCraft's document is never written by this one (it would drop what this one doesn't know).
        let read_only = l.kit.status().read_only.is_some() || matches!(l.banner, Some(Banner::ReadOnly(_)));
        // Unsent edits don't hold it back: they are saved as they are, with no record (see `live_write`).
        let missing = if unsent { st.doc.media.iter().any(|m| m.data.is_empty() && m.link.is_none()) } else { media_missing(l.kit.model()) };
        let in_the_way = busy || read_only || missing || matches!(l.banner, Some(Banner::ChangedOutside));
        // (`file_behind` is true after an edit made here too: those wait for their 5 s.)
        let behind = l.changed_at.is_none() && l.kit.file_behind();
        if !in_the_way && (local_due || behind) {
            match self.live_write(uid) {
                Ok(_) => changed = true,
                Err(e) => {
                    if let Some(l) = self.live.docs.get_mut(&uid) {
                        l.error = Some(e.to_string());
                        l.changed_at = Some(Instant::now());
                    }
                }
            }
        }
        changed
    }

    /// Saves a live presentation to its file, in the kit's order: the save point, the bytes from its model, the
    /// record, then the write. With no save point (read-only, stopped, behind) it saves the document as it is, with no
    /// record. Returns the bytes written.
    fn live_write(&mut self, uid: u64) -> Result<usize> {
        let unsent =
            self.docs.iter().find(|d| d.uid == uid).is_some_and(|st| self.live.docs.get(&uid).is_some_and(|l| !Arc::ptr_eq(&st.doc, &l.base)));
        let Some(l) = self.live.docs.get_mut(&uid) else { return Err(EngineError::NoDocument) };
        // Edits on screen the live document doesn't have yet: the document is saved as it is, with no record (the
        // kit's fallback); a save point would write the live document without them.
        let point = if unsent { Err(KitError::NoRecord("edits not sent yet".into())) } else { l.kit.save_point() };
        let (pres, bytes) = match &point {
            Ok(p) => {
                let pres = p.model().pres.clone();
                if media_missing(p.model()) {
                    return Err(EngineError::Other("a picture or media file is still on its way from Craft Cloud; it saves once it is here".into()));
                }
                let bytes = deckcraft_format::save(&pres).map_err(|e| EngineError::Other(e.to_string()))?;
                (pres, bytes)
            }
            Err(_) => {
                let st = self.docs.iter().find(|d| d.uid == uid).ok_or(EngineError::NoDocument)?;
                let bytes = deckcraft_format::save(&st.doc).map_err(|e| EngineError::Other(e.to_string()))?;
                (st.doc.clone(), bytes)
            }
        };
        if let Ok(p) = &point {
            l.kit.record(p, &bytes);
        }
        crate::io::write_file(&l.path, &bytes).map_err(EngineError::Other)?;
        l.changed_at = None;
        // The file read at the open is no longer the file.
        l.file = None;
        if matches!(l.banner, Some(Banner::ChangedOutside)) {
            l.banner = None;
        }
        let n = bytes.len();
        if let Some(st) = self.docs.iter_mut().find(|d| d.uid == uid) {
            // Saved: the document as the save saw it (a remote change since stays unsaved).
            st.saved_doc = if Arc::ptr_eq(&st.doc, &pres) { st.doc.clone() } else { pres };
            st.revision += 1;
            if let Some(dir) = &self.recovery_dir {
                crate::recovery::discard(dir, uid);
            }
        }
        Ok(n)
    }

    /// `file.save` of a live presentation to its own file: the kit's order. `None` when the active presentation
    /// isn't live or `path` is another file (saving a copy: as always).
    pub(crate) fn live_save(&mut self, path: &str, format: &str) -> Option<Result<Value>> {
        let uid = self.active()?.uid;
        let l = self.live.docs.get(&uid)?;
        if !same_file(&l.path, path) {
            return None;
        }
        // Another format over the live file would leave it unreadable as the live presentation's file.
        if format != "deckcraft" {
            return Some(Err(EngineError::Other(
                "This is the live presentation's file: to keep it in another format, use Save As with another name.".into(),
            )));
        }
        if l.kit.status().read_only.is_some() {
            return Some(Err(EngineError::Other(
                "This presentation was made by a newer DeckCraft, which this one can't save without losing what it doesn't know. Update DeckCraft, or use Save As to keep a copy.".into(),
            )));
        }
        self.sync_live();
        Some(self.live_write(uid).map(|n| json!({"path": path, "bytes": n, "live": true})))
    }

    /// Undo or redo of a live presentation, through the kit: only this machine's changes. `None` when the active
    /// presentation isn't live.
    pub(crate) fn live_undo(&mut self, redo: bool) -> Option<Result<Value>> {
        let uid = self.active()?.uid;
        self.live.docs.get(&uid)?;
        self.sync_live();
        let st = self.docs.iter_mut().find(|d| d.uid == uid)?;
        let l = self.live.docs.get_mut(&uid)?;
        if !Arc::ptr_eq(&st.doc, &l.base) {
            // An undo now would show the live document without the edits on screen that didn't go out.
            return Some(Err(EngineError::Other(format!(
                "Undo waits until your last edits reach Craft Cloud ({}). Save keeps them meanwhile.",
                l.error.clone().unwrap_or_else(|| "not sent yet".into())
            ))));
        }
        let name = l.kit.step_name(!redo).map(str::to_owned);
        let done = if redo { l.kit.redo() } else { l.kit.undo() };
        Some(match done {
            Ok(done) => {
                let model = l.kit.model().pres.clone();
                if !Arc::ptr_eq(&model, &st.doc) {
                    follow(st, &l.base, &model);
                    st.doc = model.clone();
                    l.base = model;
                    st.revision += 1;
                    crate::clamp_selection(st);
                }
                l.changed_at = Some(Instant::now());
                Ok(json!({"label": name.unwrap_or_default(), "done": done, "live": true}))
            }
            Err(e) => Err(EngineError::Other(e.to_string())),
        })
    }

    /// Whether undo (or redo) has a step on a live presentation. `None` when the active one isn't live.
    pub(crate) fn live_can_undo(&self, redo: bool) -> Option<bool> {
        let uid = self.active()?.uid;
        let l = self.live.docs.get(&uid)?;
        Some(if redo { l.kit.can_redo() } else { l.kit.can_undo() })
    }

    /// The name of the step undo (or redo) would take back on a live presentation, when it has one.
    pub fn live_step_name(&self, redo: bool) -> Option<String> {
        let uid = self.active()?.uid;
        self.live.docs.get(&uid)?.kit.step_name(!redo).map(str::to_owned)
    }

    /// Revert, or the banner's "Bring it in", on a live presentation: the file's content replaces the live content
    /// as one change everyone gets (Undo brings the live edits back). `None` when the active one isn't live.
    pub(crate) fn live_revert(&mut self) -> Option<Result<Value>> {
        let uid = self.active()?.uid;
        let path = self.live.docs.get(&uid)?.path.clone();
        self.sync_live();
        // "Bring it in" brings the outside edit read at the open; Revert brings the file as it is on disk now.
        let outside = self.live.docs.get_mut(&uid).filter(|l| matches!(l.banner, Some(Banner::ChangedOutside))).and_then(|l| l.file.take());
        let file = match outside {
            Some(f) => f,
            None => match crate::io::read_file(&path).map_err(EngineError::Other).and_then(|b| crate::cmd::file::open_presentation(&path, &b)) {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            },
        };
        let st = self.docs.iter_mut().find(|d| d.uid == uid)?;
        let l = self.live.docs.get_mut(&uid)?;
        let replaced = l.kit.model().wrap(Arc::new(file));
        Some(match l.kit.replace_with_file(replaced) {
            Ok(()) => {
                let model = l.kit.model().pres.clone();
                st.doc = model.clone();
                st.saved_doc = model.clone();
                l.base = model;
                l.changed_at = Some(Instant::now());
                st.revision += 1;
                crate::clamp_selection(st);
                l.banner = None;
                Ok(json!({"live": true, "replaced": true}))
            }
            Err(e) => Err(EngineError::Other(e.to_string())),
        })
    }

    /// The banner's "Keep the live document": the banner goes; the next save writes the live content.
    pub fn live_keep(&mut self) {
        let Some(uid) = self.active().map(|d| d.uid) else { return };
        if let Some(l) = self.live.docs.get_mut(&uid) {
            l.banner = None;
            l.file = None;
            l.changed_at = Some(Instant::now() - AUTOSAVE_AFTER);
        }
        if let Some(st) = self.active_mut()
            && !st.is_dirty()
        {
            // So the autosave writes the live content over the outside edit.
            st.saved_doc = Arc::new((*st.doc).clone());
        }
    }
}

/// Keeps the user where they were across a change that came from elsewhere: the current slide (by id), and the caret
/// in the text being edited (moved by what was typed before it).
fn follow(st: &mut crate::DocState, before: &Arc<Presentation>, after: &Arc<Presentation>) {
    let current = before.slides.get(st.selection.slide).map(|s| s.id);
    if let Some(id) = current
        && let Some(i) = after.slide_index(id)
    {
        st.selection.slide = i;
    }
    let Some(t) = st.selection.text.clone() else { return };
    let old_body = crate::cmd::text::body_of(st, &t).cloned();
    let mut probe = st.clone();
    probe.doc = after.clone();
    let new_body = crate::cmd::text::body_of(&probe, &t).cloned();
    let (Some(old), Some(new)) = (old_body, new_body) else { return };
    if old == new {
        return;
    }
    let (ot, nt): (Vec<char>, Vec<char>) = (old.text().chars().collect(), new.text().chars().collect());
    let prefix = ot.iter().zip(&nt).take_while(|(a, b)| a == b).count();
    let max = ot.len().min(nt.len()) - prefix;
    let suffix = ot.iter().rev().zip(nt.iter().rev()).take(max).take_while(|(a, b)| a == b).count();
    let flat = |(p, c): (usize, usize), body: &deckcraft_model::TextBody| -> usize {
        body.paragraphs.iter().take(p).map(|x| x.char_len() + 1).sum::<usize>() + c
    };
    let unflat = |mut o: usize, body: &deckcraft_model::TextBody| -> (usize, usize) {
        for (i, p) in body.paragraphs.iter().enumerate() {
            let n = p.char_len();
            if o <= n {
                return (i, o);
            }
            o -= n + 1;
        }
        let last = body.paragraphs.len().saturating_sub(1);
        (last, body.paragraphs.get(last).map_or(0, |p| p.char_len()))
    };
    let shift = |pos: (usize, usize)| {
        let o = flat(pos, &old);
        let o = if o <= prefix {
            o
        } else if o >= ot.len() - suffix {
            // After the change: moved by what it added or removed.
            (o + nt.len()).saturating_sub(ot.len())
        } else {
            prefix + (nt.len() - suffix).saturating_sub(prefix)
        };
        unflat(o, &new)
    };
    if let Some(sel) = st.selection.text.as_mut() {
        sel.caret = shift(t.caret);
        sel.anchor = shift(t.anchor);
        sel.line_end = None;
    }
}

/// For the tests: the kit session of the active presentation.
#[cfg(test)]
impl Session {
    pub(crate) fn live_kit(&mut self) -> Option<&mut Kit<Deck>> {
        let uid = self.active()?.uid;
        self.live.docs.get_mut(&uid).map(|l| &mut l.kit)
    }
}
