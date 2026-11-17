//! Theo's chat panel inside the tool (egui), and Theo's cursor.
//!
//! The tool implements [`Host`] over its own document model and command API; the panel never moves the
//! real mouse. Each frame the tool calls [`Chat::pump`] (runs what the chat thread asks), and, while the
//! panel shows, [`Chat::ui`] inside a side panel and [`Chat::paint_cursor`]. When [`Chat::wants_key`] is
//! true it asks Work for a key (Texto queues `work.chatKey`); Work's answer goes to [`Chat::offer_key`].

use crate::agent::{self, Ask, READ, Run, ToUi};
use crate::dash::{CHAT_URL, ChatError, Clock, KEY_WAIT, SharedKey, Transport};
use crate::store::{self, Conversation};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

/// What the tool lends the chat.
pub trait Host {
    /// Theo's instructions for this app: who he is here, its menus in Spanish, examples.
    fn skill(&self) -> String;
    /// The actions, as OpenAI function definitions.
    fn actions(&self) -> Vec<Value>;
    /// The document now, without its full text (structure, selection, caret).
    fn read(&mut self) -> Value;
    /// Runs one action through the app's command API.
    fn act(&mut self, name: &str, args: &Value) -> Result<Value, String>;
    /// The question to ask the person first, for an action that deletes or replaces their work. It may
    /// read the document to decide (is that cell empty?).
    fn confirm(&mut self, name: &str, args: &Value) -> Option<String>;
    /// Where the action lands on screen, for Theo's cursor.
    fn target(&self, name: &str, args: &Value) -> Option<egui::Rect>;
}

struct Pending {
    name: String,
    args: Value,
    reply: Sender<Value>,
    question: String,
}

pub struct Chat {
    tool: String,
    tool_name: String,
    url: String,
    keys: Arc<SharedKey>,
    transport: Arc<dyn Transport>,
    /// The panel shows.
    pub open: bool,
    file: Option<PathBuf>,
    conv: Conversation,
    run: Option<Run>,
    live: String,
    error: Option<ChatError>,
    pending: Option<Pending>,
    cursor: Option<(egui::Pos2, egui::Pos2)>,
    zwork_exe: Option<PathBuf>,
    /// How long a send waits for Work to hand over a key.
    key_wait: std::time::Duration,
}

impl Chat {
    pub fn new(tool: &str, tool_name: &str, transport: Arc<dyn Transport>, clock: Arc<dyn Clock>) -> Self {
        Self {
            tool: tool.into(),
            tool_name: tool_name.into(),
            url: std::env::var("ZWORK_CHAT_URL").ok().filter(|u| u.starts_with("https://") || u.starts_with("http://127.0.0.1")).unwrap_or(CHAT_URL.into()),
            keys: Arc::new(SharedKey::new(clock)),
            transport,
            open: false,
            file: None,
            conv: Conversation::default(),
            run: None,
            live: String::new(),
            error: None,
            pending: None,
            cursor: None,
            zwork_exe: work_exe(std::env::var_os("ZWORK_EXE")),
            key_wait: KEY_WAIT,
        }
    }

    pub fn keys(&self) -> &Arc<SharedKey> {
        &self.keys
    }

    /// Work sent a key (`zenda.chat.key`).
    pub fn offer_key(&self, token: &str, expires_in_s: u64) {
        self.keys.offer(token, expires_in_s);
        // A key back from Work clears «Abrí Work»; the person retries with the button, nothing is lost.
    }

    /// Whether to ask Work for a key now (first open, half life, after a 401, with backoff).
    pub fn wants_key(&self) -> bool {
        self.keys.should_ask()
    }

    pub fn busy(&self) -> bool {
        self.run.is_some()
    }

    /// Shows the conversation of `window` + `document`, saving the one on screen first.
    pub fn bind(&mut self, profile: &Path, window: &str, document: &str) {
        let file = store::path(profile, window, document);
        if self.file.as_ref() == Some(&file) {
            return;
        }
        self.save();
        self.conv = store::load(&file);
        self.file = Some(file);
    }

    fn save(&self) {
        if let Some(f) = &self.file {
            let _ = store::save(f, &self.conv);
        }
    }

