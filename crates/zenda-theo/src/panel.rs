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
    fn target(&mut self, name: &str, args: &Value) -> Option<egui::Rect>;
    /// Two or three things to ask Theo in this app, shown on an empty chat. Tapping one sends it.
    fn suggestions(&self) -> Vec<String> {
        Vec::new()
    }
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
            url: std::env::var("ZWORK_CHAT_URL")
                .ok()
                .filter(|u| u.starts_with("https://") || u.starts_with("http://127.0.0.1"))
                .unwrap_or(CHAT_URL.into()),
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

    /// «Nueva charla»: forgets this document's conversation (and its saved file), keeping the draft.
    pub fn new_conversation(&mut self) {
        if self.run.is_some() {
            return;
        }
        self.conv.messages.clear();
        self.error = None;
        self.live.clear();
        self.save();
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

    /// The panel's contents, for a right side panel about 320 points wide. It follows the tool's light or
    /// dark visuals: the person's messages in a tinted bubble on the right, Theo's answers as plain text
    /// with light Markdown (bold, `code`, lists), and the message box pinned to the bottom.
    pub fn ui(&mut self, ui: &mut egui::Ui, host: &mut dyn Host) {
        let busy = self.run.is_some();
        self.header(ui);
        ui.add_space(4.0);
        ui.separator();
        // The message box keeps its height from the last frame, so the conversation fills the rest.
        let box_h = ui.ctx().data(|d| d.get_temp::<f32>(ui.id().with("zenda.theo.box"))).unwrap_or(88.0);
        let list_h = (ui.available_height() - box_h - 10.0).max(80.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(list_h).stick_to_bottom(true).show(ui, |ui| {
            ui.set_min_height(list_h);
            ui.add_space(6.0);
            self.messages(ui, host);
            ui.add_space(6.0);
        });
        ui.add_space(6.0);
        let h = self.composer(ui, host);
        ui.ctx().data_mut(|d| d.insert_temp(ui.id().with("zenda.theo.box"), h));
        if busy {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            avatar(ui, 13.0);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(egui::RichText::new("Theo").strong().size(15.0));
                ui.label(egui::RichText::new(format!("Tu compañero en {}", self.tool_name)).small().weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if close_button(ui).on_hover_text("Cerrar el chat").clicked() {
                    self.open = false;
                }
                if !self.conv.messages.is_empty() && self.run.is_none() {
                    let b = egui::Button::new(egui::RichText::new("Nueva charla").small()).frame(false);
                    if ui.add(b).on_hover_text("Empezar de cero con Theo en este documento").clicked() {
                        self.new_conversation();
                    }
                }
            });
        });
    }

    fn messages(&mut self, ui: &mut egui::Ui, host: &mut dyn Host) {
        let transcript = self.transcript();
        if transcript.is_empty() && self.run.is_none() {
            self.welcome(ui, host);
        }
        let mut last_mine = None;
        for (mine, text) in transcript {
            if last_mine.is_some() {
                ui.add_space(if last_mine == Some(mine) { 4.0 } else { 12.0 });
            }
            if mine {
                user_bubble(ui, &text);
            } else {
                theo_says(ui, &text, last_mine != Some(false));
            }
            last_mine = Some(mine);
        }
        if self.run.is_some() {
            ui.add_space(12.0);
            if self.live.trim().is_empty() {
                ui.horizontal(|ui| {
                    avatar(ui, 9.0);
                    ui.label(egui::RichText::new("Theo está pensando").weak());
                    dots(ui);
                });
            } else {
                theo_says(ui, &self.live, last_mine != Some(false));
                dots(ui);
            }
        }
        if let Some(q) = self.pending_question().map(str::to_string) {
            ui.add_space(12.0);
            card(ui, ACCENT, |ui| {
                ui.label(egui::RichText::new(q).strong());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(primary("Sí, hacelo")).clicked() {
                        self.answer(host, true);
                    }
                    if ui.button("No").clicked() {
                        self.answer(host, false);
                    }
                });
            });
        }
        if let Some(e) = self.error.clone() {
            ui.add_space(12.0);
            let red = ui.visuals().error_fg_color;
            card(ui, red, |ui| {
                ui.label(e.text());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if e == ChatError::WorkGone
                        && let Some(exe) = &self.zwork_exe
                        && ui.add(primary("Abrir Work")).clicked()
                    {
                        let _ = std::process::Command::new(exe).arg("abrir").spawn();
                    }
                    if ui.button("Reintentar").clicked() {
                        self.retry(host);
                    }
                });
            });
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui, host: &mut dyn Host) {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Hola, soy Theo.").strong().size(16.0));
        ui.add_space(2.0);
        ui.label(egui::RichText::new(format!("Estás en {}. Pedime que lo haga por vos o preguntame cómo se hace.", self.tool_name)).weak());
        let ideas = host.suggestions();
        if ideas.is_empty() {
            return;
        }
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Probá con").small().weak());
        ui.add_space(2.0);
        for idea in ideas {
            let chip = egui::Button::new(egui::RichText::new(&idea).color(ui.visuals().text_color()))
                .wrap()
                .fill(ui.visuals().faint_bg_color)
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .corner_radius(8.0)
                .min_size(egui::vec2(ui.available_width(), 30.0));
            if ui.add(chip).clicked() {
                self.conv.draft = idea;
                self.send(host);
            }
            ui.add_space(2.0);
        }
    }

    /// The message box; returns its height.
    fn composer(&mut self, ui: &mut egui::Ui, host: &mut dyn Host) -> f32 {
        let busy = self.run.is_some();
        let v = ui.visuals().clone();
        let id = ui.id().with("zenda.theo.draft");
        let focused = ui.memory(|m| m.has_focus(id));
        let stroke = if focused { egui::Stroke::new(1.5, ACCENT) } else { v.widgets.noninteractive.bg_stroke };
        let frame = egui::Frame::new().fill(v.extreme_bg_color).stroke(stroke).corner_radius(10.0).inner_margin(8.0);
        let shown = frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            {
                let edit = ui.add(
                    egui::TextEdit::multiline(&mut self.conv.draft)
                        .id(id)
                        .hint_text("Escribile a Theo")
                        .frame(egui::Frame::NONE)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
                // The hint is not a name: screen readers and UI Automation need one to find the box.
                // Built from `text_edit` because egui 0.37 renamed `WidgetType::TextEdit` (CAD is on 0.37).
                let draft = self.conv.draft.clone();
                edit.widget_info(|| {
                    let mut info = egui::WidgetInfo::text_edit(true, &draft, &draft, "Escribile a Theo");
                    info.label = Some("Escribile a Theo".into());
                    info
                });
                let enter = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Enter envía · Shift+Enter, otra línea").small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if busy {
                            if ui.button("Frenar").on_hover_text("Theo para antes de su próximo paso").clicked() {
                                self.stop();
                            }
                        } else {
                            let empty = self.conv.draft.trim().is_empty();
                            let send = ui.add_enabled(!empty, primary("Enviar"));
                            if send.clicked() || (enter && !empty) {
                                self.send(host);
                            }
                        }
                    });
                });
            }
        });
        shown.response.rect.height()
    }

    /// Theo's cursor over the app, gliding to where he acts. Only a picture: the real mouse never moves.
    pub fn paint_cursor(&mut self, ctx: &egui::Context) {
        let Some((at, to)) = self.cursor else { return };
        let next = at + (to - at) * 0.25;
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("zenda.theo.cursor")));
        let orange = ACCENT;
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

