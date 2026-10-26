//! The request pool: one flat, ordered list of providers and saved accounts.
//!
//! A request tries the enabled rows from the top. A row is a saved account
//! (`account_id` is its id), or a whole provider that holds one credential
//! (`account_id` is absent). Providers can mix: a Claude account, a Codex
//! account, then another Claude account. Disabled rows stay in the list and are
//! never picked.
//!
//! The list lives in `route.pool` in config.json. Older settings kept a first
//! provider and a fallback list instead. They are read as rows until the first
//! save, which writes the pool and removes the old keys. Reading never writes.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::accounts::Accounts;
use crate::config::{self, PoolEntry, Route};
use crate::provider::AuthState;
use crate::registry::Registry;
use crate::{config_keys, paths, route, ui};

/// What this computer has for one provider.
struct Candidate {
    provider: String,
    /// The rows it can serve: its saved accounts, or one row without an id for
    /// a provider with a single credential. `None` when the account list can't
    /// be read: the saved rows of that provider then stay as they are.
    ids: Option<Vec<Option<String>>>,
    /// The saved choice. The older settings put it first.
    current: Option<String>,
}

/// What this computer has for each routable provider, or only for `only`.
fn candidates(registry: &Registry, only: Option<&HashSet<String>>) -> Vec<Candidate> {
    route::routable_providers(registry)
        .into_iter()
        .filter(|id| only.is_none_or(|only| only.contains(*id)))
        .filter_map(|id| {
            let provider = registry.provider(id)?;
            let cli = provider.cli();
            let saved = cli
                .accounts()
                .filter(|support| support.pinned_credentials().is_none())
                .map(|support| saved_ids(id, support));
            let (ids, current) = match saved {
                Some(Ok((ids, current))) if !ids.is_empty() => (Some(ids), current),
                Some(Err(_)) => (None, None),
                // No saved account: the provider's one credential, from the
                // environment or its own file, is the row. Nothing, when it has none.
                _ if matches!(cli.auth_state(), AuthState::Missing) => (Some(Vec::new()), None),
                _ => (Some(vec![None]), None),
            };
            Some(Candidate {
                provider: id.to_string(),
                ids,
                current,
            })
        })
        .collect()
}

fn saved_ids(
    id: &str,
    support: &dyn crate::accounts::AccountSupport,
) -> Result<(Vec<Option<String>>, Option<String>)> {
    let accounts = Accounts::open(id, support)?;
    let ids = accounts
        .list()?
        .into_iter()
        .map(|account| Some(account.id))
        .collect();
    Ok((ids, accounts.current()?.map(|account| account.id)))
}

/// The live pool: the saved rows in their saved order, without rows whose
/// provider or account is gone, plus rows for what was added since.
///
/// A new account of a provider that is in use joins it, enabled, after that
/// provider's last row. A new provider waits at the end, turned off. With no
/// saved pool yet, the older first provider and fallbacks become enabled rows,
/// with each provider's saved choice first. Nothing is written.
pub fn entries(route: &Route, registry: &Registry) -> Result<Vec<PoolEntry>> {
    Ok(arrange(route, &candidates(registry, None)))
}

/// The enabled rows of [`entries`], for a request or a ping. It looks only at
/// the providers that have an enabled row, so a request never waits on a
/// provider it will not use.
pub(crate) fn active(route: &Route, registry: &Registry) -> Vec<PoolEntry> {
    let used: HashSet<String> = match &route.pool {
        Some(rows) => rows
            .iter()
            .filter(|row| row.enabled)
            .map(|row| row.provider.clone())
            .collect(),
        None => route
            .provider
            .iter()
            .chain(&route.fallback)
            .cloned()
            .collect(),
    };
    arrange(route, &candidates(registry, Some(&used)))
        .into_iter()
        .filter(|row| row.enabled)
        .collect()
}