    /// The visible messages: the person's and Theo's words (not tool steps).
    pub fn transcript(&self) -> Vec<(bool, String)> {
        self.conv
            .messages
            .iter()
            .filter_map(|m| {
                let text = m["content"].as_str().filter(|t| !t.trim().is_empty())?;
                match m["role"].as_str()? {
                    "user" => Some((true, text.to_string())),
                    "assistant" => Some((false, text.to_string())),
                    _ => None,
                }
            })
            .collect()
    }

    pub fn draft_mut(&mut self) -> &mut String {
        &mut self.conv.draft
    }

    pub fn error(&self) -> Option<&ChatError> {
        self.error.as_ref()
    }

    /// Sends the draft. The message is kept in the conversation (and saved) before anything goes out.
    pub fn send(&mut self, host: &mut dyn Host) {
        let text = self.conv.draft.trim().to_string();
        if text.is_empty() || self.run.is_some() {
            return;
        }
        self.conv.draft.clear();
        self.conv.messages.push(json!({"role": "user", "content": text}));
        self.save();
        self.start(host);
    }

    /// Sends the same conversation again after a failure, without retyping.
    pub fn retry(&mut self, host: &mut dyn Host) {
        if self.run.is_none() && self.conv.messages.last().is_some_and(|m| m["role"] != "assistant") {
            self.start(host);
        }
    }

    fn start(&mut self, host: &mut dyn Host) {
        self.error = None;
        self.live.clear();
        let ask = Ask {
            url: self.url.clone(),
            tool: self.tool.clone(),
            system: host.skill(),
            messages: self.conv.messages.clone(),
            actions: host.actions(),
            key_wait: self.key_wait,
        };
        self.run = Some(agent::start(ask, self.transport.clone(), self.keys.clone()));
    }

    /// Frenar: Theo stops before his next step.
    pub fn stop(&mut self) {
        if let Some(run) = &self.run {
            run.stop.store(true, Ordering::SeqCst);
        }
        if let Some(p) = self.pending.take() {
            let _ = p.reply.send(json!({"ok": false, "error": "La persona tocó Frenar."}));
        }
    }

    /// Runs what the chat thread asks. Call every frame.
    pub fn pump(&mut self, host: &mut dyn Host) {
        while self.pending.is_none() {
            let Some(Ok(msg)) = self.run.as_ref().map(|r| r.rx.try_recv()) else { break };
            match msg {
                ToUi::Text(t) => self.live = t,
                ToUi::Act { name, arguments, reply } => {
                    if name == READ {
                        let _ = reply.send(json!({"ok": true, "result": host.read()}));
                    } else if let Some(question) = host.confirm(&name, &arguments) {
                        self.pending = Some(Pending { name, args: arguments, reply, question });
                    } else {
                        self.act(host, &name, &arguments, &reply);
                    }
                }
                ToUi::Done { messages, result } => {
                    self.conv.messages = messages;
                    self.live.clear();
                    if let Err(e) = result {
                        self.error = Some(e);
                    }
                    self.run = None;
                    self.save();
                    return;
                }
            }
        }
    }

    fn act(&mut self, host: &mut dyn Host, name: &str, args: &Value, reply: &Sender<Value>) {
        if let Some(rect) = host.target(name, args) {
            let from = self.cursor.map_or(rect.center(), |(at, _)| at);
            self.cursor = Some((from, rect.center()));
        }
        let result = match host.act(name, args) {
            Ok(v) => json!({"ok": true, "result": v}),
            Err(e) => json!({"ok": false, "error": e}),
        };
        let _ = reply.send(result);
    }

    /// The person answered the confirmation.
    pub fn answer(&mut self, host: &mut dyn Host, yes: bool) {
        let Some(p) = self.pending.take() else { return };
        if yes {
            self.act(host, &p.name, &p.args, &p.reply);
        } else {
            let _ = p.reply.send(json!({"ok": false, "error": "La persona no lo confirmó."}));
        }
    }

