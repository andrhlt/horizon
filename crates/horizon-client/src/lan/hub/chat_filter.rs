//! The chat filter on the server side (docs/chat-filter.md): a player's text, before the server relays
//! it, through the server's [`ChatGuard`] (the platform's `horizon-chat-filter` on a dedicated server):
//! its list, mutes and escalation. A refused text reaches nobody; the sender is told in private, and the
//! game logs it and bans when the escalation says so (`HostEvent::ChatFiltered`).
use super::{HostEvent, Hub, chat::now_millis};
use horizon_core::nbt::{JavaString, Tag};
use horizon_protocol::{
    packets::play::{self, Clientbound as C},
    text::literal,
};
pub use horizon_server_files::chat_guard::Place;
use horizon_server_files::chat_guard::{Action, ChatGuard, Mute, duration_text};

/// What the sender reads when their text held a slur.
pub fn notice(place: Place, action: Action, now: i64) -> String {
    let refused = match place {
        Place::Chat | Place::Command => "That message wasn't sent",
        Place::Sign => "That sign wasn't changed",
        Place::Anvil => "That name wasn't used",
        Place::Book => "That book wasn't signed",
    };
    match action {
        Action::Muted { until } => format!(
            "{refused}: slurs aren't allowed here. You're muted for {}.",
            duration_text(until - now)
        ),
        _ => format!("{refused}: slurs aren't allowed here."),
    }
}

/// What a muted player reads: when they try to chat, and when an operator mutes them.
pub fn muted_notice(mute: &Mute, now: i64) -> String {
    let time = match mute.until {
        // "2 more minutes".
        Some(until) => format!(
            "for {}",
            duration_text(until - now).replacen(' ', " more ", 1)
        ),
        None => "until an operator unmutes you".into(),
    };
    let why = match (&mute.by, &mute.reason) {
        (None, _) => "slurs aren't allowed here",
        (Some(_), Some(reason)) => reason,
        (Some(_), None) => "",
    };
    if why.is_empty() {
        format!("You're muted {time}.")
    } else {
        format!("You're muted {time}: {why}.")
    }
}

/// The text in red.
fn red(text: &str) -> Tag {
    let mut t = literal(text);
    if let Tag::Compound(map) = &mut t {
        map.insert(
            JavaString::from("color"),
            Tag::String(JavaString::from("red")),
        );
    }
    t
}

/// The server's chat filter, shared by the server's hub and the hubs of its regions: a region handles its own
/// players' chat on its own thread, and the words, the mutes and the hits behind them are the server's.
pub type SharedChatGuard = std::sync::Arc<std::sync::Mutex<Box<dyn ChatGuard>>>;

