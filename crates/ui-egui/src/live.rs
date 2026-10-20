//! Craft Cloud's piece of the window (the `live` feature): the live badge in the status bar, the account line and the
//! shared recent files in Backstage, the banner over a live presentation (changed outside Craft, read-only), and the
//! other machines' selections outlined on the slide. The engine does the rest (`deckcraft_engine::live`).

use std::sync::Arc;

use craft_cloud_egui::CloudWatch;
use deckcraft_engine::live::craft_cloud_kit::Agent;
use deckcraft_engine::live::session::Banner;
use egui::{Color32, Pos2, RichText, Stroke, Ui};

use crate::SlideApp;
use crate::canvas::Xf;
use crate::theme::{self, Tokens};

/// The agent's state and this app's shared recents, kept current off the UI thread.
pub struct Cloud {
    watch: CloudWatch,
    /// The synced folder, when the agent said.
    folder: Option<String>,
}

/// Colours of the other machines' outlines (by their slot), on light and dark slides alike.
const PEERS: [Color32; 8] = [
    Color32::from_rgb(0xE0, 0x4F, 0x8B),
    Color32::from_rgb(0x2E, 0x9E, 0x6A),
    Color32::from_rgb(0xE8, 0x8A, 0x1A),
    Color32::from_rgb(0x7A, 0x5C, 0xE0),
    Color32::from_rgb(0x1F, 0x9C, 0xC7),
    Color32::from_rgb(0xC2, 0x3B, 0x3B),
    Color32::from_rgb(0x8A, 0x9A, 0x1C),
    Color32::from_rgb(0x5A, 0x6B, 0x80),
];

/// The outline colour of the machine in presence slot `slot`.
pub(crate) fn peer_colour(slot: usize) -> Color32 {
    PEERS.get(slot % PEERS.len()).copied().unwrap_or(Color32::GRAY)
}

impl SlideApp {
    /// Opens `.deckcraft` files in the Craft Cloud folder live from now on, and shows Craft Cloud's piece of the
    /// window. The desktop app calls it once.
    pub fn enable_live(&mut self, ctx: &egui::Context) {
        self.enable_live_with(Agent::local(), ctx);
    }

    /// [`SlideApp::enable_live`] with the agent `agent` finds (a fake one in tests).
    pub fn enable_live_with(&mut self, agent: Agent, ctx: &egui::Context) {
        let c = ctx.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || c.request_repaint());
        self.session.enable_live(agent.clone(), Some(wake));
        let folder = agent.client().ok().and_then(|c| c.status().ok()).and_then(|s| s.folder);
        let c = ctx.clone();
        let a = agent.clone();
        let watch = CloudWatch::start("deckcraft", move || a.client(), move || c.request_repaint());
        self.cloud = Some(Cloud { watch, folder });
    }

    /// Each frame: the live presentations' remote changes, media, presence and autosave.
    pub(crate) fn poll_live(&mut self, ctx: &egui::Context) {
        if self.session.poll_live() {
            ctx.request_repaint();
        }
        if self.session.has_live() {
            // Autosave and presence run on time, not only on input.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    /// Notes `path` in the shared recents when it is in the Craft Cloud folder.
    pub(crate) fn note_live_recent(&mut self, path: &str, saved: bool) {
        let agent = self.session.live_agent();
        let Some(cloud) = &mut self.cloud else { return };
        if cloud.folder.is_none() {
            cloud.folder = agent.and_then(|a| a.client().ok()).and_then(|c| c.status().ok()).and_then(|s| s.folder);
        }
        let Some(folder) = &cloud.folder else { return };
        if let Some(rel) = deckcraft_engine::live::session::relative_path(folder, path) {
            cloud.watch.add_recent(&rel, saved);
        }
    }
}

/// The live badge at the right of the status bar (Live, Waiting, Offline, Read-only…), with what went wrong on hover.
pub fn badge(app: &SlideApp, ui: &mut Ui) {
    if let Some(status) = app.session.live_status() {
        // In the status bar's type.
        ui.style_mut().override_font_id = Some(theme::font(12.0));
        craft_cloud_egui::badge(ui, &status);
        // What the media waits for ("waiting for media from the laptop": its upload hasn't come; the file saves once
        // it is here).
        if let Some(waiting) = app.session.live_media_waiting() {
            ui.label(RichText::new(waiting).weak()).on_hover_text("The presentation saves once every picture and video is here.");
        }
        if status.gaps > 0 {
            ui.label(RichText::new(format!("caught up {}×", status.gaps)).weak())
                .on_hover_text("Changes from another computer that came only part of the way; nothing of them was applied, and this window caught up with Craft Cloud.");
        }
        ui.style_mut().override_font_id = None;
    }
}

/// The longest closing the window waits for media not sent to Craft Cloud yet, in seconds.
pub const CLOSE_WAIT: f64 = 15.0;

/// Closing the window while media this computer added hasn't gone to Craft Cloud yet (the other computers can't have
/// it until it has): it waits, at most [`CLOSE_WAIT`], saying so, with "Close now", and closes once the media is sent.
pub(crate) fn close_after_uploads(app: &mut SlideApp, ctx: &egui::Context, now: f64) {
    let requested = ctx.input(|i| i.viewport().close_requested());
    match app.closing {
        // (The question about unsaved changes comes first.)
        None if requested && (app.quit_confirmed || !app.session.documents().iter().any(|d| d.is_dirty())) => {
            if app.session.live_unsent_media() > 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                app.closing = Some((now, false));
            }
        }
        Some((since, false)) => {
            let unsent = app.session.live_unsent_media();
            let close = |app: &mut SlideApp| {
                app.closing = Some((since, true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            };
            // Sent, or waited long enough, or closed again: it closes.
            if unsent == 0 || now - since >= CLOSE_WAIT || requested {
                return close(app);
            }
            let left = (CLOSE_WAIT - (now - since)).ceil();
            let mut now_please = false;
            egui::Window::new("Sending media").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0)).show(
                ctx,
                |ui| {
                    ui.label(format!(
                        "Sending {unsent} picture{} or video{} to your other computers through Craft Cloud…",
                        if unsent == 1 { "" } else { "s" },
                        if unsent == 1 { "" } else { "s" }
                    ));
                    ui.label(RichText::new(format!("DeckCraft closes once they're sent (at most {left:.0} s).")).weak());
                    now_please = ui.button("Close now").clicked();
                },
            );
            if now_please {
                return close(app);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        _ => {}
    }
}

/// The banner over a live presentation, when it has one.
pub fn banner(app: &mut SlideApp, ui: &mut Ui) {
    let Some(b) = app.session.live_banner() else { return };
    let t = Tokens::get(ui.ctx());
    egui::Frame::NONE.fill(t.card).stroke(Stroke::new(1.0, t.card_border)).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
        ui.horizontal(|ui| match b {
            Banner::ChangedOutside => {
                ui.label(
                    RichText::new(
                        "This file was changed outside Craft. Bringing it in replaces the live edits made since it was written, on every computer (Undo brings them back).",
                    )
                    .font(theme::font(12.0))
                    .color(t.text),
                );
                if ui.button("Bring it in").clicked() {
                    let r = app.run("file.revert", serde_json::json!({}));
                    if let Err(e) = r {
                        app.set_status(e);
                    }
                }
                if ui.button("Keep the live document").clicked()
                    && let Err(e) = app.run("file.liveKeep", serde_json::json!({}))
                {
                    app.set_status(e);
                }
            }
            Banner::ReadOnly(text) => {
                ui.label(RichText::new(text).font(theme::font(12.0)).color(t.text));
            }
        });
    });
}

