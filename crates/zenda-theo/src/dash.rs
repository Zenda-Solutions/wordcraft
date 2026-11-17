//! The tool's side of the dash chat proxy (`POST /api/v1/zwork/chat`): the key shared between the UI thread
//! (which receives keys from Work) and the chat thread (which sends), and one model turn with the
//! transparent retry on 401.

use crate::key::KeyHolder;
use serde_json::{Value, json};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Where the dash answers the chat.
pub const CHAT_URL: &str = "https://dash.zenda.group/api/v1/zwork/chat";
/// How long a send waits for Work to hand over a key it needs (first key, or after a 401).
pub const KEY_WAIT: Duration = Duration::from_secs(12);

/// A monotonic clock; tests move it by hand.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

pub struct MonoClock(Instant);

impl Default for MonoClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl Clock for MonoClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// The key, shared by the UI thread and the chat thread.
pub struct SharedKey {
    holder: Mutex<KeyHolder>,
    arrived: Condvar,
    clock: Arc<dyn Clock>,
}

impl SharedKey {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { holder: Mutex::new(KeyHolder::default()), arrived: Condvar::new(), clock }
    }

    pub fn now(&self) -> Duration {
        self.clock.now()
    }

    /// Work sent a key (control method `zenda.chat.key`).
    pub fn offer(&self, token: &str, expires_in_s: u64) {
        let now = self.clock.now();
        self.holder.lock().unwrap_or_else(|e| e.into_inner()).offer(token, expires_in_s, now);
        self.arrived.notify_all();
    }

    /// Called every frame by the UI: true when the tool should ask Work for a key now.
    pub fn should_ask(&self) -> bool {
        let now = self.clock.now();
        self.holder.lock().unwrap_or_else(|e| e.into_inner()).should_ask(now)
    }

    pub fn work_gone(&self) -> bool {
        let now = self.clock.now();
        self.holder.lock().unwrap_or_else(|e| e.into_inner()).work_gone(now)
    }

    pub fn reject(&self, token: &str) {
        let now = self.clock.now();
        self.holder.lock().unwrap_or_else(|e| e.into_inner()).reject(token, now);
    }

    /// A usable key, waiting up to `wait` for Work to send one. The UI keeps asking meanwhile.
    pub fn wait_token(&self, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        let mut holder = self.holder.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(t) = holder.token(self.clock.now()) {
                return Some(t);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            // Short waits, so a test clock moved by hand is seen too.
            holder = self.arrived.wait_timeout(holder, left.min(Duration::from_millis(100))).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    pub status: u16,
    /// The dash's `code` (`llave_vencida`, `sin_credito`…), empty when there is none.
    pub code: String,
    pub message: String,
}

/// Sends one request and feeds each line of the answer (server-sent events) to `on_line`.
pub trait Transport: Send + Sync {
    fn post(&self, url: &str, key: &str, tool: &str, body: &Value, on_line: &mut dyn FnMut(&str)) -> Result<(), HttpError>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatError {
    /// No key: Work is closed or did not answer. The panel offers to open Work.
    WorkGone,
    /// The dash said no (credit, plan, busy): its message, in Spanish.
    Refused { status: u16, code: String, message: String },
    /// No connection, or the model failed.
    Network(String),
}

impl ChatError {
    pub fn text(&self) -> String {
        match self {
            ChatError::WorkGone => "Abrí Work para seguir con Theo.".into(),
            ChatError::Refused { message, .. } => message.clone(),
            ChatError::Network(_) => "No pude hablar con Theo. Revisá la conexión y probá de nuevo.".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

impl Reply {
    /// The assistant message to keep in the history.
    pub fn message(&self) -> Value {
        let mut m = json!({"role": "assistant", "content": self.text});
        if !self.tool_calls.is_empty() {
            m["tool_calls"] = Value::Array(
                self.tool_calls
                    .iter()
                    .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                    .collect(),
            );
        }
        m
    }
}

/// Folds streamed chunks (`data: {…}` lines) into a reply.
#[derive(Default)]
pub struct Stream {
    pub reply: Reply,
}

impl Stream {
    /// One line of the stream; returns true when the visible text grew.
    pub fn line(&mut self, line: &str) -> bool {
        let Some(data) = line.trim().strip_prefix("data:") else { return false };
        let data = data.trim();
        if data == "[DONE]" {
            return false;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return false };
        let delta = &v["choices"][0]["delta"];
        let mut grew = false;
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.reply.text.push_str(t);
            grew = true;
        }
        for c in delta["tool_calls"].as_array().into_iter().flatten() {
            let i = c["index"].as_u64().unwrap_or(0) as usize;
            while self.reply.tool_calls.len() <= i {
                self.reply.tool_calls.push(ToolCall::default());
            }
            let call = &mut self.reply.tool_calls[i];
            if let Some(id) = c["id"].as_str() {
                call.id = id.to_string();
            }
            if let Some(n) = c["function"]["name"].as_str() {
                call.name.push_str(n);
            }
            if let Some(a) = c["function"]["arguments"].as_str() {
                call.arguments.push_str(a);
            }
        }
        grew
    }
}

/// One model turn. On a 401 (the key expired or was revoked) it drops the key, waits for the fresh one Work
/// sends, and sends the same request once more: the person never sees it. `on_text` gets the answer so far.
pub fn complete(
    transport: &dyn Transport,
    url: &str,
    tool: &str,
    keys: &SharedKey,
    body: &Value,
    key_wait: Duration,
    on_text: &mut dyn FnMut(&str),
) -> Result<Reply, ChatError> {
    let mut retried = false;
    loop {
        let token = keys.wait_token(key_wait).ok_or(ChatError::WorkGone)?;
        let mut stream = Stream::default();
        let result = transport.post(url, &token, tool, body, &mut |line| {
            if stream.line(line) {
                on_text(&stream.reply.text);
            }
        });
        match result {
            Ok(()) => return Ok(stream.reply),
            Err(e) if e.status == 401 => {
                keys.reject(&token);
                if retried {
                    return Err(ChatError::WorkGone);
                }
                retried = true;
            }
            Err(e) if e.status == 0 || e.status >= 500 => return Err(ChatError::Network(e.message)),
            Err(e) => return Err(ChatError::Refused { status: e.status, code: e.code, message: e.message }),
        }
    }
}

/// The real transport, with ureq.
#[cfg(feature = "http")]
pub struct Http;

#[cfg(feature = "http")]
impl Transport for Http {
    fn post(&self, url: &str, key: &str, tool: &str, body: &Value, on_line: &mut dyn FnMut(&str)) -> Result<(), HttpError> {
        use std::io::BufRead;
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(120))).http_status_as_error(false).build().into();
        let response = agent
            .post(url)
            .header("authorization", &format!("Bearer {key}"))
            .header("x-zwork-herramienta", tool)
            .send_json(body)
            .map_err(|e| HttpError { status: 0, code: String::new(), message: e.to_string() })?;
        let status = response.status().as_u16();
        if status != 200 {
            let v: Value = response.into_body().read_json().unwrap_or_default();
            return Err(HttpError {
                status,
                code: v["code"].as_str().unwrap_or_default().to_string(),
                message: v["error"].as_str().unwrap_or("Theo no está disponible ahora.").to_string(),
            });
        }
        let reader = std::io::BufReader::new(response.into_body().into_reader());
        for line in reader.lines() {
            let line = line.map_err(|e| HttpError { status: 0, code: String::new(), message: e.to_string() })?;
            on_line(&line);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A clock moved by hand, in milliseconds.
    #[derive(Default)]
    pub struct FakeClock(pub AtomicU64);

    impl FakeClock {
        pub fn set(&self, secs: u64) {
            self.0.store(secs * 1000, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    /// Answers from a script: each entry is the status for one request (200 streams `text`).
    pub struct Scripted {
        pub statuses: Mutex<Vec<u16>>,
        pub keys_seen: Mutex<Vec<String>>,
        pub text: String,
    }

    impl Scripted {
        pub fn new(statuses: &[u16], text: &str) -> Self {
            Self { statuses: Mutex::new(statuses.to_vec()), keys_seen: Mutex::new(Vec::new()), text: text.into() }
        }
    }

    impl Transport for Scripted {
        fn post(&self, _url: &str, key: &str, _tool: &str, _body: &Value, on_line: &mut dyn FnMut(&str)) -> Result<(), HttpError> {
            self.keys_seen.lock().unwrap().push(key.to_string());
            let status = { let mut s = self.statuses.lock().unwrap(); if s.is_empty() { 200 } else { s.remove(0) } };
            match status {
                200 => {
                    for word in self.text.split_inclusive(' ') {
                        on_line(&format!("data: {}", json!({"choices": [{"delta": {"content": word}}]})));
                    }
                    on_line("data: [DONE]");
                    Ok(())
                }
                401 => Err(HttpError { status, code: "llave_vencida".into(), message: "venció".into() }),
                402 => Err(HttpError { status, code: "sin_credito".into(), message: "Usaste todo lo de tu plan para Theo.".into() }),
                s => Err(HttpError { status: s, code: String::new(), message: "falló".into() }),
            }
        }
    }

    fn keys(clock: Arc<FakeClock>) -> Arc<SharedKey> {
        Arc::new(SharedKey::new(clock))
    }

    #[test]
    fn the_stream_folds_text_and_tool_calls() {
        let mut s = Stream::default();
        assert!(s.line(r##"data: {"choices":[{"delta":{"content":"Ho"}}]}"##));
        assert!(s.line(r##"data: {"choices":[{"delta":{"content":"la"}}]}"##));
        assert!(!s.line(r##"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"texto_","arguments":"{\"a\""}}]}}]}"##));
        s.line(r##"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"hacer","arguments":":1}"}}]}}]}"##);
        s.line("data: [DONE]");
        s.line(": keep-alive");
        assert_eq!(s.reply.text, "Hola");
        assert_eq!(s.reply.tool_calls, [ToolCall { id: "c1".into(), name: "texto_hacer".into(), arguments: r#"{"a":1}"#.into() }]);
        assert_eq!(s.reply.message()["tool_calls"][0]["function"]["name"], "texto_hacer");
    }

    #[test]
    fn a_401_mid_chat_gets_a_fresh_key_and_resends_once_transparently() {
        let clock = Arc::new(FakeClock::default());
        let k = keys(clock.clone());
        k.offer("zck_old", 3600);
        let t = Scripted::new(&[401, 200], "Listo, quedó en negrita.");
        // Work answers the ask with a new key while the send waits.
        let k2 = k.clone();
        let work = std::thread::spawn(move || {
            for _ in 0..50 {
                if k2.should_ask() {
                    k2.offer("zck_new", 3600);
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let mut shown = String::new();
        let r = complete(&t, CHAT_URL, "texto", &k, &json!({}), Duration::from_secs(3), &mut |s| shown = s.to_string()).unwrap();
        work.join().unwrap();
        assert_eq!(r.text, "Listo, quedó en negrita.");
        assert_eq!(shown, r.text);
        assert_eq!(*t.keys_seen.lock().unwrap(), ["zck_old", "zck_new"]);
    }

    #[test]
    fn two_401s_in_a_row_end_as_work_gone_not_a_loop() {
        let clock = Arc::new(FakeClock::default());
        let k = keys(clock);
        k.offer("zck_a", 3600);
        let t = Scripted::new(&[401, 401, 200], "x");
        let k2 = k.clone();
        let work = std::thread::spawn(move || {
            for _ in 0..50 {
                if k2.should_ask() {
                    k2.offer("zck_b", 3600);
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let e = complete(&t, CHAT_URL, "texto", &k, &json!({}), Duration::from_secs(3), &mut |_| {}).unwrap_err();
        work.join().unwrap();
        assert_eq!(e, ChatError::WorkGone);
        assert_eq!(t.keys_seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn with_work_closed_and_no_key_the_send_says_open_work() {
        let k = keys(Arc::new(FakeClock::default()));
        let t = Scripted::new(&[], "x");
        let e = complete(&t, CHAT_URL, "texto", &k, &json!({}), Duration::from_millis(300), &mut |_| {}).unwrap_err();
        assert_eq!(e, ChatError::WorkGone);
        assert_eq!(e.text(), "Abrí Work para seguir con Theo.");
        assert!(t.keys_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn an_expired_key_is_not_sent_even_if_the_wall_clock_says_otherwise() {
        let clock = Arc::new(FakeClock::default());
        let k = keys(clock.clone());
        k.offer("zck_a", 3600);
        clock.set(3590);
        let t = Scripted::new(&[], "x");
        assert_eq!(complete(&t, CHAT_URL, "texto", &k, &json!({}), Duration::from_millis(200), &mut |_| {}), Err(ChatError::WorkGone));
        assert!(t.keys_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn credit_and_server_errors_are_not_retried() {
        let k = keys(Arc::new(FakeClock::default()));
        k.offer("zck_a", 3600);
        let t = Scripted::new(&[402], "x");
        let e = complete(&t, CHAT_URL, "texto", &k, &json!({}), Duration::from_secs(1), &mut |_| {}).unwrap_err();
        assert!(matches!(&e, ChatError::Refused { status: 402, code, .. } if code == "sin_credito"), "{e:?}");
        let t = Scripted::new(&[502], "x");
        assert!(matches!(complete(&t, CHAT_URL, "texto", &k, &json!({}), Duration::from_secs(1), &mut |_| {}), Err(ChatError::Network(_))));
        assert_eq!(t.keys_seen.lock().unwrap().len(), 1);
    }
}
