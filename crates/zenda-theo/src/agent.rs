//! The chat thread: one question from the person, answered in turns of think → act → check until Theo
//! answers in words. Actions run on the UI thread, through the tool's own command API (never the mouse):
//! the thread asks with [`ToUi::Act`] and waits for the result. The UI asks the person first for a
//! destructive action. [`Run::stop`] (Frenar) is checked before every model call and every action.

use crate::dash::{ChatError, SharedKey, Transport, complete};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::time::Duration;

/// Turns with actions before Theo must answer in words.
pub const MAX_STEPS: usize = 8;
/// The read-only tool every tool offers: what the document looks like now.
pub const READ: &str = "leer_documento";

/// What the chat thread asks of the UI thread.
pub enum ToUi {
    /// The answer so far, to show as it streams.
    Text(String),
    /// Run one action (or [`READ`]) and send back its result as JSON.
    Act { name: String, arguments: Value, reply: Sender<Value> },
    /// The question ended: the conversation's messages after it (to keep), and the answer or why it failed.
    Done { messages: Vec<Value>, result: Result<String, ChatError> },
}

/// A question running on the chat thread.
pub struct Run {
    pub stop: Arc<AtomicBool>,
    pub rx: Receiver<ToUi>,
}

/// What the thread needs for one question.
pub struct Ask {
    pub url: String,
    pub tool: String,
    /// The tool's skill: who Theo is in this app, its menus in Spanish, examples.
    pub system: String,
    /// The conversation so far, ending with the person's new message.
    pub messages: Vec<Value>,
    /// The actions, as OpenAI function definitions ([`READ`] is added here).
    pub actions: Vec<Value>,
    pub key_wait: Duration,
}

pub fn read_tool() -> Value {
    json!({"type": "function", "function": {
        "name": READ,
        "description": "Lee cómo está el documento ahora: estructura, selección y dónde está el cursor. No cambia nada.",
        "parameters": {"type": "object", "properties": {}},
    }})
}

/// Starts `ask` on its own thread.
pub fn start(ask: Ask, transport: Arc<dyn Transport>, keys: Arc<SharedKey>) -> Run {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = channel();
    let flag = stop.clone();
    std::thread::spawn(move || {
        let mut messages = ask.messages.clone();
        let result = run(&ask, transport.as_ref(), &keys, &flag, &tx, &mut messages);
        let _ = tx.send(ToUi::Done { messages, result });
    });
    Run { stop, rx }
}

