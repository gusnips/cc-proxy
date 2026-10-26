//! Which provider and account each chat is on, kept across restarts.
//!
//! A chat that stays on one account keeps that provider's prompt cache. If a
//! restart or an update forgot the places, every running chat would take the
//! first ready row of the pool and send its whole history again, cold, to
//! another account. So the places are saved in `chats.json` in the state
//! folder. A chat id is the session id that is already in the log, and nothing
//! secret is stored.
//!
//! A place is dropped after a day without a request, and at most 2,048 chats
//! are kept. A chat is saved when it is new or moves, and again when its saved
//! time is more than five minutes old, so a request does not write the file.
//! A file that is missing, unreadable or corrupt starts the list empty. Saving
//! never fails or slows a request: the first error is logged once.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use super::now_ms;

/// A provider and one of its saved accounts, or the provider alone.
pub(super) type Place = (String, Option<String>);

/// How long a chat keeps its place after its last request.
const CHAT_MEMORY: Duration = Duration::from_secs(24 * 60 * 60);
/// Most chats remembered at once. The oldest goes first.
const CHAT_LIMIT: usize = 2_048;
/// A chat that keeps talking is saved again only after this long.
const SAVE_AGAIN_MS: u64 = 5 * 60 * 1_000;

/// One chat as it is saved.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedChat {
    chat: String,
    provider: String,
    account_id: Option<String>,
    /// Unix milliseconds of its last request.
    last_seen_ms: u64,
}

struct Chat {
    place: Place,
    /// Its last request. Ages are measured with this clock inside the process.
    seen: Instant,
    /// The `last_seen_ms` that is in the file.
    saved_ms: u64,
}

pub(super) struct ChatMemory {
    path: PathBuf,
    chats: HashMap<String, Chat>,
    warned: bool,
}

impl ChatMemory {
    /// The chats saved in `path` that are still fresh. Never fails.
    pub(super) fn load(path: PathBuf) -> Self {
        Self::load_at(path, now_ms())
    }

    fn load_at(path: PathBuf, now_ms: u64) -> Self {
        let mut memory = Self {
            path,
            chats: HashMap::new(),
            warned: false,
        };
        let mut rows = match read(&memory.path, now_ms) {
            Ok(rows) => rows,
            Err(error) => {
                memory.warn("chats_not_read", &error);
                Vec::new()
            }
        };
        rows.sort_by_key(|row| std::cmp::Reverse(row.last_seen_ms));
        rows.truncate(CHAT_LIMIT);
        for row in rows {
            let age = Duration::from_millis(now_ms.saturating_sub(row.last_seen_ms));
            memory.chats.insert(
                row.chat,
                Chat {
                    place: (row.provider, row.account_id),
                    seen: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
                    saved_ms: row.last_seen_ms,
                },
            );
        }
        memory
    }

    pub(super) fn place_of(&self, chat: &str) -> Option<Place> {
        let known = self.chats.get(chat)?;
        (known.seen.elapsed() < CHAT_MEMORY).then(|| known.place.clone())
    }

    pub(super) fn remember(&mut self, chat: &str, place: Place) {
        self.remember_at(chat, place, now_ms());
    }

    fn remember_at(&mut self, chat: &str, place: Place, now_ms: u64) {
        let (save, saved_ms) = match self.chats.get(chat) {
            Some(known)
                if known.place == place
                    && now_ms.saturating_sub(known.saved_ms) <= SAVE_AGAIN_MS =>
            {
                (false, known.saved_ms)
            }
            _ => (true, now_ms),
        };
        if !self.chats.contains_key(chat) && self.chats.len() >= CHAT_LIMIT {
            self.chats
                .retain(|_, known| known.seen.elapsed() < CHAT_MEMORY);
            let oldest = self
                .chats
                .iter()
                .min_by_key(|(_, known)| known.seen)
                .map(|(chat, _)| chat.clone());
            if self.chats.len() >= CHAT_LIMIT
                && let Some(oldest) = oldest
            {
                self.chats.remove(&oldest);
            }
        }
        self.chats.insert(
            chat.to_string(),
            Chat {
                place,
                seen: Instant::now(),
                saved_ms,
            },
        );
        if save && let Err(error) = self.write(now_ms) {
            self.warn("chats_not_saved", &error);
        }
    }