fn arrange(route: &Route, candidates: &[Candidate]) -> Vec<PoolEntry> {
    let stored = route
        .pool
        .clone()
        .unwrap_or_else(|| older_rows(route, candidates));
    let known = |row: &PoolEntry| {
        candidates
            .iter()
            .find(|candidate| candidate.provider == row.provider)
            .is_some_and(|candidate| {
                candidate
                    .ids
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&row.account_id))
            })
    };
    let mut rows: Vec<PoolEntry> = Vec::new();
    for row in stored {
        let repeated = rows
            .iter()
            .any(|kept| kept.provider == row.provider && kept.account_id == row.account_id);
        if known(&row) && !repeated {
            rows.push(row);
        }
    }
    for candidate in candidates {
        for id in candidate.ids.iter().flatten() {
            let present = rows
                .iter()
                .any(|row| row.provider == candidate.provider && &row.account_id == id);
            if present {
                continue;
            }
            let in_use = rows
                .iter()
                .any(|row| row.provider == candidate.provider && row.enabled);
            let row = PoolEntry {
                provider: candidate.provider.clone(),
                account_id: id.clone(),
                enabled: in_use,
            };
            match rows.iter().rposition(|kept| kept.provider == row.provider) {
                Some(last) if in_use => rows.insert(last + 1, row),
                _ => rows.push(row),
            }
        }
    }
    rows
}

/// The rows the older settings meant: the first provider, then the fallbacks,
/// every saved account of each, the saved choice first.
fn older_rows(route: &Route, candidates: &[Candidate]) -> Vec<PoolEntry> {
    let mut rows = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for name in route.provider.iter().chain(&route.fallback) {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let Some(candidate) = candidates.iter().find(|c| &c.provider == name) else {
            continue;
        };
        let mut ids: Vec<Option<String>> = candidate.ids.clone().unwrap_or_default();
        if let Some(at) = ids.iter().position(|id| id == &candidate.current) {
            let chosen = ids.remove(at);
            ids.insert(0, chosen);
        }
        rows.extend(ids.into_iter().map(|account_id| PoolEntry {
            provider: name.clone(),
            account_id,
            enabled: true,
        }));
    }
    rows
}

/// Save the pool as it is given. One write. It keeps `route.on`, the model
/// choices and every other key, and removes the older first provider and
/// fallback keys.
pub fn save(entries: &[PoolEntry]) -> Result<()> {
    write(&paths::config_dir(), entries, false)
}

fn write(dir: &Path, entries: &[PoolEntry], turn_on: bool) -> Result<()> {
    let clean = clean(entries, &route::routable_providers(&Registry::new()))?;
    let mut values = vec![("route.pool", serde_json::json!(clean))];
    if turn_on {
        values.push(("route.on", serde_json::Value::Bool(true)));
    }
    config_keys::update_values_in(dir, values, ["route.provider", "route.fallback"])?;
    Ok(())
}

/// Rows that are safe to save: a known provider, an account id that is an id,
/// no row twice.
fn clean(entries: &[PoolEntry], routable: &[&str]) -> Result<Vec<PoolEntry>> {
    let mut rows: Vec<PoolEntry> = Vec::new();
    for entry in entries {
        let provider = entry.provider.trim().to_ascii_lowercase();
        if !routable.contains(&provider.as_str()) {
            bail!(
                "cc-proxy can't use `{provider}` in the pool. Use one of: {}.",
                routable.join(", ")
            );
        }
        if let Some(id) = &entry.account_id
            && uuid::Uuid::parse_str(id).is_err()
        {
            bail!("`{id}` is not a saved account id. Pick the account from the list.");
        }
        let repeated = rows
            .iter()
            .any(|row| row.provider == provider && row.account_id == entry.account_id);
        if !repeated {
            rows.push(PoolEntry {
                provider,
                ..entry.clone()
            });
        }
    }
    Ok(rows)
}

/// Turn one row on or off. The row keeps its place.
pub fn set_enabled(provider: &str, account_id: Option<&str>, enabled: bool) -> Result<()> {
    edit(|rows| {
        let at = find(rows, provider, account_id)?;
        rows[at].enabled = enabled;
        Ok(())
    })
}

/// Move one row up (negative) or down (positive) by `offset` places, stopping
/// at the ends. Returns where the row is now.
pub fn move_by(provider: &str, account_id: Option<&str>, offset: isize) -> Result<usize> {
    edit(|rows| shift(rows, provider, account_id, offset))
}

