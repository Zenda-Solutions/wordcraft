//! One conversation per window and document, kept in the tool's profile (`<profile>/theo-chat/<id>.json`),
//! so closing the panel, a failed send or a restart never loses what was said or what was being typed.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Messages kept per conversation; older ones are dropped first (the model sees the newest anyway).
pub const KEEP: usize = 60;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Conversation {
    /// OpenAI-style messages: user, assistant (with tool calls), tool results.
    pub messages: Vec<Value>,
    /// What the person was typing and did not send yet.
    pub draft: String,
}

/// FNV-1a, stable across Rust versions (unlike `DefaultHasher`), for file names.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// The conversation's file for `window` (the viewport id) and `document` (its path, or its title when
/// never saved).
pub fn path(profile: &Path, window: &str, document: &str) -> PathBuf {
    profile.join("theo-chat").join(format!("{:016x}.json", fnv(&format!("{window}\u{0}{document}"))))
}

pub fn load(file: &Path) -> Conversation {
    std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Writes atomically (temp file + rename), trimming to [`KEEP`] messages at a user turn.
pub fn save(file: &Path, c: &Conversation) -> std::io::Result<()> {
    let mut c = c.clone();
    if c.messages.len() > KEEP {
        let mut cut = c.messages.len() - KEEP;
        // Never start on a tool result or an assistant turn: the model needs whole exchanges.
        while cut < c.messages.len() && c.messages[cut]["role"] != "user" {
            cut += 1;
        }
        c.messages.drain(..cut);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&c)?)?;
    std::fs::rename(tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zenda-theo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn each_window_and_document_has_its_own_conversation() {
        let p = Path::new("C:/profile");
        assert_ne!(path(p, "w1", "a.docx"), path(p, "w2", "a.docx"));
        assert_ne!(path(p, "w1", "a.docx"), path(p, "w1", "b.docx"));
        assert_eq!(path(p, "w1", "a.docx"), path(p, "w1", "a.docx"));
        assert!(path(p, "w1", "a.docx").starts_with(p.join("theo-chat")));
    }

    #[test]
    fn it_survives_a_restart_with_the_draft_and_trims_at_a_user_turn() {
        let d = dir("store");
        let f = path(&d, "w", "doc");
        assert_eq!(load(&f), Conversation::default());
        let mut c = Conversation { draft: "a medio escribir".into(), ..Default::default() };
        for i in 0..40 {
            c.messages.push(json!({"role": "user", "content": format!("p{i}")}));
            c.messages.push(json!({"role": "assistant", "content": format!("r{i}")}));
        }
        save(&f, &c).unwrap();
        let back = load(&f);
        assert_eq!(back.draft, "a medio escribir");
        assert!(back.messages.len() <= KEEP);
        assert_eq!(back.messages[0]["role"], "user");
        assert_eq!(back.messages.last().unwrap()["content"], "r39");
        let _ = std::fs::remove_dir_all(d);
    }
}
