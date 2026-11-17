//! The chat without egui, for a tool whose interface is a web page (Circuitos: Tauri + JavaScript).
//!
//! The backend holds a [`Headless`] behind a mutex, the page polls [`Headless::view`] and calls
//! [`Headless::send`], [`Headless::retry`], [`Headless::stop`] and [`Headless::answer`]. Actions run on a
//! thread of the chat's own through [`Actions`] (the backend forwards them to the page or runs them
//! itself), so nothing waits on the UI. Same keys, renewal, conversation store and confirmation as the
//! egui panel.

use crate::agent::{self, Ask, READ, Run, ToUi};
use crate::dash::{CHAT_URL, ChatError, Clock, KEY_WAIT, SharedKey, Transport};
use crate::store::{self, Conversation};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the tool lends the chat, callable from any thread.
pub trait Actions: Send + Sync {
    fn skill(&self) -> String;
    fn actions(&self) -> Vec<Value>;
    fn read(&self) -> Value;
    fn act(&self, name: &str, args: &Value) -> Result<Value, String>;
    fn confirm(&self, name: &str, args: &Value) -> Option<String>;
}

struct Pending {
    name: String,
    args: Value,
    reply: Sender<Value>,
    question: String,
}

pub struct Headless {
    tool: String,
    tool_name: String,
    url: String,
    keys: Arc<SharedKey>,
    transport: Arc<dyn Transport>,
    file: Option<PathBuf>,
    conv: Conversation,
    run: Option<Run>,
    live: String,
    error: Option<ChatError>,
    pending: Option<Pending>,
    key_wait: Duration,
}

impl Headless {
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
            file: None,
            conv: Conversation::default(),
            run: None,
            live: String::new(),
            error: None,
            pending: None,
            key_wait: KEY_WAIT,
        }
    }

    pub fn keys(&self) -> &Arc<SharedKey> {
        &self.keys
    }

    pub fn bind(&mut self, profile: &Path, window: &str, document: &str) {
        let file = store::path(profile, window, document);
        if self.file.as_ref() != Some(&file) {
            self.save();
            self.conv = store::load(&file);
            self.file = Some(file);
        }
    }

    fn save(&self) {
        if let Some(f) = &self.file {
            let _ = store::save(f, &self.conv);
        }
    }

    pub fn set_draft(&mut self, draft: &str) {
        self.conv.draft = draft.chars().take(4000).collect();
    }

    /// Keeps the message (saved) before anything goes out, then starts the question.
    pub fn send(&mut self, text: &str, actions: Arc<dyn Actions>) {
        let text = text.trim();
        if text.is_empty() || self.run.is_some() {
            return;
        }
        self.conv.draft.clear();
        self.conv.messages.push(json!({"role": "user", "content": text}));
        self.save();
        self.start(actions);
    }

    pub fn retry(&mut self, actions: Arc<dyn Actions>) {
        if self.run.is_none() && self.conv.messages.last().is_some_and(|m| m["role"] != "assistant") {
            self.start(actions);
        }
    }

    fn start(&mut self, actions: Arc<dyn Actions>) {
        self.error = None;
        self.live.clear();
        let ask = Ask {
            url: self.url.clone(),
            tool: self.tool.clone(),
            system: actions.skill(),
            messages: self.conv.messages.clone(),
            actions: actions.actions(),
            key_wait: self.key_wait,
        };
        self.run = Some(agent::start(ask, self.transport.clone(), self.keys.clone()));
    }

    pub fn stop(&mut self) {
        if let Some(run) = &self.run {
            run.stop.store(true, Ordering::SeqCst);
        }
        if let Some(p) = self.pending.take() {
            let _ = p.reply.send(json!({"ok": false, "error": "La persona tocó Frenar."}));
        }
    }

    /// Runs what the chat thread asks. The backend calls it often (every poll of the page).
    pub fn pump(&mut self, actions: &dyn Actions) {
        while self.pending.is_none() {
            let Some(Ok(msg)) = self.run.as_ref().map(|r| r.rx.try_recv()) else { break };
            match msg {
                ToUi::Text(t) => self.live = t,
                ToUi::Act { name, arguments, reply } => {
                    if name == READ {
                        let _ = reply.send(json!({"ok": true, "result": actions.read()}));
                    } else if let Some(question) = actions.confirm(&name, &arguments) {
                        self.pending = Some(Pending { name, args: arguments, reply, question });
                    } else {
                        let _ = reply.send(result(actions.act(&name, &arguments)));
                    }
                }
                ToUi::Done { messages, result } => {
                    self.conv.messages = messages;
                    self.live.clear();
                    self.error = result.err();
                    self.run = None;
                    self.save();
                    return;
                }
            }
        }
    }

    pub fn answer(&mut self, yes: bool, actions: &dyn Actions) {
        let Some(p) = self.pending.take() else { return };
        let r = if yes { result(actions.act(&p.name, &p.args)) } else { json!({"ok": false, "error": "La persona no lo confirmó."}) };
        let _ = p.reply.send(r);
    }

    /// Everything the page shows.
    pub fn view(&self) -> Value {
        let transcript: Vec<Value> = self
            .conv
            .messages
            .iter()
            .filter_map(|m| {
                let text = m["content"].as_str().filter(|t| !t.trim().is_empty())?;
                matches!(m["role"].as_str()?, "user" | "assistant").then(|| json!({"mine": m["role"] == "user", "text": text}))
            })
            .collect();
        json!({
            "tool": self.tool_name,
            "hello": format!("Estás en {}. ¿En qué te ayudo?", self.tool_name),
            "transcript": transcript,
            "draft": self.conv.draft,
            "busy": self.run.is_some(),
            "live": self.live,
            "question": self.pending.as_ref().map(|p| p.question.clone()),
            "error": self.error.as_ref().map(ChatError::text),
            "workGone": self.error.as_ref() == Some(&ChatError::WorkGone),
        })
    }
}