/// Make `provider` the first of the pool: its rows move to the top, in their
/// order. They turn on when none of them is on. With `only`, every other row
/// turns off. The rest keep their order.
pub fn promote_provider(provider: &str, only: bool) -> Result<()> {
    edit(|rows| {
        *rows = promoted(std::mem::take(rows), provider, only)?;
        Ok(())
    })
}

/// Put these providers first, in this order, and turn every other row off.
pub fn select_providers(providers: &[String]) -> Result<()> {
    edit(|rows| {
        *rows = selected(std::mem::take(rows), providers)?;
        Ok(())
    })
}

/// The pool as `cc-proxy use` leaves it, and the lock-free half of
/// [`promote_provider`], so a caller can check it before it changes anything.
pub(crate) fn promoted_rows(provider: &str, only: bool) -> Result<Vec<PoolEntry>> {
    promoted(live_rows()?, provider, only)
}

/// The pool as `cc-proxy use A B C` leaves it. See [`promoted_rows`].
pub(crate) fn selected_rows(providers: &[String]) -> Result<Vec<PoolEntry>> {
    selected(live_rows()?, providers)
}

/// Save `rows` and turn routing on, in one write.
pub(crate) fn save_and_turn_on(rows: &[PoolEntry]) -> Result<()> {
    locked(|| write(&paths::config_dir(), rows, true))
}

fn live_rows() -> Result<Vec<PoolEntry>> {
    entries(&config::route(), &Registry::new())
}

fn edit<R>(change: impl FnOnce(&mut Vec<PoolEntry>) -> Result<R>) -> Result<R> {
    locked(|| {
        let mut rows = live_rows()?;
        let out = change(&mut rows)?;
        write(&paths::config_dir(), &rows, false)?;
        Ok(out)
    })
}

/// Runs `change` while no other cc-proxy process edits the pool.
fn locked<R>(change: impl FnOnce() -> Result<R>) -> Result<R> {
    let dir = paths::config_dir();
    fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
    let lock = File::create(dir.join("pool.lock"))?;
    lock.lock().context("can't lock the request pool")?;
    change()
}

fn find(rows: &[PoolEntry], provider: &str, account_id: Option<&str>) -> Result<usize> {
    let provider = provider.trim().to_ascii_lowercase();
    rows.iter()
        .position(|row| row.provider == provider && row.account_id.as_deref() == account_id)
        .with_context(|| match account_id {
            Some(_) => format!(
                "That {} account is not in the pool. It may have been signed out or replaced. \
                 Refresh the list and try again.",
                ui::provider_name(&provider)
            ),
            None => format!(
                "{} is not in the pool. Run `cc-proxy login {provider}` first.",
                ui::provider_name(&provider)
            ),
        })
}

fn shift(
    rows: &mut Vec<PoolEntry>,
    provider: &str,
    account_id: Option<&str>,
    offset: isize,
) -> Result<usize> {
    let at = find(rows, provider, account_id)?;
    let last = rows.len() - 1;
    let to = at.saturating_add_signed(offset).min(last);
    let row = rows.remove(at);
    rows.insert(to, row);
    Ok(to)
}

fn promoted(rows: Vec<PoolEntry>, provider: &str, only: bool) -> Result<Vec<PoolEntry>> {
    let provider = provider.trim().to_ascii_lowercase();
    let (mut block, rest): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|row| row.provider == provider);
    if block.is_empty() {
        bail!(not_signed_in(&provider));
    }
    if block.iter().all(|row| !row.enabled) {
        block.iter_mut().for_each(|row| row.enabled = true);
    }
    block.extend(rest.into_iter().map(|row| PoolEntry {
        enabled: row.enabled && !only,
        ..row
    }));
    Ok(block)
}

