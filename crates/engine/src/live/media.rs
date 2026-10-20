//! Media and embedded fonts through the blob store: their bytes never go into the live document. A media item's row
//! names its chunk reference once its bytes are in the store; uploads and downloads run on a worker thread, so a 50 MB
//! video freezes neither app. A media item another machine added shows once its bytes are here; one added here goes
//! to the others once its upload is done (later, when the agent is away for now: it is tried again).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use craft_cloud_kit::{BlobStore, ChunkRef, ElemId};
use deckcraft_model::Presentation;

use super::ids::Ids;

/// A failed download is tried again after this long.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Job {
    Put(ElemId, Arc<Vec<u8>>),
    Get(ChunkRef),
}

#[derive(Default)]
struct State {
    /// Bytes by chunk hash: what came down, and what went up.
    cache: HashMap<[u8; 32], Arc<Vec<u8>>>,
    /// The chunk of bytes this machine uploaded, by the bytes' allocation (so a media item's chunk is found again).
    put_of: HashMap<usize, ChunkRef>,
    uploading: HashSet<ElemId>,
    fetching: HashSet<[u8; 32]>,
    uploaded: Vec<(ElemId, ChunkRef)>,
    fetched: bool,
    /// When a download last failed, by chunk hash (tried again after [`RETRY_AFTER`]).
    failed: HashMap<[u8; 32], std::time::Instant>,
    /// When an upload last failed, by media element.
    put_failed: HashMap<ElemId, std::time::Instant>,
    /// The last failure (shown, and tried again).
    error: Option<String>,
}

/// What the sessions of one document share about media, outside the model.
pub struct Shared {
    store: Arc<dyn BlobStore>,
    state: Arc<Mutex<State>>,
    /// The worker's queue; `None` runs each job at once (tests).
    worker: Mutex<Option<Sender<Job>>>,
    inline: bool,
    wake: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

impl Shared {
    /// Media through `store`, uploads and downloads on a worker thread.
    pub fn new(store: Arc<dyn BlobStore>) -> Self {
        Self { store, state: Arc::default(), worker: Mutex::new(None), inline: false, wake: Arc::default() }
    }

    /// Media through `store`, each upload and download done at once (tests).
    pub fn inline(store: Arc<dyn BlobStore>) -> Self {
        Self { inline: true, ..Self::new(store) }
    }

    /// Called (from the worker) when an upload or download is done: the UI's repaint.
    pub fn set_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.wake) = Some(wake);
    }

    fn run(store: &dyn BlobStore, state: &Mutex<State>, job: Job) {
        match job {
            Job::Put(elem, data) => match store.put(&data) {
                Ok(chunk) => {
                    let mut s = lock(state);
                    s.cache.insert(chunk.hash, data.clone());
                    s.put_of.insert(Arc::as_ptr(&data) as usize, chunk);
                    s.uploading.remove(&elem);
                    s.uploaded.push((elem, chunk));
                    s.error = None;
                }
                Err(e) => {
                    let mut s = lock(state);
                    s.uploading.remove(&elem);
                    s.put_failed.insert(elem, std::time::Instant::now());
                    s.error = Some(format!("a picture or media file couldn't be stored yet: {e}"));
                }
            },
            Job::Get(chunk) => match store.get(&chunk) {
                Ok(bytes) => {
                    let mut s = lock(state);
                    s.cache.insert(chunk.hash, Arc::new(bytes.to_vec()));
                    s.fetching.remove(&chunk.hash);
                    s.fetched = true;
                }
                Err(e) => {
                    let mut s = lock(state);
                    s.fetching.remove(&chunk.hash);
                    s.failed.insert(chunk.hash, std::time::Instant::now());
                    s.error = Some(format!("a picture or media file couldn't be fetched yet: {e}"));
                }
            },
        }
    }

    fn submit(&self, job: Job) {
        if self.inline {
            Self::run(self.store.as_ref(), &self.state, job);
            return;
        }
        let mut worker = lock(&self.worker);
        if worker.is_none() {
            let (tx, rx) = channel::<Job>();
            let (store, state) = (self.store.clone(), self.state.clone());
            let wake = self.wake.clone();
            let spawned = std::thread::Builder::new().name("deckcraft-live-media".into()).spawn(move || {
                while let Ok(job) = rx.recv() {
                    Self::run(store.as_ref(), &state, job);
                    let w = lock(&wake).clone();
                    if let Some(w) = w {
                        w();
                    }
                }
            });
            if spawned.is_err() {
                drop(worker);
                Self::run(self.store.as_ref(), &self.state, job);
                return;
            }
            *worker = Some(tx);
        }
        let failed = worker.as_ref().map(|tx| tx.send(job));
        if let Some(Err(e)) = failed {
            *worker = None;
            drop(worker);
            Self::run(self.store.as_ref(), &self.state, e.0);
        }
    }

    /// Stores a media item's bytes, unless that is under way.
    pub fn upload(&self, elem: ElemId, data: Arc<Vec<u8>>) {
        {
            let mut s = lock(&self.state);
            if s.put_failed.get(&elem).is_some_and(|t| t.elapsed() < RETRY_AFTER) || !s.uploading.insert(elem) {
                return;
            }
        }
        self.submit(Job::Put(elem, data));
    }

    /// Fetches the bytes of `chunk`, unless they are here or on their way.
    pub fn fetch(&self, chunk: ChunkRef) {
        {
            let mut s = lock(&self.state);
            if s.cache.contains_key(&chunk.hash) {
                // Here already: the next pull puts it into the model.
                s.fetched = true;
                return;
            }
            if s.failed.get(&chunk.hash).is_some_and(|t| t.elapsed() < RETRY_AFTER) || !s.fetching.insert(chunk.hash) {
                return;
            }
        }
        self.submit(Job::Get(chunk));
    }

    /// The bytes of `chunk`, if they are here.
    pub fn bytes(&self, chunk: &ChunkRef) -> Option<Arc<Vec<u8>>> {
        lock(&self.state).cache.get(&chunk.hash).cloned()
    }

    /// Uploads done since the last call.
    pub fn take_uploaded(&self) -> Vec<(ElemId, ChunkRef)> {
        std::mem::take(&mut lock(&self.state).uploaded)
    }

    /// Whether downloads finished since the last call.
    pub fn take_fetched(&self) -> bool {
        std::mem::take(&mut lock(&self.state).fetched)
    }

    /// Whether anything is on its way (up or down).
    pub fn busy(&self) -> bool {
        let s = lock(&self.state);
        !s.uploading.is_empty() || !s.fetching.is_empty()
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.state).error.clone()
    }

    /// The chunks already known for `p`'s media (bytes this machine stored before).
    pub(crate) fn known_chunks(&self, ids: &Ids, p: &Presentation) -> HashMap<ElemId, ChunkRef> {
        let s = lock(&self.state);
        p.media.iter().filter_map(|m| Some((ids.elem(m.id.0)?, *s.put_of.get(&(Arc::as_ptr(&m.data) as usize))?))).collect()
    }
}