fn locked(guard: &SharedChatGuard) -> std::sync::MutexGuard<'_, Box<dyn ChatGuard>> {
    guard.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Hub {
    /// `server.toml`'s `[chat_filter]` (dedicated servers); `None` turns it off.
    pub fn set_chat_filter(&mut self, guard: Option<Box<dyn ChatGuard>>) {
        self.chat_filter = guard.map(|g| std::sync::Arc::new(std::sync::Mutex::new(g)));
    }

    /// Whether the filter holds a word of `text`: false without a filter.
    pub fn chat_filter_matches(&self, text: &str) -> bool {
        self.chat_filter
            .as_ref()
            .is_some_and(|g| locked(g).matches(text))
    }

    /// Whether nobody is muted at `now` (true without a filter).
    pub fn chat_filter_has_no_mutes(&self, now: i64) -> bool {
        self.chat_filter
            .as_ref()
            .is_none_or(|g| locked(g).mutes(now).is_empty())
    }

    pub fn chat_filter_on(&self) -> bool {
        self.chat_filter.is_some()
    }

    /// Whether guest `i` may send `texts` (`place` says where to): true without a filter. A refusal
    /// tells the sender, counts a hit when a text holds a slur, and pushes `ChatFiltered`.
    pub fn chat_allowed(
        &mut self,
        i: usize,
        texts: &[&str],
        place: Place,
        events: &mut Vec<HostEvent>,
    ) -> bool {
        let Some(shared) = self.chat_filter.clone() else {
            return true;
        };
        let mut guard = locked(&shared);
        let (uuid, now) = (self.guests[i].uuid, now_millis());
        let hit = texts.iter().any(|t| guard.matches(t));
        let muted = place.is_chat().then(|| guard.muted(uuid, now)).flatten();
        if !hit {
            let Some(mute) = muted else {
                return true;
            };
            let text = muted_notice(mute, now);
            self.guests[i].send(&C::SystemChat(play::SystemChat {
                content: red(&text),
                overlay: false,
            }));
            return false;
        }
        // An anvil renames at every key typed: refused quietly, on the action bar, with no hit.
        if place == Place::Anvil {
            self.guests[i].send(&C::SystemChat(play::SystemChat {
                content: red(&notice(place, Action::Refused, now)),
                overlay: true,
            }));
            return false;
        }
        let name = self.guests[i].name.clone();
        let (count, action) = guard.strike(uuid, &name, now);
        if !matches!(action, Action::Banned { .. }) {
            self.guests[i].send(&C::SystemChat(play::SystemChat {
                content: red(&notice(place, action, now)),
                overlay: false,
            }));
        }
        events.push(HostEvent::ChatFiltered {
            sender: uuid,
            name,
            place,
            text: texts.join(" | "),
            count,
            action,
        });
        false
    }

    /// An operator's `/mute` or `/tempmute`; the player is told if they are here.
    pub fn mute(
        &mut self,
        uuid: u128,
        name: &str,
        until: Option<i64>,
        by: &str,
        reason: Option<String>,
    ) {
        let Some(shared) = self.chat_filter.clone() else {
            return;
        };
        let mut guard = locked(&shared);
        let now = now_millis();
        guard.mute(uuid, name, until, by, reason);
        let Some(text) = guard.muted(uuid, now).map(|m| muted_notice(m, now)) else {
            return;
        };
        for g in self.guests.iter().filter(|g| g.uuid == uuid) {
            g.send(&C::SystemChat(play::SystemChat {
                content: red(&text),
                overlay: false,
            }));
        }
    }

    /// The mutes in force, by name.
    pub fn mutes(&self) -> Vec<Mute> {
        self.chat_filter.as_ref().map_or_else(Vec::new, |g| {
            locked(g).mutes(now_millis()).into_iter().cloned().collect()
        })
    }

    /// `/unmute <player>`: the name as it was muted, if they were.
    pub fn unmute(&mut self, name: &str) -> Option<String> {
        locked(self.chat_filter.as_ref()?).unmute(name)
    }

    /// Writes a refused text to the filter's own log, when it keeps one; `Err` says why it couldn't.
    #[allow(clippy::too_many_arguments)]
    pub fn log_chat_filtered(
        &mut self,
        now: i64,
        name: &str,
        uuid: u128,
        place: Place,
        count: u32,
        action: Action,
        text: &str,
    ) -> Result<(), String> {
        match self.chat_filter.as_ref() {
            Some(guard) => locked(guard).log(now, name, uuid, place, count, action, text),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sender_is_told_why() {
        let now = 1_000_000;
        assert_eq!(
            notice(Place::Chat, Action::Refused, now),
            "That message wasn't sent: slurs aren't allowed here."
        );
        assert_eq!(
            notice(
                Place::Command,
                Action::Muted {
                    until: now + 600_000
                },
                now
            ),
            "That message wasn't sent: slurs aren't allowed here. You're muted for 10 minutes."
        );
        let mute = |until, by: Option<&str>, reason: Option<&str>| Mute {
            uuid: String::new(),
            name: "Ann".into(),
            until,
            by: by.map(Into::into),
            reason: reason.map(Into::into),
        };
        assert_eq!(
            muted_notice(&mute(Some(now + 61_000), None, None), now),
            "You're muted for 2 more minutes: slurs aren't allowed here."
        );
        assert_eq!(
            muted_notice(&mute(None, Some("Op"), Some("spam")), now),
            "You're muted until an operator unmutes you: spam."
        );
        assert_eq!(
            muted_notice(&mute(Some(now + 7_200_000), Some("Op"), None), now),
            "You're muted for 2 more hours."
        );
        assert_eq!(
            notice(Place::Sign, Action::Refused, now),
            "That sign wasn't changed: slurs aren't allowed here."
        );
        assert!(Place::Chat.is_chat() && Place::Command.is_chat() && !Place::Sign.is_chat());
    }
}
