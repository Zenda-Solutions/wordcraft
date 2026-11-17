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
pub mod dash;
pub mod key;
#[cfg(feature = "egui")]
pub mod panel;
pub mod store;