/// Zenda's orange: Theo's avatar, his cursor, the main button.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(234, 108, 30);

/// Theo's round avatar with his initial.
fn avatar(ui: &mut egui::Ui, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(radius * 2.0, radius * 2.0), egui::Sense::hover());
    let p = ui.painter();
    p.circle_filled(rect.center(), radius, ACCENT);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, "T", egui::FontId::proportional(radius * 1.15), egui::Color32::WHITE);
}

/// A drawn ✕ (not a glyph: the tools' fonts may not have one).
fn close_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let style = ui.style().interact(&resp);
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let c = rect.center();
    let d = 4.5;
    let stroke = egui::Stroke::new(1.5, style.fg_stroke.color);
    ui.painter().line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], stroke);
    ui.painter().line_segment([c + egui::vec2(-d, d), c + egui::vec2(d, -d)], stroke);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Cerrar el chat"));
    resp
}

/// The main button: orange with white text.
fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_string()).color(egui::Color32::WHITE).strong())
        .fill(ACCENT)
        .corner_radius(6.0)
        .min_size(egui::vec2(64.0, 26.0))
}

/// A notice inside the conversation (a question before deleting, an error), with a coloured edge.
fn card(ui: &mut egui::Ui, edge: egui::Color32, add: impl FnOnce(&mut egui::Ui)) {
    let fill = edge.gamma_multiply(if ui.visuals().dark_mode { 0.16 } else { 0.10 });
    let r = egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, edge.gamma_multiply(0.5)))
        .corner_radius(8.0)
        .inner_margin(egui::Margin { left: 12, right: 10, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    let rect = r.response.rect;
    let bar = egui::Rect::from_min_max(rect.min + egui::vec2(1.0, 6.0), egui::pos2(rect.min.x + 4.0, rect.max.y - 6.0));
    ui.painter().rect_filled(bar, 2.0, edge);
}

/// Three dots that pulse while Theo works.
fn dots(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 14.0), egui::Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    let base = ui.visuals().weak_text_color();
    for k in 0..3 {
        let phase = (t * 5.0 - k as f32 * 0.8).sin() * 0.5 + 0.5;
        let c = rect.left_center() + egui::vec2(4.0 + k as f32 * 8.0, 0.0);
        ui.painter().circle_filled(c, 2.6, base.gamma_multiply(0.35 + 0.65 * phase));
    }
}

