//! The chat key the tool holds, and when to ask Work for the next one.
//!
//! Work gives the tool a key valid for `expires_in` seconds. Times here are a monotonic clock read by the
//! caller (`Duration` since the app started), counted from when the key *arrived*: the PC's wall clock,
//! however wrong, never decides anything. Renewal is designed so a chat never breaks:
//!
//! - The tool asks Work for a new key at half the key's life, then again with backoff (1 s, 2 s, 4 s … 60 s)
//!   until one arrives. The old key keeps working meanwhile; the dash never cuts it before it expires.
//! - A key is not used in its last [`MARGIN`], so a request never starts on a key that dies mid-way.
//! - A 401 from the dash drops that key ([`KeyHolder::reject`]): the next read asks Work at once.
//! - With Work gone the current key is used until it expires; then [`KeyHolder::work_gone`] says so and the
//!   panel shows «Abrí Work para seguir con Theo».

use std::time::Duration;

/// A key is not handed out in its last seconds.
pub const MARGIN: Duration = Duration::from_secs(20);
/// After asking this long with no key to use, Work is taken as gone.
pub const GONE_AFTER: Duration = Duration::from_secs(15);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
struct Key {
    token: String,
    received: Duration,
    life: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    /// No key yet (first open, or the last one was rejected).
    Missing,
    /// Usable, before half its life.
    Fresh,
    /// Usable, past half its life: a new one is being asked for.
    Renewing,
    /// Too close to its end to start a request.
    Expired,
}

#[derive(Default, Debug)]
pub struct KeyHolder {
    key: Option<Key>,
    /// When the tool last asked Work for a key, and how many times since one arrived.
    asked: Option<Duration>,
    attempts: u32,
    /// When the tool started needing a key it did not have.
    lacking_since: Option<Duration>,
}

impl KeyHolder {
    /// Work sent a key valid for `expires_in_s` seconds from now. Ignores an empty token or a life of zero.
    pub fn offer(&mut self, token: &str, expires_in_s: u64, now: Duration) {
        if token.is_empty() || expires_in_s == 0 {
            return;
        }
        self.key = Some(Key { token: token.to_string(), received: now, life: Duration::from_secs(expires_in_s) });
        self.asked = None;
        self.attempts = 0;
        self.lacking_since = None;
    }

    pub fn state(&self, now: Duration) -> KeyState {
        let Some(k) = &self.key else { return KeyState::Missing };
        let age = now.saturating_sub(k.received);
        if age + MARGIN >= k.life {
            KeyState::Expired
        } else if age * 2 >= k.life {
            KeyState::Renewing
        } else {
            KeyState::Fresh
        }
    }

    /// The key to send now, if there is a usable one. Reading with none starts the «Work gone» clock.
    pub fn token(&mut self, now: Duration) -> Option<String> {
        match self.state(now) {
            KeyState::Fresh | KeyState::Renewing => self.key.as_ref().map(|k| k.token.clone()),
            KeyState::Missing | KeyState::Expired => {
                self.lacking_since.get_or_insert(now);
                None
            }
        }
    }

    /// Whether to ask Work for a key now. When true the caller asks (queues `work.chatKey`) and this
    /// counts the attempt.
    pub fn should_ask(&mut self, now: Duration) -> bool {
        if self.state(now) == KeyState::Fresh {
            return false;
        }
        if let Some(at) = self.asked
            && now.saturating_sub(at) < backoff(self.attempts)
        {
            return false;
        }
        self.asked = Some(now);
        self.attempts = self.attempts.saturating_add(1);
        true
    }

    /// The dash refused `token` (401): it is dropped, so the next [`should_ask`](Self::should_ask) asks at once.
    pub fn reject(&mut self, token: &str, now: Duration) {
        if self.key.as_ref().is_some_and(|k| k.token == token) {
            self.key = None;
            self.asked = None;
            self.attempts = 0;
            self.lacking_since.get_or_insert(now);
        }
    }

