//! Model values as live properties: each field of a slide, shape, cell or setting is its own property (its JSON, last
//! writer wins), so two machines changing different properties of one shape both keep their change. Local ids inside
//! them become element references (`"@<32 hex>"`) on the way in and local ids again on the way out. A value too big
//! for one property is cut into pieces under keys named by its hash, so concurrent writes never mix two versions.

use std::collections::HashMap;

use craft_cloud_kit::{ElemId, KitError, Props, PropsRead, Value};
use deckcraft_model::Presentation;
use serde_json::{Map, Value as Json};

use super::ids::Ids;

/// Keys whose number is an element's id.
const ID_KEYS: &[&str] = &[
    "id",
    "shape",
    "trigger",
    "media",
    "poster",
    "preview",
    "picture",
    "layout",
    "sound",
    "original",
    "slide",
    "zoomReturn",
    "regular",
    "bold",
    "italic",
    "boldItalic",
];
/// The longest property kept in one piece.
const PIECE: usize = 192 * 1024;
/// What a cut property's own key holds: this, the hash, and how many pieces.
const CUT: &str = "\u{1}cut:";

fn elem_text(e: ElemId) -> Json {
    Json::String(format!("@{}", e.hex()))
}

fn elem_of(v: &Json) -> Option<ElemId> {
    v.as_str()?.strip_prefix('@').and_then(ElemId::from_hex)
}

/// Local ids in `v` as element references (an id the map doesn't know, a dangling one, becomes 0).
pub(crate) fn to_live(v: &mut Json, ids: &Ids) {
    let elem = |x: &Json| x.as_u64().and_then(|n| u32::try_from(n).ok()).and_then(|n| ids.elem(n));
    match v {
        Json::Object(map) => {
            for (k, x) in map.iter_mut() {
                let k = k.as_str();
                if ID_KEYS.contains(&k) && x.is_u64() {
                    *x = elem(x).map_or(Json::from(0u32), elem_text);
                } else if k == "slides" && x.as_array().is_some_and(|a| a.iter().all(Json::is_u64)) {
                    if let Json::Array(a) = x {
                        *a = a.iter().filter_map(|n| elem(n).map(elem_text)).collect();
                    }
                } else if (k == "start" || k == "end") && is_pair(x) {
                    if let Json::Array(a) = x {
                        match a.first().and_then(elem) {
                            Some(e) => {
                                if let Some(first) = a.first_mut() {
                                    *first = elem_text(e);
                                }
                            }
                            None => *x = Json::Null,
                        }
                    }
                } else if k == "parts" && is_parts(x) {
                    if let Json::Array(a) = x {
                        a.retain_mut(|part| match part.get(2).and_then(elem) {
                            Some(e) => {
                                if let Some(id) = part.get_mut(2) {
                                    *id = elem_text(e);
                                }
                                true
                            }
                            None => false,
                        });
                    }
                } else {
                    to_live(x, ids);
                }
            }
        }
        Json::Array(a) => a.iter_mut().for_each(|x| to_live(x, ids)),
        _ => {}
    }
}

fn is_pair(x: &Json) -> bool {
    x.as_array().is_some_and(|a| a.len() == 2 && a.iter().all(Json::is_u64))
}

fn is_parts(x: &Json) -> bool {
    x.as_array().is_some_and(|a| a.iter().all(|p| p.as_array().is_some_and(|p| p.len() == 3 && p.get(2).is_some_and(Json::is_u64))))
}

/// Element references in `v` as local ids: an `id` the map doesn't know is an element another machine made (it gets
/// a fresh local id from `p`); a reference to one not seen yet gets a local id held for it (`Ids::is_pending`).
pub(crate) fn to_local(v: &mut Json, ids: &mut Ids, p: &mut Presentation) {
    match v {
        Json::Object(map) => {
            for (k, x) in map.iter_mut() {
                let k = k.as_str();
                if ID_KEYS.contains(&k) && elem_of(x).is_some() {
                    let e = elem_of(x);
                    // An element not seen yet (its change comes later) gets a local id held for it: the reference
                    // isn't dangling, and stays the same element.
                    let local = match e {
                        Some(e) if k == "id" => ids.local_or_new(e, p),
                        Some(e) => ids.local_or_pending(e, p),
                        None => None,
                    };
                    *x = Json::from(local.unwrap_or(0));
                } else if k == "slides" && x.as_array().is_some_and(|a| a.iter().all(|s| elem_of(s).is_some())) {
                    if let Json::Array(a) = x {
                        let mut out = Vec::with_capacity(a.len());
                        for s in a.iter() {
                            if let Some(l) = elem_of(s).and_then(|e| ids.local_or_pending(e, p)) {
                                out.push(Json::from(l));
                            }
                        }
                        *a = out;
                    }
                } else if (k == "start" || k == "end") && x.as_array().is_some_and(|a| a.len() == 2 && a.first().and_then(elem_of).is_some()) {
                    if let Json::Array(a) = x {
                        match a.first().and_then(elem_of).and_then(|e| ids.local_or_pending(e, p)) {
                            Some(l) => {
                                if let Some(first) = a.first_mut() {
                                    *first = Json::from(l);
                                }
                            }
                            None => *x = Json::Null,
                        }
                    }
                } else if k == "parts" && x.as_array().is_some_and(|a| a.iter().all(|p| p.get(2).and_then(elem_of).is_some())) {
                    if let Json::Array(a) = x {
                        let mut kept = Vec::with_capacity(a.len());
                        for mut part in std::mem::take(a) {
                            if let Some(l) = part.get(2).and_then(elem_of).and_then(|e| ids.local_or_pending(e, p)) {
                                if let Some(id) = part.get_mut(2) {
                                    *id = Json::from(l);
                                }
                                kept.push(part);
                            }
                        }
                        *a = kept;
                    }
                } else {
                    to_local(x, ids, p);
                }
            }
        }
        Json::Array(a) => a.iter_mut().for_each(|x| to_local(x, ids, p)),
        _ => {}
    }
}

