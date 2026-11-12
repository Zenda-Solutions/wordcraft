//! Zenda's build of WordCraft for Work (the `zenda` cargo feature).
//!
//! Work (Zenda's desktop app) runs WordCraft with `--control <port>` and talks to it over the
//! control channel. The channel is request/reply, so the app cannot call Work: what the person asks
//! of Work from inside WordCraft (the Theo button, Share) goes into an [`Outbox`], and Work drains
//! it with the `work.events` control method. Accounts, sending and Theo live in Work, never here.
//!
//! Every patch checks [`ON`] instead of `#[cfg]`, so both builds compile the same code and the
//! upstream tests keep running on the stock behaviour. Patches never change the document format.
//!
//! | Patch | What |
//! |---|---|
//! | `zenda-strip` | No Discord button, menu entry, keytip or link; no community line in MCP. |
//! | `zenda-theo-button` | A «Theo» button in the title bar: queues `work.theo {context}`. |
//! | `zenda-share` | The Share button queues `work.share {path, dirty, name}` instead of opening Export. |
//! | `zenda-brand` | About says it is Zenda's build based on WordCraft by the ArtCraft team, never an ArtCraft product. |

use serde_json::{Value, json};

/// True in Zenda's build for Work.
pub const ON: bool = cfg!(feature = "zenda");

/// Events kept for Work when it does not drain them; older ones are dropped first.
const OUTBOX_MAX: usize = 32;

/// What the person asked of Work, oldest first, until Work drains it (`work.events`).
#[derive(Default, Debug)]
pub struct Outbox {
    events: Vec<Value>,
    seq: u64,
}

impl Outbox {
    /// Queue `event` (`work.theo`, `work.share`) with its params.
    pub fn push(&mut self, event: &str, params: Value) {
        self.seq += 1;
        if self.events.len() == OUTBOX_MAX {
            self.events.remove(0);
        }
        self.events.push(json!({"seq": self.seq, "event": event, "params": params}));
    }