    /// No usable key, and Work has not sent one for [`GONE_AFTER`] of asking.
    pub fn work_gone(&self, now: Duration) -> bool {
        matches!(self.state(now), KeyState::Missing | KeyState::Expired)
            && self.lacking_since.is_some_and(|since| now.saturating_sub(since) >= GONE_AFTER)
    }
}

/// Wait before asking again after `attempts` asks: 1 s, 2 s, 4 s … up to a minute.
pub fn backoff(attempts: u32) -> Duration {
    if attempts == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs(1u64 << (attempts - 1).min(6)).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn a_key_is_fresh_then_renewing_then_expired_by_its_own_clock() {
        let mut k = KeyHolder::default();
        assert_eq!(k.state(S(0)), KeyState::Missing);
        // Arrived at t=100 s of app time; the wall clock is irrelevant (clock skew cannot matter).
        k.offer("zck_a", 3600, S(100));
        assert_eq!(k.state(S(100)), KeyState::Fresh);
        assert_eq!(k.state(S(100 + 1799)), KeyState::Fresh);
        assert_eq!(k.state(S(100 + 1800)), KeyState::Renewing);
        assert_eq!(k.token(S(100 + 3500)).as_deref(), Some("zck_a"));
        assert_eq!(k.state(S(100 + 3600 - 20)), KeyState::Expired);
        assert_eq!(k.token(S(100 + 3590)), None);
    }

    #[test]
    fn it_asks_at_half_life_with_backoff_and_keeps_the_old_key_meanwhile() {
        let mut k = KeyHolder::default();
        k.offer("zck_a", 3600, S(0));
        assert!(!k.should_ask(S(1000)), "fresh: no ask");
        assert!(k.should_ask(S(1800)), "half life: ask");
        assert!(!k.should_ask(S(1800)), "not twice at once");
        assert!(k.should_ask(S(1801)), "1 s later");
        assert!(!k.should_ask(S(1802)));
        assert!(k.should_ask(S(1803)), "2 s later");
        assert!(k.should_ask(S(1807)), "4 s later");
        // Meanwhile the old key is still sent.
        assert_eq!(k.token(S(1808)).as_deref(), Some("zck_a"));
        k.offer("zck_b", 3600, S(1810));
        assert_eq!(k.token(S(1811)).as_deref(), Some("zck_b"));
        assert!(!k.should_ask(S(1811)), "the new key is fresh");
    }

    #[test]
    fn backoff_doubles_up_to_a_minute() {
        let got: Vec<u64> = (0..10).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(got, [0, 1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn a_rejected_key_is_dropped_and_asked_for_at_once() {
        let mut k = KeyHolder::default();
        k.offer("zck_a", 3600, S(0));
        k.reject("zck_other", S(10));
        assert_eq!(k.token(S(10)).as_deref(), Some("zck_a"), "another token does not drop this one");
        k.reject("zck_a", S(10));
        assert_eq!(k.state(S(10)), KeyState::Missing);
        assert!(k.should_ask(S(10)));
    }

    #[test]
    fn with_work_gone_the_key_lasts_until_it_expires_then_says_so() {
        let mut k = KeyHolder::default();
        k.offer("zck_a", 600, S(0));
        // Work closed at t=300: asks go nowhere, but the key works until its end.
        for t in (300..570).step_by(10) {
            let _ = k.should_ask(S(t));
            assert_eq!(k.token(S(t)).as_deref(), Some("zck_a"), "t={t}");
            assert!(!k.work_gone(S(t)));
        }
        assert_eq!(k.token(S(585)), None);
        assert!(!k.work_gone(S(590)), "not before GONE_AFTER of lacking");
        assert!(k.work_gone(S(600)));
        // Work opens again and sends one: back to normal.
        k.offer("zck_b", 3600, S(700));
        assert!(!k.work_gone(S(700)));
        assert_eq!(k.token(S(701)).as_deref(), Some("zck_b"));
    }

    #[test]
    fn nothing_usable_from_a_bad_offer() {
        let mut k = KeyHolder::default();
        k.offer("", 3600, S(0));
        k.offer("zck_a", 0, S(0));
        assert_eq!(k.state(S(0)), KeyState::Missing);
    }
}