    /// Saves every fresh chat. Chats another proxy saved meanwhile stay.
    fn write(&self, now_ms: u64) -> io::Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("the chats file has no folder"))?;
        fs::create_dir_all(dir)?;
        let lock = File::create(self.path.with_extension("lock"))?;
        lock.lock()?;
        let mut rows: HashMap<String, SavedChat> = read(&self.path, now_ms)
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.chat.clone(), row))
            .collect();
        for (chat, known) in &self.chats {
            let ours = SavedChat {
                chat: chat.clone(),
                provider: known.place.0.clone(),
                account_id: known.place.1.clone(),
                last_seen_ms: now_ms.saturating_sub(known.seen.elapsed().as_millis() as u64),
            };
            match rows.get(chat) {
                Some(theirs) if theirs.last_seen_ms > ours.last_seen_ms => {}
                _ => {
                    rows.insert(chat.clone(), ours);
                }
            }
        }
        let mut rows: Vec<SavedChat> = rows.into_values().collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.last_seen_ms));
        rows.truncate(CHAT_LIMIT);
        crate::fsutil::write_atomic(&self.path, &serde_json::to_vec(&rows)?)
    }

    fn warn(&mut self, message: &str, error: &dyn std::fmt::Display) {
        if std::mem::replace(&mut self.warned, true) {
            return;
        }
        crate::logging::create_logger("auth").warn(
            message,
            Some(serde_json::Map::from_iter([
                (
                    "path".into(),
                    serde_json::json!(self.path.display().to_string()),
                ),
                ("error".into(), serde_json::json!(error.to_string())),
            ])),
        );
    }
}

/// The saved chats that are not too old. A missing file is no chats.
fn read(path: &Path, now_ms: u64) -> io::Result<Vec<SavedChat>> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let rows: Vec<SavedChat> = serde_json::from_slice(&raw)?;
    let oldest = now_ms.saturating_sub(CHAT_MEMORY.as_millis() as u64);
    Ok(rows
        .into_iter()
        .filter(|row| row.last_seen_ms >= oldest)
        .collect())
}

fn chats_path() -> PathBuf {
    // Unit tests never touch the real state folder.
    #[cfg(test)]
    {
        std::env::temp_dir().join(format!("cc-proxy-test-{}-chats.json", std::process::id()))
    }
    #[cfg(not(test))]
    {
        crate::paths::state_dir().join("chats.json")
    }
}

static MEMORY: Lazy<Mutex<Option<ChatMemory>>> = Lazy::new(|| Mutex::new(None));

/// Runs `use_memory` on the saved chats, loading them the first time, or again
/// when the state folder is another one (as in a test that isolates it).
fn with_memory<R>(use_memory: impl FnOnce(&mut ChatMemory) -> R) -> Option<R> {
    let path = chats_path();
    let mut guard = MEMORY.lock().ok()?;
    if guard.as_ref().is_none_or(|memory| memory.path != path) {
        *guard = Some(ChatMemory::load(path));
    }
    guard.as_mut().map(use_memory)
}

pub(super) fn place_of(chat: &str) -> Option<Place> {
    with_memory(|memory| memory.place_of(chat))?
}

pub(super) fn remember(chat: &str, place: Place) {
    with_memory(|memory| memory.remember(chat, place));
}