    pub fn pending_question(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.question.as_str())
    }

    /// The panel's contents, for a right side panel about 320 points wide.
    pub fn ui(&mut self, ui: &mut egui::Ui, host: &mut dyn Host) {
        let orange = egui::Color32::from_rgb(234, 108, 30);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Theo").strong().color(orange));
            ui.label(egui::RichText::new(format!("en {}", self.tool_name)).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").on_hover_text("Cerrar el chat").clicked() {
                    self.open = false;
                }
            });
        });
        ui.separator();
        let input_height = 96.0;
        egui::ScrollArea::vertical().stick_to_bottom(true).max_height((ui.available_height() - input_height).max(80.0)).show(ui, |ui| {
            if self.conv.messages.is_empty() {
                ui.label(format!("Estás en {}. ¿En qué te ayudo?", self.tool_name));
            }
            for (mine, text) in self.transcript() {
                let frame = egui::Frame::new().inner_margin(8.0).corner_radius(10.0).fill(if mine {
                    ui.visuals().faint_bg_color
                } else {
                    egui::Color32::from_rgb(255, 243, 230)
                });
                frame.show(ui, |ui| {
                    ui.label(egui::RichText::new(text).color(egui::Color32::BLACK));
                });
                ui.add_space(4.0);
            }
            if self.run.is_some() {
                ui.label(if self.live.is_empty() { "Theo está pensando…".to_string() } else { self.live.clone() });
            }
            if let Some(q) = self.pending_question().map(str::to_string) {
                ui.label(egui::RichText::new(q).strong());
                ui.horizontal(|ui| {
                    if ui.button("Sí, hacelo").clicked() {
                        self.answer(host, true);
                    }
                    if ui.button("No").clicked() {
                        self.answer(host, false);
                    }
                });
            }
            if let Some(e) = self.error.clone() {
                ui.colored_label(egui::Color32::from_rgb(180, 35, 24), e.text());
                ui.horizontal(|ui| {
                    if e == ChatError::WorkGone && self.zwork_exe.is_some() && ui.button("Abrir Work").clicked() {
                        if let Some(exe) = &self.zwork_exe {
                            let _ = std::process::Command::new(exe).arg("abrir").spawn();
                        }
                    }
                    if ui.button("Reintentar").clicked() {
                        self.retry(host);
                    }
                });
            }
        });
        ui.separator();
        let busy = self.run.is_some();
        let edit = ui.add(egui::TextEdit::multiline(&mut self.conv.draft).hint_text("Escribile a Theo").desired_rows(2).desired_width(f32::INFINITY));
        let enter = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        ui.horizontal(|ui| {
            if busy {
                if ui.button("Frenar").clicked() {
                    self.stop();
                }
            } else if ui.button("Enviar").clicked() || enter {
                self.send(host);
            }
        });
        if busy {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Theo's cursor over the app, gliding to where he acts. Only a picture: the real mouse never moves.
    pub fn paint_cursor(&mut self, ctx: &egui::Context) {
        let Some((at, to)) = self.cursor else { return };
        let next = at + (to - at) * 0.25;
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("zenda.theo.cursor")));
        let orange = egui::Color32::from_rgb(234, 108, 30);
        painter.circle_filled(next, 7.0, orange);
        painter.text(next + egui::vec2(10.0, 6.0), egui::Align2::LEFT_TOP, "Theo", egui::FontId::proportional(11.0), orange);
        if (to - next).length() > 0.5 {
            self.cursor = Some((next, to));
            ctx.request_repaint();
        } else if self.run.is_none() {
            self.cursor = None;
        } else {
            self.cursor = Some((to, to));
        }
    }
}

