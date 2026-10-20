//! A text body as a live story: the paragraphs' characters joined by `\n` (a line break is its `\u{b}`), each run's
//! properties as character marks (one per field: two machines making one word bold and italic both keep theirs), each
//! paragraph's level and properties as paragraph marks. Fields and equations carry their kind as a mark too. The body
//! properties and list style aren't text: they are the shape's `body` property.

use std::ops::Range;

use craft_cloud_kit::{Attrs, RichText, Value};
use deckcraft_model::Presentation;
use deckcraft_model::text::{Paragraph, Run, RunKind, RunProps, TextBody};
use serde_json::{Map, Value as Json};

use super::ids::Ids;
use super::json::{object, to_live, to_local};

/// The mark of a run's kind (fields, breaks, equations); `n` tells two neighbours of one kind apart.
const KIND: &str = "kind";
/// The paragraph marks: the level, `p.<field>` for each paragraph property, the defaults and the end mark's.
const LEVEL: &str = "level";
const DEF: &str = "def";
const END: &str = "end";

fn marks(obj: Map<String, Json>, prefix: &str, ids: &Ids, out: &mut Attrs) {
    let mut obj = Json::Object(obj);
    to_live(&mut obj, ids);
    let Json::Object(obj) = obj else { return };
    for (k, v) in obj {
        if v.is_null() {
            continue;
        }
        out.insert(format!("{prefix}{k}"), Value::Str(v.to_string()));
    }
}

fn json_mark<T: serde::Serialize>(v: &T, ids: &Ids) -> Value {
    let mut j = serde_json::to_value(v).unwrap_or(Json::Null);
    to_live(&mut j, ids);
    Value::Str(j.to_string())
}

/// A body's paragraphs as a story.
pub(crate) fn story(body: &TextBody, ids: &Ids) -> RichText {
    let mut out = RichText::default();
    for (i, para) in body.paragraphs.iter().enumerate() {
        if i > 0 {
            out.text.push('\n');
        }
        let mut pa = Attrs::new();
        if para.level != 0 {
            pa.insert(LEVEL.into(), Value::I64(i64::from(para.level)));
        }
        marks(object(&para.props), "p.", ids, &mut pa);
        if para.def_props != RunProps::default() {
            pa.insert(DEF.into(), json_mark(&para.def_props, ids));
        }
        if para.end_props != RunProps::default() {
            pa.insert(END.into(), json_mark(&para.end_props, ids));
        }
        out.paragraphs.push(pa);
        let mut odd = false;
        for run in &para.runs {
            let content = run.content();
            if content.is_empty() {
                continue;
            }
            let start = out.text.len();
            // A `\n` inside a run would end the paragraph: kept as a line break's character.
            out.text.push_str(&content.replace('\n', "\u{b}"));
            let mut a = Attrs::new();
            marks(object(&run.props), "", ids, &mut a);
            if run.kind != RunKind::Text {
                let mut k = object(&run.kind);
                odd = !odd;
                k.insert("n".into(), Json::from(u8::from(odd)));
                let mut j = Json::Object(k);
                to_live(&mut j, ids);
                a.insert(KIND.into(), Value::Str(j.to_string()));
            }
            if !a.is_empty() {
                out.runs.push((start..out.text.len(), a));
            }
        }
    }
    if out.paragraphs.is_empty() {
        out.paragraphs.push(Attrs::new());
    }
    out
}

fn decode<T: serde::de::DeserializeOwned + Default>(obj: Map<String, Json>, ids: &mut Ids, p: &mut Presentation) -> T {
    let mut j = Json::Object(obj);
    to_local(&mut j, ids, p);
    serde_json::from_value(j).unwrap_or_default()
}

fn mark_json(v: &Value) -> Option<Json> {
    serde_json::from_str(v.as_str()?).ok()
}

/// A story's paragraphs as a body's (the body properties and list style are the caller's).
pub(crate) fn paragraphs(rt: &RichText, ids: &mut Ids, p: &mut Presentation) -> Vec<Paragraph> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut pieces: Vec<Range<usize>> = Vec::new();
    for (i, b) in rt.text.bytes().enumerate() {
        if b == b'\n' {
            pieces.push(start..i);
            start = i + 1;
        }
    }
    pieces.push(start..rt.text.len());
    let empty = Attrs::new();
    for (k, range) in pieces.into_iter().enumerate() {
        let pa = rt.paragraphs.get(k).unwrap_or(&empty);
        let mut para =
            Paragraph { level: pa.get(LEVEL).and_then(Value::as_i64).and_then(|l| u8::try_from(l).ok()).unwrap_or(0), ..Paragraph::default() };
        let mut props = Map::new();
        for (key, v) in pa {
            if let Some(field) = key.strip_prefix("p.")
                && let Some(j) = mark_json(v)
            {
                props.insert(field.to_owned(), j);
            }
        }
        para.props = decode(props, ids, p);
        if let Some(Json::Object(o)) = pa.get(DEF).and_then(mark_json) {
            para.def_props = decode(o, ids, p);
        }
        if let Some(Json::Object(o)) = pa.get(END).and_then(mark_json) {
            para.end_props = decode(o, ids, p);
        }
        // The runs in this paragraph, with the gaps no run covers.
        let mut at = range.start;
        let mut runs: Vec<(Range<usize>, Option<&Attrs>)> = Vec::new();
        for (r, a) in &rt.runs {
            let (s, e) = (r.start.max(range.start), r.end.min(range.end));
            if s >= e {
                continue;
            }
            if s > at {
                runs.push((at..s, None));
            }
            runs.push((s..e, Some(a)));
            at = e;
        }
        if at < range.end {
            runs.push((at..range.end, None));
        }
        for (r, a) in runs {
            let text = rt.text.get(r).unwrap_or_default();
            let (props, kind) = match a {
                None => (RunProps::default(), RunKind::Text),
                Some(a) => {
                    let mut obj = Map::new();
                    let mut kind = RunKind::Text;
                    for (key, v) in a {
                        if key == KIND {
                            if let Some(Json::Object(mut o)) = mark_json(v) {
                                o.remove("n");
                                let mut j = Json::Object(o);
                                to_local(&mut j, ids, p);
                                kind = serde_json::from_value(j).unwrap_or(RunKind::Text);
                            }
                        } else if let Some(j) = mark_json(v) {
                            obj.insert(key.clone(), j);
                        }
                    }
                    (decode(obj, ids, p), kind)
                }
            };
            match kind {
                // One break per character.
                RunKind::Break => {
                    for _ in text.chars() {
                        para.runs.push(Run { text: String::new(), props: props.clone(), kind: RunKind::Break });
                    }
                }
                kind => para.runs.push(Run { text: text.to_owned(), props, kind }),
            }
        }
        out.push(para);
    }
    out
}