fn selected(rows: Vec<PoolEntry>, providers: &[String]) -> Result<Vec<PoolEntry>> {
    let mut rest = rows;
    let mut out: Vec<PoolEntry> = Vec::new();
    for name in providers {
        let name = name.trim().to_ascii_lowercase();
        let (mut block, others): (Vec<_>, Vec<_>) =
            rest.into_iter().partition(|row| row.provider == name);
        rest = others;
        if block.is_empty() {
            if out.iter().any(|row| row.provider == name) {
                continue;
            }
            bail!(not_signed_in(&name));
        }
        if block.iter().all(|row| !row.enabled) {
            block.iter_mut().for_each(|row| row.enabled = true);
        }
        out.extend(block);
    }
    out.extend(rest.into_iter().map(|row| PoolEntry {
        enabled: false,
        ..row
    }));
    Ok(out)
}

fn not_signed_in(provider: &str) -> String {
    format!(
        "{} has no sign-in yet. Run `cc-proxy login {provider}`, then try again.",
        ui::provider_name(provider)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const A1: &str = "11111111-1111-4111-8111-111111111111";
    const A2: &str = "22222222-2222-4222-8222-222222222222";
    const B1: &str = "33333333-3333-4333-8333-333333333333";
    const C1: &str = "44444444-4444-4444-8444-444444444444";

    fn row(provider: &str, id: Option<&str>, enabled: bool) -> PoolEntry {
        PoolEntry {
            provider: provider.into(),
            account_id: id.map(str::to_string),
            enabled,
        }
    }

    fn found(provider: &str, ids: &[&str], current: Option<&str>) -> Candidate {
        Candidate {
            provider: provider.into(),
            ids: Some(ids.iter().map(|id| Some((*id).to_string())).collect()),
            current: current.map(|id| Some(id.to_string())).unwrap_or(None),
        }
    }

    fn route_with(pool: Option<Vec<PoolEntry>>) -> Route {
        Route {
            on: true,
            pool,
            ..Route::default()
        }
    }

    fn summary(rows: &[PoolEntry]) -> Vec<(String, String, bool)> {
        rows.iter()
            .map(|row| {
                (
                    row.provider.clone(),
                    row.account_id.clone().unwrap_or_default(),
                    row.enabled,
                )
            })
            .collect()
    }

    fn at(provider: &str, id: &str, enabled: bool) -> (String, String, bool) {
        (provider.into(), id.into(), enabled)
    }

    #[test]
    fn rows_from_different_providers_keep_the_order_they_were_saved_in() {
        let candidates = [
            found("claude", &[A1, A2], None),
            found("codex", &[B1], None),
        ];
        let saved = vec![
            row("claude", Some(A1), true),
            row("codex", Some(B1), true),
            row("claude", Some(A2), true),
        ];
        let rows = arrange(&route_with(Some(saved.clone())), &candidates);
        assert_eq!(rows, saved);
    }

    #[test]
    fn a_disabled_row_stays_in_the_list_and_a_removed_account_leaves_it() {
        let candidates = [found("claude", &[A1], None), found("codex", &[B1], None)];
        let saved = vec![
            row("claude", Some(A1), false),
            row("claude", Some(A2), true),
            row("codex", Some(B1), true),
            row("codex", Some(B1), true),
            row("gone", None, true),
        ];
        let rows = arrange(&route_with(Some(saved)), &candidates);
        assert_eq!(
            summary(&rows),
            [at("claude", A1, false), at("codex", B1, true)]
        );
    }

    #[test]
    fn a_new_account_of_a_provider_in_use_joins_it_and_a_new_provider_waits_off() {
        let candidates = [
            found("claude", &[A1, A2], None),
            found("codex", &[B1], None),
            found("kimi", &[C1], None),
        ];
        let saved = vec![row("claude", Some(A1), true), row("codex", Some(B1), true)];
        let rows = arrange(&route_with(Some(saved)), &candidates);
        assert_eq!(
            summary(&rows),
            [
                at("claude", A1, true),
                at("claude", A2, true),
                at("codex", B1, true),
                at("kimi", C1, false),
            ]
        );
        // A provider the person turned off stays off when it gets an account.
        let saved = vec![row("claude", Some(A1), false), row("codex", Some(B1), true)];
        let rows = arrange(&route_with(Some(saved)), &candidates);
        assert!(
            rows.iter()
                .any(|r| r.account_id.as_deref() == Some(A2) && !r.enabled)
        );
    }

    #[test]
    fn a_replaced_login_never_inherits_the_row_of_the_old_one() {
        let candidates = [found("claude", &[A2], None)];
        let saved = vec![row("claude", Some(A1), true)];
        let rows = arrange(&route_with(Some(saved)), &candidates);
        // The old id is gone. The new login joins as a new row, off, because
        // nothing of that provider was left on.
        assert_eq!(summary(&rows), [at("claude", A2, false)]);
    }

    #[test]
    fn an_account_list_that_cannot_be_read_keeps_the_saved_rows_of_that_provider() {
        let candidates = [Candidate {
            provider: "codex".into(),
            ids: None,
            current: None,
        }];
        let saved = vec![row("codex", Some(B1), true)];
        let rows = arrange(&route_with(Some(saved.clone())), &candidates);
        assert_eq!(rows, saved);
    }

    #[test]
    fn older_settings_become_rows_with_the_saved_choice_first() {
        let candidates = [
            found("codex", &[B1, A1], Some(A1)),
            found("claude", &[A2], None),
            found("kimi", &[C1], None),
        ];
        let route = Route {
            on: true,
            provider: Some("codex".into()),
            fallback: vec!["claude".into()],
            ..Route::default()
        };
        let rows = arrange(&route, &candidates);
        assert_eq!(
            summary(&rows),
            [
                at("codex", A1, true),
                at("codex", B1, true),
                at("claude", A2, true),
                at("kimi", C1, false),
            ]
        );
    }

    #[test]
    fn an_empty_saved_pool_stays_empty_instead_of_falling_back_to_older_settings() {
        let candidates = [found("codex", &[B1], None)];
        let route = Route {
            on: true,
            provider: Some("codex".into()),
            pool: Some(Vec::new()),
            ..Route::default()
        };
        let rows = arrange(&route, &candidates);
        assert_eq!(summary(&rows), [at("codex", B1, false)]);
        assert!(rows.iter().all(|row| !row.enabled));
    }

    #[test]
    fn a_provider_with_one_credential_is_one_row_without_an_id() {
        let candidates = [Candidate {
            provider: "anthropic".into(),
            ids: Some(vec![None]),
            current: None,
        }];
        let saved = vec![row("anthropic", None, true)];
        let rows = arrange(&route_with(Some(saved.clone())), &candidates);
        assert_eq!(rows, saved);
        // An id that no longer matches the provider's one credential is dropped.
        let stale = vec![row("anthropic", Some(A1), true)];
        let rows = arrange(&route_with(Some(stale)), &candidates);
        assert_eq!(summary(&rows), [at("anthropic", "", false)]);
    }

    #[test]
    fn moving_a_row_keeps_the_rest_in_order_and_stops_at_the_ends() {
        let mut rows = vec![
            row("claude", Some(A1), true),
            row("codex", Some(B1), true),
            row("claude", Some(A2), true),
        ];
        assert_eq!(shift(&mut rows, "claude", Some(A2), -1).unwrap(), 1);
        assert_eq!(
            summary(&rows),
            [
                at("claude", A1, true),
                at("claude", A2, true),
                at("codex", B1, true)
            ]
        );
        assert_eq!(shift(&mut rows, "claude", Some(A2), -9).unwrap(), 0);
        assert_eq!(shift(&mut rows, "claude", Some(A2), 9).unwrap(), 2);
        assert_eq!(rows[2].account_id.as_deref(), Some(A2));
        let error = shift(&mut rows, "kimi", None, 1).unwrap_err().to_string();
        assert!(error.contains("cc-proxy login kimi"), "{error}");
    }

    #[test]
    fn using_a_provider_moves_its_rows_up_and_keeps_what_was_turned_off() {
        let rows = vec![
            row("claude", Some(A1), true),
            row("codex", Some(B1), false),
            row("codex", Some(A2), true),
        ];
        let rows = promoted(rows, "codex", false).unwrap();
        assert_eq!(
            summary(&rows),
            [
                at("codex", B1, false),
                at("codex", A2, true),
                at("claude", A1, true)
            ]
        );
        let only = promoted(rows, "codex", true).unwrap();
        assert_eq!(
            summary(&only),
            [
                at("codex", B1, false),
                at("codex", A2, true),
                at("claude", A1, false)
            ]
        );
        // A provider with every row off comes back on.
        let off = vec![row("kimi", Some(C1), false), row("codex", Some(B1), true)];
        let on = promoted(off, "kimi", false).unwrap();
        assert_eq!(summary(&on), [at("kimi", C1, true), at("codex", B1, true)]);
    }

    #[test]
    fn using_a_provider_without_a_sign_in_says_how_to_sign_in() {
        let error = promoted(vec![row("codex", Some(B1), true)], "kimi", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cc-proxy login kimi"), "{error}");
        let error = selected(vec![row("codex", Some(B1), true)], &["kimi".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("cc-proxy login kimi"), "{error}");
    }

    #[test]
    fn a_chosen_list_of_providers_sets_the_order_and_turns_the_others_off() {
        let rows = vec![
            row("claude", Some(A1), true),
            row("codex", Some(B1), true),
            row("kimi", Some(C1), true),
        ];
        let rows = selected(rows, &["kimi".into(), "claude".into(), "kimi".into()]).unwrap();
        assert_eq!(
            summary(&rows),
            [
                at("kimi", C1, true),
                at("claude", A1, true),
                at("codex", B1, false)
            ]
        );
    }

    #[test]
    fn only_known_providers_and_real_ids_are_saved_and_repeats_are_dropped() {
        let routable = ["codex", "claude"];
        let rows = clean(
            &[
                row("Codex", Some(B1), true),
                row("codex", Some(B1), false),
                row("claude", None, true),
            ],
            &routable,
        )
        .unwrap();
        assert_eq!(
            summary(&rows),
            [at("codex", B1, true), at("claude", "", true)]
        );
        let error = clean(&[row("nope", None, true)], &routable)
            .unwrap_err()
            .to_string();
        assert!(error.contains("codex, claude"), "{error}");
        let error = clean(&[row("codex", Some("work"), true)], &routable)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a saved account id"), "{error}");
    }

    #[test]
    fn saving_keeps_every_other_key_and_removes_the_older_route_keys() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.json"),
            serde_json::json!({
                "port": 4000,
                "route": {
                    "on": true,
                    "provider": "codex",
                    "fallback": ["claude"],
                    "tiers": {"codex": {"opus": "gpt-6"}}
                }
            })
            .to_string(),
        )
        .unwrap();
        write(
            dir.path(),
            &[row("codex", Some(B1), true), row("claude", Some(A1), false)],
            false,
        )
        .unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(saved["port"], 4000);
        assert_eq!(saved["route"]["on"], true);
        assert_eq!(saved["route"]["tiers"]["codex"]["opus"], "gpt-6");
        assert!(saved["route"].get("provider").is_none());
        assert!(saved["route"].get("fallback").is_none());
        assert_eq!(
            saved["route"]["pool"],
            serde_json::json!([
                {"provider": "codex", "accountId": B1, "enabled": true},
                {"provider": "claude", "accountId": A1, "enabled": false}
            ])
        );
        // Turning routing on is part of the same write.
        write(dir.path(), &[row("codex", Some(B1), true)], true).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(saved["route"]["on"], true);
    }

    #[test]
    fn the_route_reads_the_pool_and_shows_its_first_enabled_providers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.json"),
            serde_json::json!({
                "route": {
                    "on": true,
                    "provider": "ignored",
                    "pool": [
                        {"provider": "claude", "accountId": A1, "enabled": false},
                        {"provider": "Codex", "accountId": B1},
                        {"provider": "claude", "accountId": A2, "enabled": true},
                        {"bad": "row"}
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();
        let route = crate::config::route_from(dir.path());
        assert_eq!(route.target(), Some("codex"));
        assert_eq!(route.fallback(), ["claude"]);
        assert_eq!(route.pool.as_ref().map(Vec::len), Some(3));
    }
}