    /// Every queued event, oldest first; the outbox is empty afterwards.
    pub fn drain(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// `work.theo`'s params: where the person is, never document text (Work reads that itself over
/// the channel when the person asks Theo something).
pub fn theo_context(app: &crate::WordApp) -> Value {
    json!({
        "from": "button",
        "tab": app.ui.tab,
        "hasDocument": app.has_document(),
        "selection": app.has_document() && !app.session.sel.is_collapsed(),
    })
}

/// `work.share`'s params: the saved file Work should send. `path` is null for a document never
/// saved, and `dirty` says the file on disk is behind the window; Work asks to save first.
pub fn share_params(app: &crate::WordApp) -> Value {
    let path = app.session.path.as_ref().map(|p| p.to_string_lossy().to_string());
    let name =
        app.session.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).or_else(|| app.session.file_name.clone());
    json!({"path": path, "dirty": app.session.dirty, "name": name})
}

/// Work's mascot and orange brand button, shared by the curated app patches.
pub fn theo_button(ui: &mut egui::Ui, label: &str, hint: &str) -> egui::Response {
    let key = egui::Id::new("zenda.theo.texture");
    let texture = ui.ctx().data_mut(|d| d.get_temp::<egui::TextureHandle>(key)).or_else(|| {
        let image = image::load_from_memory(include_bytes!("../../../assets/zenda/theo-icon-64.png")).ok()?.to_rgba8();
        let color = egui::ColorImage::from_rgba_unmultiplied([image.width() as usize, image.height() as usize], image.as_raw());
        let texture = ui.ctx().load_texture("Theo mascot", color, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|d| d.insert_temp(key, texture.clone()));
        Some(texture)
    });
    let text = egui::RichText::new("Theo").color(egui::Color32::BLACK).size(11.5);
    let button = if let Some(texture) = texture {
        egui::Button::image_and_text(egui::Image::new((texture.id(), egui::vec2(18.0, 18.0))), text)
    } else {
        egui::Button::new(text)
    };
    let response = ui.add(button.fill(egui::Color32::from_rgb(234, 108, 30)).corner_radius(11.0).min_size(egui::vec2(74.0, 24.0)));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response.on_hover_text(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outbox_drains_oldest_first_and_empties() {
        let mut o = Outbox::default();
        o.push("work.theo", json!({"from": "button"}));
        o.push("work.share", json!({"path": null}));
        let got = o.drain();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["event"], "work.theo");
        assert_eq!(got[1]["event"], "work.share");
        assert_eq!(got[1]["seq"], 2);
        assert!(o.is_empty());
    }

    #[test]
    fn work_drains_the_outbox_over_the_control_channel_only_in_the_zenda_build() {
        use crate::control::{ControlRequest, Outcome, handle};
        let mut app = crate::WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), crate::Services::default());
        app.zenda.push("work.theo", theo_context(&app));
        let ctx = egui::Context::default();
        let (req, _) = ControlRequest::new("work.events", json!({}));
        let Outcome::Done(reply) = handle(&mut app, &ctx, &req) else { panic!("work.events replies at once") };
        if ON {
            assert_eq!(reply["ok"], true, "{reply}");
            assert_eq!(reply["result"]["events"][0]["event"], "work.theo");
            assert!(app.zenda.is_empty());
        } else {
            assert_eq!(reply["ok"], false, "the stock build has no work.events: {reply}");
        }
    }

    #[test]
    fn the_zenda_build_opens_no_community_link() {
        let mut app = crate::WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), crate::Services::default());
        let _ = app.run("ui.discord", json!({}));
        assert_eq!(app.canvas.open_url.is_some(), !ON, "zenda-strip: ui.discord opens nothing in Work");
    }

    #[test]
    fn the_theo_button_queues_work_theo_in_the_zenda_build_only() {
        use egui_kittest::kittest::Queryable;
        let a = crate::WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::from_text("Hola")), crate::Services::default());
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1440.0, 900.0)).build_ui_state(
            |ui, app: &mut crate::WordApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            a,
        );
        h.run_steps(4);
        if !ON {
            assert!(h.query_by_label("Ask Theo for help").is_none(), "the stock build has no Theo button");
            return;
        }
        h.get_by_label("Ask Theo for help").click();
        h.run_steps(2);
        let events = h.state_mut().zenda.drain();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["event"], "work.theo");
        assert_eq!(events[0]["params"]["from"], "button");
        assert_eq!(events[0]["params"]["hasDocument"], true);
    }

    #[test]
    fn share_hands_the_saved_file_to_work_in_the_zenda_build() {
        let mut app = crate::WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::from_text("Hola")), crate::Services::default());
        app.session.path = Some(std::path::PathBuf::from("C:/Docs/Z Work/carta.docx"));
        app.session.dirty = false;
        let reply = app.run("file.share", json!({})).unwrap();
        let events = app.zenda.drain();
        if !ON {
            assert!(events.is_empty(), "the stock build keeps its own Share");
            return;
        }
        assert_eq!(reply["work"], "share");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "work.share");
        assert_eq!(events[0]["params"]["path"], "C:/Docs/Z Work/carta.docx");
        assert_eq!(events[0]["params"]["name"], "carta.docx");
        assert_eq!(events[0]["params"]["dirty"], false);
    }

    #[test]
    fn the_outbox_keeps_only_the_newest_events() {
        let mut o = Outbox::default();
        for _ in 0..(OUTBOX_MAX + 5) {
            o.push("work.theo", json!({}));
        }
        let got = o.drain();
        assert_eq!(got.len(), OUTBOX_MAX);
        assert_eq!(got[0]["seq"], 6);
    }
    #[test]
    fn theo_mascot_preview() {
        let Some(path) = std::env::var_os("ZENDA_THEO_PREVIEW") else { return };
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(160.0, 50.0)).with_pixels_per_point(3.0).wgpu().build_ui(|ui| {
            let _ = theo_button(ui, "Ask Theo for help", "Theo");
        });
        h.run_steps(2);
        h.render().unwrap().save(path).unwrap();
    }
}
