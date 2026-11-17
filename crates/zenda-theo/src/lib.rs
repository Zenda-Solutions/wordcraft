//! Theo's chat inside Zenda's builds of the tools (Texto first).
//!
//! Work hands the tool a short-lived chat key (one hour, one tool, revocable) over the tool's authenticated
//! control channel; the tool calls the dash's chat proxy with it, and the dash checks plan and credit and
//! streams the answer. The model key never reaches the tool.
//!
//! - [`key`]: when to ask Work for the next key, so renewal never breaks a chat.
//! - [`dash`]: the key shared between threads, one model turn, the transparent retry on 401.
//! - [`agent`]: think → act → check on the chat thread; actions run on the UI thread; Frenar.
//! - [`store`]: one conversation per window and document, in the tool's profile.
//! - [`panel`] (feature `egui`): the side panel, the confirmation before destructive actions, Theo's
//!   cursor. The egui dependency is a range (0.36–0.37), so each fork's own egui is used.
//!
//! Nothing here depends on the tool: each one supplies its skill and its actions through
//! [`panel::Host`].

pub mod agent;
pub mod commands;
pub mod dash;
pub mod headless;
pub mod key;
#[cfg(feature = "egui")]
pub mod panel;
pub mod store;

use serde_json::Value;

/// Where conversations are kept: `ZWORK_CHAT_DIR`, which Work points at the tool's profile, or the temp
/// folder for a copy Work did not start.
pub fn chat_dir() -> std::path::PathBuf {
    std::env::var_os("ZWORK_CHAT_DIR").filter(|p| !p.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// The params of the control method `zenda.chat.key {llave, venceEn}`, checked: a `zck_` key and a life
/// of one second to one day.
pub fn key_params(p: &Value) -> Result<(String, u64), &'static str> {
    let token = p.get("llave").and_then(Value::as_str).ok_or("missing llave")?;
    let secs = p.get("venceEn").and_then(Value::as_u64).ok_or("missing venceEn")?;
    if !token.starts_with("zck_") || token.len() > 200 || secs == 0 || secs > 24 * 3600 {
        return Err("invalid chat key");
    }
    Ok((token.to_string(), secs))
}

/// A chat for `tool` (Work's manifest id) shown as `name`, talking to the dash over HTTPS.
/// The chat without egui (Circuitos), talking to the dash over HTTPS.
#[cfg(feature = "http")]
pub fn new_headless(tool: &str, name: &str) -> headless::Headless {
    headless::Headless::new(tool, name, std::sync::Arc::new(dash::Http), std::sync::Arc::new(dash::MonoClock::default()))
}

#[cfg(all(feature = "egui", feature = "http"))]
pub fn new_chat(tool: &str, name: &str) -> panel::Chat {
    panel::Chat::new(tool, name, std::sync::Arc::new(dash::Http), std::sync::Arc::new(dash::MonoClock::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_chat_key_with_a_sane_life_is_taken() {
        assert_eq!(key_params(&json!({"llave": "zck_a", "venceEn": 3600})), Ok(("zck_a".to_string(), 3600)));
        for bad in [
            json!({}),
            json!({"llave": "sk-model", "venceEn": 3600}),
            json!({"llave": "zck_a", "venceEn": 0}),
            json!({"llave": "zck_a", "venceEn": 999_999}),
            json!({"llave": "zck_a"}),
        ] {
            assert!(key_params(&bad).is_err(), "{bad}");
        }
    }
}