/// The person's message: a tinted bubble on the right, at most 85% of the panel wide.
fn user_bubble(ui: &mut egui::Ui, text: &str) {
    let max = (ui.available_width() * 0.85).max(120.0);
    let fill = if ui.visuals().dark_mode { egui::Color32::from_gray(58) } else { egui::Color32::from_rgb(253, 236, 222) };
    let color = ui.visuals().strong_text_color();
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        egui::Frame::new()
            .fill(fill)
            .corner_radius(egui::CornerRadius { nw: 12, ne: 12, sw: 12, se: 4 })
            .inner_margin(egui::Margin::symmetric(10, 7))
            .show(ui, |ui| {
                ui.set_max_width(max);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).color(color)).wrap());
                });
            });
    });
}

/// Theo's answer: plain text under his name, with light Markdown.
fn theo_says(ui: &mut egui::Ui, text: &str, with_name: bool) {
    if with_name {
        ui.horizontal(|ui| {
            avatar(ui, 9.0);
            ui.label(egui::RichText::new("Theo").small().strong());
        });
        ui.add_space(2.0);
    }
    markdown(ui, text);
}

/// The Markdown models write in a chat: paragraphs, `#` headings, `-`/`*`/`1.` lists, `**bold**` and
/// `` `code` ``. Anything else shows as written.
pub fn markdown(ui: &mut egui::Ui, text: &str) {
    let mut gap = false;
    let mut in_fence = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            let code = egui::RichText::new(line).monospace().background_color(ui.visuals().code_bg_color);
            ui.add(egui::Label::new(code).wrap());
            continue;
        }
        if t.is_empty() {
            gap = true;
            continue;
        }
        if gap {
            ui.add_space(6.0);
            gap = false;
        }
        let heading = t.trim_start_matches('#');
        if heading.len() < t.len() && heading.starts_with(' ') {
            ui.add(egui::Label::new(inline(ui, heading.trim(), true)).wrap());
            continue;
        }
        let indent = (line.len() - line.trim_start().len()) as f32 * 4.0;
        let bullet = ["- ", "* ", "• "].iter().find_map(|b| t.strip_prefix(b)).map(|rest| ("•".to_string(), rest));
        let numbered = || {
            let digits = t.chars().take_while(char::is_ascii_digit).count();
            (digits > 0 && t[digits..].starts_with(". ")).then(|| (t[..digits + 1].to_string(), &t[digits + 2..]))
        };
        match bullet.or_else(numbered) {
            Some((mark, rest)) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(4.0 + indent);
                    let w = if mark == "•" { 10.0 } else { 18.0 };
                    ui.add_sized([w, 0.0], egui::Label::new(egui::RichText::new(mark).color(ui.visuals().weak_text_color())));
                    ui.add(egui::Label::new(inline(ui, rest, false)).wrap());
                });
            }
            None => {
                ui.add(egui::Label::new(inline(ui, t, false)).wrap());
            }
        }
    }
}