/// A restart, for a test: the next use reads the file again.
#[cfg(test)]
pub(super) fn forget_loaded() {
    if let Ok(mut guard) = MEMORY.lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(provider: &str, id: Option<&str>) -> Place {
        (provider.to_string(), id.map(str::to_string))
    }

    fn saved_rows(path: &Path) -> Vec<serde_json::Value> {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn a_chat_keeps_its_place_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chats.json");
        let mut before = ChatMemory::load(path.clone());
        before.remember("chat-a", place("claude", Some("acct-1")));
        before.remember("chat-b", place("codex", None));
        drop(before);

        let after = ChatMemory::load(path.clone());
        assert_eq!(
            after.place_of("chat-a"),
            Some(place("claude", Some("acct-1")))
        );
        assert_eq!(after.place_of("chat-b"), Some(place("codex", None)));
        assert_eq!(after.place_of("chat-c"), None);
        // The file holds what is needed to find the row again and nothing else.
        let rows = saved_rows(&path);
        assert_eq!(rows.len(), 2);
        let row = rows.iter().find(|row| row["chat"] == "chat-a").unwrap();
        assert_eq!(row["provider"], "claude");
        assert_eq!(row["accountId"], "acct-1");
        assert!(row["lastSeenMs"].as_u64().unwrap() > 0);
        assert_eq!(
            rows.iter().find(|row| row["chat"] == "chat-b").unwrap()["accountId"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_chat_that_moves_is_saved_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chats.json");
        let mut memory = ChatMemory::load(path.clone());
        memory.remember("chat-a", place("claude", Some("acct-1")));
        memory.remember("chat-a", place("codex", Some("acct-2")));
        let after = ChatMemory::load(path);
        assert_eq!(
            after.place_of("chat-a"),
            Some(place("codex", Some("acct-2")))
        );
    }

    #[test]
    fn a_chat_that_stays_put_is_not_saved_again_until_its_saved_time_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chats.json");
        let start = now_ms();
        let mut memory = ChatMemory::load_at(path.clone(), start);
        memory.remember_at("chat-a", place("claude", Some("acct-1")), start);
        let first = fs::read(&path).unwrap();

        // The same place a minute later writes nothing.
        fs::write(&path, b"[]").unwrap();
        memory.remember_at("chat-a", place("claude", Some("acct-1")), start + 60_000);
        assert_eq!(fs::read(&path).unwrap(), b"[]");

        // Six minutes after the last save, it is saved again with a fresh time.
        memory.remember_at(
            "chat-a",
            place("claude", Some("acct-1")),
            start + 6 * 60_000,
        );
        let rows = saved_rows(&path);
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["lastSeenMs"].as_u64().unwrap() >= start);
        assert_ne!(fs::read(&path).unwrap(), first);
    }

    #[test]
    fn chats_older_than_a_day_are_dropped_when_the_file_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chats.json");
        let now = now_ms();
        let day = CHAT_MEMORY.as_millis() as u64;
        fs::write(
            &path,
            serde_json::json!([
                {"chat": "fresh", "provider": "claude", "accountId": "a", "lastSeenMs": now - 60_000},
                {"chat": "nearly", "provider": "codex", "accountId": null, "lastSeenMs": now - day + 60_000},
                {"chat": "stale", "provider": "claude", "accountId": "b", "lastSeenMs": now - day - 1_000},
            ])
            .to_string(),
        )
        .unwrap();
        let memory = ChatMemory::load_at(path, now);
        assert_eq!(memory.place_of("fresh"), Some(place("claude", Some("a"))));
        assert_eq!(memory.place_of("nearly"), Some(place("codex", None)));
        assert_eq!(memory.place_of("stale"), None);
    }

    #[test]
    fn a_missing_or_corrupt_file_starts_empty_and_is_replaced_by_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let missing = ChatMemory::load(dir.path().join("none.json"));
        assert_eq!(missing.place_of("chat-a"), None);

        let path = dir.path().join("chats.json");
        fs::write(&path, b"{ this is not json").unwrap();
        let mut memory = ChatMemory::load(path.clone());
        assert_eq!(memory.place_of("chat-a"), None);
        memory.remember("chat-a", place("claude", Some("acct-1")));
        assert_eq!(
            ChatMemory::load(path).place_of("chat-a"),
            Some(place("claude", Some("acct-1")))
        );
    }

    #[test]
    fn a_save_that_cannot_happen_never_breaks_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut memory = ChatMemory::load(dir.path().join("chats.json"));
        assert!(!memory.warned);
        // A file where the folder should be: every write fails.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        memory.path = blocker.join("chats.json");
        memory.remember("chat-a", place("claude", None));
        // The place still works inside this process.
        assert_eq!(memory.place_of("chat-a"), Some(place("claude", None)));
        assert!(memory.warned);
    }

    #[test]
    fn two_proxies_saving_do_not_erase_each_others_chats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chats.json");
        let mut first = ChatMemory::load(path.clone());
        let mut second = ChatMemory::load(path.clone());
        first.remember("chat-a", place("claude", Some("a")));
        second.remember("chat-b", place("codex", Some("b")));
        let both = ChatMemory::load(path);
        assert_eq!(both.place_of("chat-a"), Some(place("claude", Some("a"))));
        assert_eq!(both.place_of("chat-b"), Some(place("codex", Some("b"))));
    }

    #[test]
    fn the_oldest_chat_leaves_when_the_list_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let mut memory = ChatMemory::load(dir.path().join("chats.json"));
        for at in 0..CHAT_LIMIT {
            // chat-0 is the oldest, a millisecond older than chat-1.
            let age = Duration::from_millis((CHAT_LIMIT - at) as u64);
            memory.chats.insert(
                format!("chat-{at}"),
                Chat {
                    place: place("claude", None),
                    seen: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
                    saved_ms: now_ms(),
                },
            );
        }
        memory.remember("newest", place("codex", None));
        assert_eq!(memory.chats.len(), CHAT_LIMIT);
        assert_eq!(memory.place_of("newest"), Some(place("codex", None)));
        assert_eq!(memory.place_of("chat-0"), None);
    }
}