/// Craft Cloud in Backstage, under Recent: the account line and the presentations open on every computer.
pub fn backstage(app: &mut SlideApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let Some(cloud) = &mut app.cloud else { return };
    ui.add_space(18.0);
    ui.label(RichText::new("Craft Cloud").font(theme::bold(16.0)).color(t.text));
    ui.add_space(4.0);
    let state = cloud.watch.state();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let entries = cloud.watch.entries().to_vec();
    let folder = cloud.folder.clone();
    let clicked = ui
        .scope(|ui| {
            // In Backstage's type and colours.
            ui.style_mut().override_font_id = Some(theme::font(13.0));
            ui.visuals_mut().override_text_color = Some(t.text_dim);
            craft_cloud_egui::account_line(ui, &state);
            ui.add_space(6.0);
            if entries.is_empty() {
                ui.label("Presentations you open in the Craft Cloud folder show here on every computer.");
                return None;
            }
            ui.visuals_mut().override_text_color = Some(t.text);
            craft_cloud_egui::recents(ui, &entries, now)
        })
        .inner;
    if let Some(rel) = clicked
        && let Some(folder) = folder
    {
        let path = std::path::Path::new(&folder).join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        app.open_path_in_background(&path.to_string_lossy());
    }
}

/// One other machine's selection on the slide shown: its colour, each selected shape's corners on screen, and where
/// its name goes (above the first shape's first corner).
#[derive(Debug)]
pub(crate) struct Mark {
    pub(crate) colour: Color32,
    pub(crate) outlines: Vec<[Pos2; 4]>,
    pub(crate) tag: Option<(Pos2, String)>,
}

/// The other machines' selections on the slide shown, placed by `xf`.
pub(crate) fn presence_marks(app: &SlideApp, xf: Xf) -> Vec<Mark> {
    let Some(st) = app.session.active() else { return vec![] };
    if st.selection.target != deckcraft_engine::Target::Slides {
        return vec![];
    }
    let Some(slide) = st.current_slide() else { return vec![] };
    let mut out = vec![];
    for p in app.session.live_presence() {
        if p.slide != Some(slide.id) {
            continue;
        }
        let mut outlines = vec![];
        for id in &p.shapes {
            let Some(sh) = st.shape(*id) else { continue };
            let x = deckcraft_engine::cmd::xfrm_of(&st.doc, &st.selection, sh);
            let a = x.affine();
            outlines.push([(0.0, 0.0), (x.w, 0.0), (x.w, x.h), (0.0, x.h)].map(|(u, v)| xf.to_screen(a * deckcraft_geom::Point::new(u, v))));
        }
        let tag = outlines.first().filter(|_| !p.machine.is_empty()).map(|o| (o[0], p.machine.clone()));
        out.push(Mark { colour: peer_colour(p.slot), outlines, tag });
    }
    out
}

/// The other machines' selections on the slide shown: an outline in each machine's colour, with its name.
pub fn presence(app: &SlideApp, painter: &egui::Painter, xf: Xf) {
    for m in presence_marks(app, xf) {
        for pts in &m.outlines {
            for i in 0..4 {
                painter.line_segment([pts[i], pts[(i + 1) % 4]], Stroke::new(2.0, m.colour));
            }
        }
        if let Some((at, name)) = m.tag {
            let galley = painter.layout_no_wrap(name, theme::font(11.0), Color32::WHITE);
            let r = egui::Rect::from_min_size(at - egui::vec2(0.0, galley.size().y + 4.0), galley.size() + egui::vec2(8.0, 4.0));
            painter.rect_filled(r, 3.0, m.colour);
            painter.galley(r.min + egui::vec2(4.0, 2.0), galley, Color32::WHITE);
        }
    }
}