/// FNV-1a, for the names of a cut property's pieces.
fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Sets property `key` to `text`, cut into pieces when it is long.
pub(crate) fn put(props: &mut Props<'_, '_>, key: &str, text: String) -> Result<(), KitError> {
    if text.len() <= PIECE {
        return props.set(key, text);
    }
    let h = hash(&text);
    let mut pieces = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let mut end = (at + PIECE).min(text.len());
        while end > at && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == at {
            end = text.len();
        }
        pieces.push(text.get(at..end).unwrap_or_default().to_owned());
        at = end;
    }
    props.set(key, format!("{CUT}{h:016x}:{}", pieces.len()))?;
    for (i, piece) in pieces.into_iter().enumerate() {
        props.set(&format!("{key}#{h:016x}.{i}"), piece)?;
    }
    Ok(())
}

/// A node's or cell's properties, read once.
pub(crate) struct Read {
    pub map: HashMap<String, Value>,
}

impl Read {
    pub fn new(p: &PropsRead) -> Self {
        Self { map: p.all().into_iter().collect() }
    }

    /// The text of property `key` (put back together if it was cut); `None` when it or a piece is missing.
    pub fn text(&self, key: &str) -> Option<String> {
        let v = self.map.get(key)?.as_str()?;
        let Some(cut) = v.strip_prefix(CUT) else { return Some(v.to_owned()) };
        let (h, n) = cut.split_once(':')?;
        let n: usize = n.parse().ok()?;
        let mut out = String::new();
        for i in 0..n {
            out.push_str(self.map.get(&format!("{key}#{h}.{i}"))?.as_str()?);
        }
        Some(out)
    }

    pub fn json(&self, key: &str) -> Option<Json> {
        serde_json::from_str(&self.text(key)?).ok()
    }

    /// The model fields among the properties (each a JSON value), leaving out pieces and `skip`.
    pub fn fields(&self, skip: &dyn Fn(&str) -> bool) -> Map<String, Json> {
        let mut out = Map::new();
        for k in self.map.keys() {
            if k.contains('#') || skip(k) {
                continue;
            }
            if let Some(v) = self.json(k) {
                out.insert(k.clone(), v);
            }
        }
        out
    }
}

/// Writes each field of `obj` (but `skip`) as its own property, local ids made element references.
pub(crate) fn put_fields(props: &mut Props<'_, '_>, obj: Map<String, Json>, skip: &[&str], ids: &Ids) -> Result<(), KitError> {
    let mut obj = Json::Object(obj);
    // Translated as a whole: a field may itself be a reference (a slide's `layout`).
    to_live(&mut obj, ids);
    let Json::Object(obj) = obj else { return Ok(()) };
    for (k, v) in obj {
        if skip.contains(&k.as_str()) || v.is_null() {
            continue;
        }
        put(props, &k, v.to_string())?;
    }
    Ok(())
}

/// A model value as a JSON object (`{}` when it isn't one).
pub(crate) fn object<T: serde::Serialize>(v: &T) -> Map<String, Json> {
    match serde_json::to_value(v) {
        Ok(Json::Object(m)) => m,
        _ => Map::new(),
    }
}

/// A value as JSON text with its local ids as element references.
pub(crate) fn live_text<T: serde::Serialize>(v: &T, ids: &Ids) -> String {
    let mut j = serde_json::to_value(v).unwrap_or(Json::Null);
    to_live(&mut j, ids);
    j.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_become_references_and_back() {
        let mut ids = Ids::default();
        let mut p = Presentation::default();
        let (shape, media) = (ids.elem_or_new(5), ids.elem_or_new(9));
        let mut v = serde_json::json!({"shape": 5, "media": 9, "bold": true, "start": [5, 2], "end": [77, 1], "slides": [5, 77],
            "parts": [["r", "t", 9], ["r", "t", 1234]], "x": {"picture": 9, "size": 12}});
        to_live(&mut v, &ids);
        assert_eq!(v["shape"], elem_text(shape));
        assert_eq!(v["media"], elem_text(media));
        assert_eq!(v["bold"], true);
        assert_eq!(v["start"][0], elem_text(shape));
        assert_eq!(v["end"], Json::Null, "a dangling end is dropped");
        assert_eq!(v["slides"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["parts"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["x"]["picture"], elem_text(media));
        to_local(&mut v, &mut ids, &mut p);
        assert_eq!(v["shape"], 5);
        assert_eq!(v["x"]["picture"], 9);
        assert_eq!(v["start"], serde_json::json!([5, 2]));
        // An element another machine made gets a fresh local id when it is an `id`; a reference to an unknown one is 0.
        let other = ElemId::new();
        let mut w = serde_json::json!({"id": format!("@{}", other.hex()), "shape": format!("@{}", ElemId::new().hex())});
        to_local(&mut w, &mut ids, &mut p);
        assert!(w["id"].as_u64().is_some_and(|n| n > 9 || n > 0));
        assert_eq!(ids.elem(u32::try_from(w["id"].as_u64().unwrap_or(0)).unwrap_or(0)), Some(other));
        // A reference to an element not seen yet: a local id held for it, the same when the element comes.
        let held = u32::try_from(w["shape"].as_u64().unwrap_or(0)).unwrap_or(0);
        assert!(held != 0 && ids.is_pending(held));
    }
}
