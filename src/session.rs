use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

const SESSION_IDLE_TTL_MS: u64 = 30 * 60 * 1000;
pub const MAX_SESSIONS: usize = 10_000;

/// What the proxy remembers about one Claude Code session: how many requests
/// it has sent, numbered for the monitor and the traffic capture.
#[derive(Debug, Clone)]
pub struct SessionState {
    pub seq: u64,
    pub last_seen: u64,
}

#[derive(Default)]
struct SessionStore {
    map: HashMap<String, SessionState>,
    order: VecDeque<String>,
}

static SESSIONS: LazyLock<Mutex<SessionStore>> =
    LazyLock::new(|| Mutex::new(SessionStore::default()));

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_millis() as u64
}

pub fn existing_session(session_id: Option<&str>, now: u64) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let state = store.map.get(id).cloned()?;
    if now.saturating_sub(state.last_seen) > SESSION_IDLE_TTL_MS {
        store.map.remove(id);
        store.order.retain(|item| item != id);
        return None;
    }
    Some(state)
}

pub fn existing_session_now(session_id: Option<&str>) -> Option<SessionState> {
    existing_session(session_id, now_millis())
}

pub fn record_session_request(
    session_id: Option<&str>,
    prior: Option<&SessionState>,
    now: u64,
) -> Option<SessionState> {
    let id = session_id?;
    let mut store = SESSIONS.lock().expect("session lock");
    let stored = store
        .map
        .get(id)
        .cloned()
        .filter(|state| now.saturating_sub(state.last_seen) <= SESSION_IDLE_TTL_MS);
    if stored.is_none() && store.map.remove(id).is_some() {
        store.order.retain(|item| item != id);
    }
    let mut next = stored
        .or_else(|| {
            prior
                .filter(|state| now.saturating_sub(state.last_seen) <= SESSION_IDLE_TTL_MS)
                .cloned()
        })
        .unwrap_or(SessionState {
            seq: 0,
            last_seen: now,
        });
    next.seq += 1;
    next.last_seen = now;

    if !store.map.contains_key(id) {
        store.order.push_back(id.to_string());
    }
    store.map.insert(id.to_string(), next.clone());

    while store.order.len() > MAX_SESSIONS {
        if let Some(evict) = store.order.pop_front() {
            store.map.remove(&evict);
        } else {
            break;
        }
    }

    Some(next)
}

#[cfg(test)]
pub fn reset_sessions_for_test() {
    let mut store = SESSIONS.lock().expect("session lock");
    store.map.clear();
    store.order.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_requests_increment_latest_sequence() {
        let session_id = "session-concurrent-sequence-test";
        let initial = record_session_request(Some(session_id), None, 1).expect("initial session");
        let handles: Vec<_> = (0..16)
            .map(|offset| {
                let stale = initial.clone();
                std::thread::spawn(move || {
                    record_session_request(Some(session_id), Some(&stale), 2 + offset)
                        .expect("recorded session")
                        .seq
                })
            })
            .collect();
        let mut sequences: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("session thread"))
            .collect();
        sequences.sort_unstable();
        assert_eq!(sequences, (2..=17).collect::<Vec<_>>());
    }
}