/// Reading colour for Theo's answers: the tools' default text is too faint for long paragraphs.
fn body_color(v: &egui::Visuals) -> egui::Color32 {
    if v.dark_mode { egui::Color32::from_gray(200) } else { egui::Color32::from_gray(64) }
}

/// One line of text with `**bold**` and `` `code` `` turned into styles.
fn inline(ui: &egui::Ui, text: &str, strong: bool) -> egui::text::LayoutJob {
    let v = ui.visuals();
    let body = egui::TextStyle::Body.resolve(ui.style());
    let normal = if strong { v.strong_text_color() } else { body_color(v) };
    let mut job = egui::text::LayoutJob::default();
    let (mut bold, mut code) = (false, false);
    let mut rest = text;
    while !rest.is_empty() {
        let next = [rest.find("**"), rest.find('`')].into_iter().flatten().min();
        let (chunk, marker) = match next {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if !chunk.is_empty() {
            let format = if code {
                egui::TextFormat {
                    font_id: egui::FontId::monospace(body.size * 0.92),
                    color: v.strong_text_color(),
                    background: v.code_bg_color,
                    ..Default::default()
                }
            } else {
                egui::TextFormat { font_id: body.clone(), color: if bold { v.strong_text_color() } else { normal }, ..Default::default() }
            };
            job.append(chunk, 0.0, format);
        }
        if let Some(after) = marker.strip_prefix("**").filter(|_| !code) {
            bold = !bold;
            rest = after;
        } else if let Some(after) = marker.strip_prefix('`') {
            code = !code;
            rest = after;
        } else if let Some(after) = marker.strip_prefix("**") {
            // `**` inside code stays as written.
            job.append("**", 0.0, job.sections.last().map(|s| s.format.clone()).unwrap_or_default());
            rest = after;
        } else {
            rest = "";
        }
    }
    job
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
        fn target(&mut self, _: &str, _: &Value) -> Option<egui::Rect> {
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
    fn markdown_marks_become_styles_not_text() {
        let mut out = String::new();
        let mut h = egui_kittest::Harness::new_ui(|ui| {
            let job = inline(ui, "En la capa **Muros**, con `OFFSET` y `a**b`.", false);
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("job"), job.text.clone()));
        });
        h.run();
        h.ctx.data(|d| out = d.get_temp::<String>(egui::Id::new("job")).unwrap());
        assert_eq!(out, "En la capa Muros, con OFFSET y a**b.");
    }

    #[test]
    fn work_exe_must_be_an_absolute_zwork_exe() {
        assert!(work_exe(None).is_none());
        assert!(work_exe(Some("zwork.exe".into())).is_none());
        assert!(work_exe(Some(std::env::current_exe().unwrap().into_os_string())).is_none());
    }

    #[test]
    fn nueva_charla_forgets_the_document_talk_and_keeps_the_draft() {
        let (mut c, _clock, dir) = chat("nueva", &[200], "Listo.");
        let mut doc = Doc { acted: vec![] };
        c.offer_key("zck_a", 3600);
        *c.draft_mut() = "Hola".into();
        c.send(&mut doc);
        wait_done(&mut c, &mut doc);
        assert_eq!(c.transcript().len(), 2);
        *c.draft_mut() = "a medio escribir".into();
        c.new_conversation();
        assert!(c.transcript().is_empty());
        assert_eq!(c.conv.draft, "a medio escribir");
        assert!(store::load(c.file.as_ref().unwrap()).messages.is_empty(), "a restart does not bring it back");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Renders the panel to PNG in light and dark, to look at it without a tool:
/// `ZENDA_THEO_PREVIEW=<dir> cargo test -p zenda-theo --features egui panel_preview -- --ignored`.
#[cfg(test)]
mod panel_preview {
    use super::*;
    use crate::dash::tests::{FakeClock, Scripted};

    struct Cad;
    impl Host for Cad {
        fn skill(&self) -> String {
            String::new()
        }
        fn actions(&self) -> Vec<Value> {
            vec![]
        }
        fn read(&mut self) -> Value {
            json!({})
        }
        fn act(&mut self, _: &str, _: &Value) -> Result<Value, String> {
            Ok(json!({}))
        }
        fn confirm(&mut self, _: &str, _: &Value) -> Option<String> {
            None
        }
        fn target(&mut self, _: &str, _: &Value) -> Option<egui::Rect> {
            None
        }
        fn suggestions(&self) -> Vec<String> {
            vec!["Dibujame un rectángulo de 4 x 3 m con cotas".into(), "¿Cómo hago muros dobles con OFFSET?".into()]
        }
    }

    fn chat(messages: Vec<Value>) -> Chat {
        let mut c = Chat::new("cad", "CAD", Arc::new(Scripted::new(&[], "")), Arc::new(FakeClock::default()));
        c.open = true;
        c.conv.messages = messages;
        c
    }

    fn talk() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "Dibujame la planta de una habitación de 4 x 3 m con una puerta de 0,80 m abajo, y acotala."}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "1"}]}),
            json!({"role": "tool", "content": "{}"}),
            json!({"role": "assistant", "content": "Listo. Dibujé la planta en la capa **Muros**:\n\n- Rectángulo de 4000 x 3000 mm.\n- Hueco de puerta de 800 mm en la pared de abajo, con su arco en **Aberturas**.\n- Cotas de largo y ancho.\n\nSi querés, la paso a muros dobles con `OFFSET` de 150 mm."}),
            json!({"role": "user", "content": "Sí, hacelo"}),
        ]
    }

    fn shot(name: &str, theme: egui::Theme, mut chat: Chat, busy: Option<&str>) {
        let Some(dir) = std::env::var_os("ZENDA_THEO_PREVIEW") else { return };
        if let Some(live) = busy {
            let (_tx, rx) = std::sync::mpsc::channel();
            chat.run = Some(Run { stop: Arc::new(std::sync::atomic::AtomicBool::new(false)), rx });
            chat.live = live.into();
        }
        let mut host = Cad;
        let mut h =
            egui_kittest::Harness::builder().with_size(egui::vec2(340.0, 640.0)).with_pixels_per_point(2.0).with_theme(theme).build_ui(move |ui| {
                egui::Frame::new().fill(ui.visuals().panel_fill).inner_margin(8.0).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    chat.ui(ui, &mut host)
                });
            });
        h.run_steps(4);
        let img = h.render().expect("render");
        img.save(std::path::Path::new(&dir).join(format!("{name}.png"))).expect("save");
    }

    /// A screen reader finds the chat: its title, each message, the box and the buttons are nodes in the
    /// AccessKit tree, the same one Windows' UI Automation reads (Store E2E of Work 0.2.28).
    #[test]
    fn a_screen_reader_reads_the_chat() {
        use egui_kittest::kittest::Queryable;
        let mut chat = chat(talk());
        let mut host = Cad;
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(340.0, 640.0)).build_ui(move |ui| {
            chat.ui(ui, &mut host);
        });
        h.run_steps(3);
        assert!(h.query_all_by_label_contains("Theo").next().is_some(), "the title");
        assert!(h.query_all_by_label_contains("habitación de 4 x 3 m").next().is_some(), "the person's message");
        assert!(h.query_all_by_label_contains("Rectángulo de 4000 x 3000 mm").next().is_some(), "Theo's answer");
        assert!(h.query_by_label("Escribile a Theo").is_some(), "the message box");
        assert!(h.query_by_label("Cerrar el chat").is_some(), "the close button");
    }

    #[test]
    #[ignore = "writes PNGs; run by hand"]
    fn panel_preview() {
        for (t, theme) in [("dark", egui::Theme::Dark), ("light", egui::Theme::Light)] {
            shot(&format!("{t}-empty"), theme, chat(vec![]), None);
            shot(&format!("{t}-talk"), theme, chat(talk()), Some(""));
            shot(&format!("{t}-done"), theme, chat(talk()[..4].to_vec()), None);
        }
    }
}