fn run(ask: &Ask, transport: &dyn Transport, keys: &SharedKey, stop: &AtomicBool, tx: &Sender<ToUi>, messages: &mut Vec<Value>) -> Result<String, ChatError> {
    let mut tools = vec![read_tool()];
    tools.extend(ask.actions.iter().cloned());
    for _ in 0..MAX_STEPS {
        if stop.load(Ordering::SeqCst) {
            return Ok("Frené.".into());
        }
        let mut body_messages = vec![json!({"role": "system", "content": ask.system})];
        body_messages.extend(messages.iter().cloned());
        let body = json!({"messages": body_messages, "tools": tools, "stream": true});
        let reply = complete(transport, &ask.url, &ask.tool, keys, &body, ask.key_wait, &mut |text| {
            let _ = tx.send(ToUi::Text(text.to_string()));
        })?;
        messages.push(reply.message());
        if reply.tool_calls.is_empty() {
            return Ok(reply.text);
        }
        for call in &reply.tool_calls {
            let result = if stop.load(Ordering::SeqCst) {
                json!({"ok": false, "error": "La persona tocó Frenar."})
            } else {
                let arguments = serde_json::from_str(&call.arguments).unwrap_or(json!({}));
                let (reply_tx, reply_rx) = channel();
                if tx.send(ToUi::Act { name: call.name.clone(), arguments, reply: reply_tx }).is_err() {
                    return Ok("Frené.".into());
                }
                // The person may take a while to confirm; the UI always answers (or drops the sender).
                reply_rx.recv().unwrap_or(json!({"ok": false, "error": "La herramienta se cerró."}))
            };
            messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": result.to_string()}));
        }
    }
    Ok("Hice varios pasos; contame si querés que siga.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dash::{HttpError, MonoClock};
    use std::sync::Mutex;

    /// Plays the model: each request gets the next scripted chunk list.
    struct Model(Mutex<Vec<Vec<Value>>>, Mutex<Vec<Value>>);

    impl Transport for Model {
        fn post(&self, _: &str, _: &str, _: &str, body: &Value, on_line: &mut dyn FnMut(&str)) -> Result<(), HttpError> {
            self.1.lock().unwrap().push(body.clone());
            let chunks = self.0.lock().unwrap().remove(0);
            for c in chunks {
                on_line(&format!("data: {c}"));
            }
            Ok(())
        }
    }

    fn call(id: &str, name: &str, args: &str) -> Value {
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": args}}]}}]})
    }

    fn text(t: &str) -> Value {
        json!({"choices": [{"delta": {"content": t}}]})
    }

    fn ask() -> Ask {
        Ask {
            url: "u".into(),
            tool: "texto".into(),
            system: "Sos Theo en Texto.".into(),
            messages: vec![json!({"role": "user", "content": "poné el título en negrita"})],
            actions: vec![json!({"type": "function", "function": {"name": "format.set"}})],
            key_wait: Duration::from_secs(1),
        }
    }

    fn keys() -> Arc<SharedKey> {
        let k = Arc::new(SharedKey::new(Arc::new(MonoClock::default())));
        k.offer("zck_a", 3600);
        k
    }

    #[test]
    fn think_act_check_then_answer() {
        let model = Arc::new(Model(
            Mutex::new(vec![
                vec![call("c1", READ, "{}")],
                vec![call("c2", "format.set", r#"{"props":{"bold":true}}"#)],
                vec![text("Listo, "), text("quedó en negrita.")],
            ]),
            Mutex::new(vec![]),
        ));
        let run = start(ask(), model.clone(), keys());
        let mut acted = vec![];
        let (messages, result) = loop {
            match run.rx.recv().unwrap() {
                ToUi::Act { name, arguments, reply } => {
                    acted.push((name, arguments));
                    reply.send(json!({"ok": true})).unwrap();
                }
                ToUi::Text(_) => {}
                ToUi::Done { messages, result } => break (messages, result),
            }
        };
        assert_eq!(result.unwrap(), "Listo, quedó en negrita.");
        assert_eq!(acted[0].0, READ);
        assert_eq!(acted[1], ("format.set".to_string(), json!({"props": {"bold": true}})));
        // user, assistant(call), tool, assistant(call), tool, assistant(text)
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[2]["tool_call_id"], "c1");
        let bodies = model.1.lock().unwrap();
        assert_eq!(bodies[0]["messages"][0]["content"], "Sos Theo en Texto.");
        assert_eq!(bodies[0]["tools"][0]["function"]["name"], READ);
        assert_eq!(bodies.len(), 3);
    }

    #[test]
    fn frenar_stops_before_the_next_action_and_keeps_the_history() {
        let model = Arc::new(Model(
            Mutex::new(vec![vec![call("c1", "format.set", "{}")], vec![text("no debería llegar")]]),
            Mutex::new(vec![]),
        ));
        let run = start(ask(), model.clone(), keys());
        let stop = run.stop.clone();
        let (messages, result) = loop {
            match run.rx.recv().unwrap() {
                ToUi::Act { reply, .. } => {
                    stop.store(true, Ordering::SeqCst);
                    reply.send(json!({"ok": true})).unwrap();
                }
                ToUi::Text(_) => {}
                ToUi::Done { messages, result } => break (messages, result),
            }
        };
        assert_eq!(result.unwrap(), "Frené.");
        assert_eq!(model.1.lock().unwrap().len(), 1, "no model call after Frenar");
        assert_eq!(messages[0]["content"], "poné el título en negrita");
    }

    #[test]
    fn a_refused_action_goes_back_to_theo_as_its_result() {
        let model = Arc::new(Model(
            Mutex::new(vec![vec![call("c1", "text.delete", "{}")], vec![text("No borré nada.")]]),
            Mutex::new(vec![]),
        ));
        let run = start(ask(), model.clone(), keys());
        let result = loop {
            match run.rx.recv().unwrap() {
                ToUi::Act { reply, .. } => reply.send(json!({"ok": false, "error": "La persona no lo confirmó."})).unwrap(),
                ToUi::Text(_) => {}
                ToUi::Done { result, messages } => {
                    assert!(messages[2]["content"].as_str().unwrap().contains("no lo confirmó"));
                    break result;
                }
            }
        };
        assert_eq!(result.unwrap(), "No borré nada.");
    }
}