/// The `zwork.exe` that started the tool: an absolute path to a file named `zwork.exe`.
fn work_exe(var: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(var?);
    let named = path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("zwork.exe"));
    (path.is_absolute() && named && path.is_file()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dash::tests::{FakeClock, Scripted};

    struct Doc {
        acted: Vec<String>,
    }

    impl Host for Doc {
        fn skill(&self) -> String {
            "Sos Theo en Texto.".into()
        }
        fn actions(&self) -> Vec<Value> {
            vec![]
        }
        fn read(&mut self) -> Value {
            json!({"blocks": 1})
        }
        fn act(&mut self, name: &str, _: &Value) -> Result<Value, String> {
            self.acted.push(name.into());
            Ok(json!({}))
        }
        fn confirm(&mut self, name: &str, _: &Value) -> Option<String> {
            (name == "text.delete").then(|| "¿Borro el texto seleccionado?".into())
        }
        fn target(&self, _: &str, _: &Value) -> Option<egui::Rect> {
            None
        }
    }

    fn wait_done(chat: &mut Chat, host: &mut Doc) {
        for _ in 0..300 {
            chat.pump(host);
            if !chat.busy() {
                return;
            }
            // Work answers key asks right away while it is open.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the question did not end");
    }

    fn chat(name: &str, statuses: &[u16], text: &str) -> (Chat, Arc<FakeClock>, PathBuf) {
        let clock = Arc::new(FakeClock::default());
        let mut c = Chat::new("texto", "Texto", Arc::new(Scripted::new(statuses, text)), clock.clone());
        let dir = std::env::temp_dir().join(format!("zenda-theo-panel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        c.key_wait = std::time::Duration::from_millis(300);
        c.bind(&dir, "w1", "carta.docx");
        (c, clock, dir)
    }

    #[test]
    fn with_work_gone_the_message_stays_and_retry_sends_it_once_a_key_arrives() {
        let (mut c, _clock, dir) = chat("gone", &[], "Listo.");
        let mut doc = Doc { acted: vec![] };
        // No key yet and Work closed: the question ends with «Abrí Work…», the message is kept.
        c.url = "u".into();
        *c.draft_mut() = "¿Cómo pongo un título?".into();
        c.keys.offer("zck_a", 3600);
        c.keys.reject("zck_a");
        let started = std::time::Instant::now();
        c.send(&mut doc);
        wait_done(&mut c, &mut doc);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(c.error(), Some(&ChatError::WorkGone));
        assert_eq!(c.transcript(), [(true, "¿Cómo pongo un título?".to_string())]);
        // It is saved: a restart shows it too.
        assert_eq!(store::load(c.file.as_ref().unwrap()).messages.len(), 1);
        // Work opens and sends a key; «Reintentar» sends the same message, nothing retyped.
        c.offer_key("zck_b", 3600);
        c.retry(&mut doc);
        wait_done(&mut c, &mut doc);
        assert_eq!(c.error(), None);
        assert_eq!(c.transcript().last().unwrap(), &(false, "Listo.".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_document_keeps_its_own_conversation_and_draft() {
        let (mut c, _clock, dir) = chat("docs", &[], "x");
        *c.draft_mut() = "para la carta".into();
        c.bind(&dir, "w1", "informe.docx");
        assert_eq!(c.draft_mut(), "");
        c.bind(&dir, "w1", "carta.docx");
        assert_eq!(c.draft_mut(), "para la carta");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_destructive_action_waits_for_the_person() {
        use crate::agent::ToUi;
        let (mut c, _clock, dir) = chat("confirm", &[], "x");
        let mut doc = Doc { acted: vec![] };
        let (tx, rx) = std::sync::mpsc::channel();
        let (reply, answer) = std::sync::mpsc::channel();
        tx.send(ToUi::Act { name: "text.delete".into(), arguments: json!({}), reply }).unwrap();
        c.run = Some(Run { stop: Arc::new(std::sync::atomic::AtomicBool::new(false)), rx });
        c.pump(&mut doc);
        assert_eq!(c.pending_question(), Some("¿Borro el texto seleccionado?"));
        assert!(doc.acted.is_empty(), "nothing deleted before the OK");
        c.answer(&mut doc, false);
        assert_eq!(answer.recv().unwrap()["ok"], false);
        assert!(doc.acted.is_empty());
        let (reply, answer) = std::sync::mpsc::channel();
        tx.send(ToUi::Act { name: "text.delete".into(), arguments: json!({}), reply }).unwrap();
        c.pump(&mut doc);
        c.answer(&mut doc, true);
        assert_eq!(answer.recv().unwrap()["ok"], true);
        assert_eq!(doc.acted, ["text.delete"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn work_exe_must_be_an_absolute_zwork_exe() {
        assert!(work_exe(None).is_none());
        assert!(work_exe(Some("zwork.exe".into())).is_none());
        assert!(work_exe(Some(std::env::current_exe().unwrap().into_os_string())).is_none());
    }
}