fn result(r: Result<Value, String>) -> Value {
    match r {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// A [`Headless`] behind a mutex, as a Tauri state.
pub type Shared = Arc<Mutex<Headless>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dash::tests::{FakeClock, Scripted};

    struct Circuit(Mutex<Vec<String>>);

    impl Actions for Circuit {
        fn skill(&self) -> String {
            "Sos Theo en Circuitos.".into()
        }
        fn actions(&self) -> Vec<Value> {
            vec![]
        }
        fn read(&self) -> Value {
            json!({"parts": 2})
        }
        fn act(&self, name: &str, _: &Value) -> Result<Value, String> {
            self.0.lock().unwrap().push(name.into());
            Ok(json!({}))
        }
        fn confirm(&self, name: &str, _: &Value) -> Option<String> {
            (name == "part.delete").then(|| "¿Saco la pieza?".into())
        }
    }

    fn headless(name: &str, statuses: &[u16]) -> (Headless, PathBuf) {
        let mut h = Headless::new("wirecraft", "Circuitos", Arc::new(Scripted::new(statuses, "Listo.")), Arc::new(FakeClock::default()));
        h.key_wait = Duration::from_millis(300);
        let dir = std::env::temp_dir().join(format!("zenda-theo-headless-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        h.bind(&dir, "main", "circuito");
        (h, dir)
    }

    fn wait(h: &mut Headless, a: &dyn Actions) {
        for _ in 0..300 {
            h.pump(a);
            if !h.view()["busy"].as_bool().unwrap() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("did not end");
    }

    #[test]
    fn the_page_sees_the_greeting_the_answer_and_work_gone_with_the_message_kept() {
        let (mut h, dir) = headless("view", &[]);
        let a: Arc<dyn Actions> = Arc::new(Circuit(Mutex::new(vec![])));
        assert_eq!(h.view()["hello"], "Estás en Circuitos. ¿En qué te ayudo?");
        h.send("¿Cómo prendo un LED?", a.clone());
        wait(&mut h, a.as_ref());
        assert_eq!(h.view()["workGone"], true, "no key yet");
        assert_eq!(h.view()["transcript"][0]["text"], "¿Cómo prendo un LED?");
        h.keys().offer("zck_a", 3600);
        h.retry(a.clone());
        wait(&mut h, a.as_ref());
        assert_eq!(h.view()["error"], Value::Null);
        assert_eq!(h.view()["transcript"][1]["text"], "Listo.");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_destructive_action_waits_for_the_page() {
        let (mut h, dir) = headless("confirm", &[]);
        let circuit = Circuit(Mutex::new(vec![]));
        let (tx, rx) = std::sync::mpsc::channel();
        let (reply, got) = std::sync::mpsc::channel();
        tx.send(ToUi::Act { name: "part.delete".into(), arguments: json!({}), reply }).unwrap();
        h.run = Some(Run { stop: Arc::new(std::sync::atomic::AtomicBool::new(false)), rx });
        h.pump(&circuit);
        assert_eq!(h.view()["question"], "¿Saco la pieza?");
        assert!(circuit.0.lock().unwrap().is_empty());
        h.answer(true, &circuit);
        assert_eq!(got.recv().unwrap()["ok"], true);
        assert_eq!(*circuit.0.lock().unwrap(), ["part.delete"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
