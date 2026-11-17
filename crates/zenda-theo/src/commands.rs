//! Two tools that cover a whole app through its own command registry, for tools without a hand-written
//! action list (Foto, CAD, Presentaciones): `buscar` finds commands and their real parameters in the app's
//! `engine.commands`, `hacer` runs one if the tool's [`Policy`] allows it. As in Work's guide, nothing
//! about files, the app or the interface is reachable, and deletions ask first.

use serde_json::{Value, json};

pub const DO: &str = "hacer";
pub const FIND: &str = "buscar";
/// Commands `buscar` returns at most.
const FOUND_MAX: usize = 25;

/// What Theo may run in one app.
pub struct Policy {
    /// Command id prefixes Theo may use (`layer.`, `image.`).
    pub allow: &'static [&'static str],
    /// Exceptions inside them (`layer.export`).
    pub deny: &'static [&'static str],
    /// Commands that ask the person first, with the question.
    pub ask: &'static [(&'static str, &'static str)],
}

/// Never reachable from the chat, whatever the tool says.
const ALWAYS_DENIED: [&str; 8] = ["file.", "app.", "ui.", "work.", "zenda.", "window.", "help.", "plugin."];

impl Policy {
    pub fn allows(&self, id: &str) -> bool {
        !ALWAYS_DENIED.iter().any(|p| id.starts_with(p))
            && self.allow.iter().any(|p| id.starts_with(p) || id == p.trim_end_matches('.'))
            && !self.deny.iter().any(|d| id == *d || (d.ends_with('.') && id.starts_with(d)))
    }

    pub fn question(&self, id: &str) -> Option<String> {
        self.ask.iter().find(|(c, _)| *c == id).map(|(_, q)| q.to_string())
    }
}

/// The two function definitions.
pub fn tools(app_name: &str) -> Vec<Value> {
    vec![
        json!({"type": "function", "function": {
            "name": FIND,
            "description": format!("Busca comandos de {app_name} por palabra (en inglés: layer, crop, blur, line, circle…) y devuelve sus ids y parámetros reales."),
            "parameters": {"type": "object", "properties": {"que": {"type": "string"}}, "required": ["que"]},
        }}),
        json!({"type": "function", "function": {
            "name": DO,
            "description": format!("Ejecuta un comando de {app_name}: {{command, params}}. Usá ids y parámetros que devolvió buscar."),
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}, "params": {"type": "object"}}, "required": ["command"]},
        }}),
    ]
}

/// The allowed commands of `list` (the app's `engine.commands` answer, an array or `{commands: […]}`)
/// whose id or label contains `query`, with their parameters.
pub fn find(list: &Value, query: &str, policy: &Policy) -> Value {
    let q = query.trim().to_ascii_lowercase();
    let items = list.as_array().or_else(|| list.get("commands").and_then(Value::as_array)).cloned().unwrap_or_default();
    let found: Vec<Value> = items
        .into_iter()
        .filter(|c| {
            let id = c["id"].as_str().unwrap_or_default();
            let label = c["label"].as_str().unwrap_or_default().to_ascii_lowercase();
            policy.allows(id) && (q.is_empty() || id.to_ascii_lowercase().contains(&q) || label.contains(&q))
        })
        .take(FOUND_MAX)
        .map(|c| json!({"id": c["id"], "label": c["label"], "params": c.get("params").cloned().unwrap_or(Value::Null)}))
        .collect();
    json!({"commands": found})
}

/// The command of a `hacer` call, if the policy allows it.
pub fn command_of<'a>(args: &'a Value, policy: &Policy) -> Result<(&'a str, Value), String> {
    let id = args["command"].as_str().ok_or("Falta el comando.")?;
    if !policy.allows(id) {
        return Err(format!("Theo no puede usar «{id}» desde el chat."));
    }
    let params = args.get("params").filter(|p| p.is_object()).cloned().unwrap_or(json!({}));
    Ok((id, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOTO: Policy = Policy {
        allow: &["layer.", "image.", "filter.", "edit.undo", "edit.fill"],
        deny: &["layer.export"],
        ask: &[("layer.delete", "¿Borro la capa?")],
    };

    #[test]
    fn the_policy_keeps_files_app_and_interface_out() {
        assert!(FOTO.allows("layer.new") && FOTO.allows("filter.blur.gaussianBlur") && FOTO.allows("edit.undo"));
        for bad in ["file.save", "app.quit", "ui.click", "work.events", "zenda.chat.key", "layer.export", "edit.paste", "edit.undoAll"] {
            assert_eq!(FOTO.allows(bad), bad == "edit.undoAll", "{bad}");
        }
        assert_eq!(FOTO.question("layer.delete").as_deref(), Some("¿Borro la capa?"));
        assert_eq!(FOTO.question("layer.new"), None);
    }

    #[test]
    fn buscar_returns_only_allowed_commands_with_params() {
        let list = json!([
            {"id": "layer.new", "label": "New Layer", "params": {"name": "string"}},
            {"id": "file.save", "label": "Save layer"},
            {"id": "image.crop", "label": "Crop"},
        ]);
        let got = find(&list, "layer", &FOTO);
        assert_eq!(got["commands"].as_array().unwrap().len(), 1);
        assert_eq!(got["commands"][0]["params"]["name"], "string");
        assert_eq!(find(&json!({"commands": list}), "crop", &FOTO)["commands"][0]["id"], "image.crop");
    }

    #[test]
    fn hacer_refuses_what_the_policy_does_not_allow() {
        assert_eq!(
            command_of(&json!({"command": "layer.new", "params": {"name": "Fondo"}}), &FOTO).unwrap(),
            ("layer.new", json!({"name": "Fondo"}))
        );
        assert!(command_of(&json!({"command": "file.save"}), &FOTO).is_err());
        assert!(command_of(&json!({}), &FOTO).is_err());
        assert_eq!(command_of(&json!({"command": "layer.new", "params": 3}), &FOTO).unwrap().1, json!({}));
    }
}
