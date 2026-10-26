//! `cc-proxy pool`: see the request pool, turn rows on or off and move them.
//! Everything here goes through `crate::pool`, the same calls the monitor uses.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};

use crate::accounts::Accounts;
use crate::config::{self, PoolEntry};
use crate::registry::Registry;
use crate::{pool, ui};

/// One row with the name a person knows it by.
struct Row {
    entry: PoolEntry,
    /// The saved account's name. Empty for a provider with one credential.
    account: String,
}

fn rows(registry: &Registry) -> Result<Vec<Row>> {
    let entries = pool::entries(&config::route(), registry)?;
    let mut names: HashMap<(String, String), String> = HashMap::new();
    let providers: BTreeSet<&str> = entries
        .iter()
        .map(|entry| entry.provider.as_str())
        .collect();
    for provider in providers {
        let Some(handler) = registry.provider(provider) else {
            continue;
        };
        let Some(support) = handler.cli().accounts() else {
            continue;
        };
        if let Ok(accounts) = Accounts::open(provider, support).and_then(|a| a.list()) {
            for account in accounts {
                names.insert((provider.to_string(), account.id), account.name);
            }
        }
    }
    Ok(entries
        .into_iter()
        .map(|entry| {
            let account = entry
                .account_id
                .as_ref()
                .and_then(|id| names.get(&(entry.provider.clone(), id.clone())))
                .cloned()
                .unwrap_or_default();
            Row { entry, account }
        })
        .collect())
}

fn print_rows(rows: &[Row]) {
    let styled = ui::styled(&std::io::stdout());
    if rows.is_empty() {
        println!(
            "Nothing is signed in yet. Run `cc-proxy login <provider>` to add a provider, \
             then it shows up here."
        );
        return;
    }
    println!("Requests try these from the top. The first one that is ready answers.\n");
    let width = rows.len().to_string().len();
    for (at, row) in rows.iter().enumerate() {
        let provider = ui::strong(
            ui::provider_name(&row.entry.provider),
            ui::provider_color(&row.entry.provider),
            styled,
        );
        let state = if row.entry.enabled { "on" } else { "off" };
        let account = if row.account.is_empty() {
            String::new()
        } else {
            format!("  {}", row.account)
        };
        println!("{:>width$}  {provider}{account}  {state}", at + 1);
    }
    println!(
        "\nChange it with `cc-proxy pool on|off <provider> [account]` and \
         `cc-proxy pool move <provider> [account] up|down`."
    );
}

/// `cc-proxy pool`: the rows in the order requests try them.
pub fn list() -> Result<()> {
    print_rows(&rows(&Registry::new())?);
    Ok(())
}

/// The rows of `provider`, or only the one account called `account`.
fn pick<'a>(rows: &'a [Row], provider: &str, account: Option<&str>) -> Result<Vec<&'a Row>> {
    let provider = provider.trim().to_ascii_lowercase();
    let of_provider: Vec<&Row> = rows
        .iter()
        .filter(|row| row.entry.provider == provider)
        .collect();
    if of_provider.is_empty() {
        bail!(
            "{} is not in the pool. Run `cc-proxy login {provider}` to sign in, or \
             `cc-proxy provider list` to see the names.",
            ui::provider_name(&provider)
        );
    }
    let Some(account) = account else {
        return Ok(of_provider);
    };
    let wanted = account.trim().to_ascii_lowercase();
    let found: Vec<&Row> = of_provider
        .iter()
        .copied()
        .filter(|row| row.account == wanted)
        .collect();
    if found.is_empty() {
        let names: Vec<&str> = of_provider
            .iter()
            .map(|row| row.account.as_str())
            .filter(|name| !name.is_empty())
            .collect();
        bail!(
            "{} has no account called `{wanted}`. You have: {}.",
            ui::provider_name(&provider),
            names.join(", ")
        );
    }
    Ok(found)
}

/// `cc-proxy pool on|off <provider> [account]`. Without an account, every row
/// of the provider.
pub fn turn(on: bool, provider: &str, account: Option<&str>) -> Result<()> {
    let registry = Registry::new();
    let before = rows(&registry)?;
    let chosen = pick(&before, provider, account)?;
    for row in &chosen {
        pool::set_enabled(&row.entry.provider, row.entry.account_id.as_deref(), on)?;
    }
    let label = ui::provider_name(&chosen[0].entry.provider);
    let what = match (account, chosen.len()) {
        (Some(account), _) => format!("{label} `{account}`"),
        (None, 1) => label.to_string(),
        (None, count) => format!("{label} ({count} accounts)"),
    };
    println!("{what} is {}.\n", if on { "on" } else { "off" });
    print_rows(&rows(&registry)?);
    Ok(())
}

/// `cc-proxy pool move <provider> [account] up|down`: one place at a time.
pub fn shift(provider: &str, rest: &[String]) -> Result<()> {
    let (account, direction) = match rest {
        [direction] => (None, direction),
        [account, direction] => (Some(account.as_str()), direction),
        _ => bail!("Say which way to move it: `cc-proxy pool move <provider> [account] up|down`."),
    };
    let offset = match direction.trim().to_ascii_lowercase().as_str() {
        "up" => -1,
        "down" => 1,
        other => bail!("`{other}` is not a direction. Use `up` or `down`."),
    };
    let registry = Registry::new();
    let before = rows(&registry)?;
    let chosen = pick(&before, provider, account)?;
    let [row] = chosen[..] else {
        let names: Vec<&str> = chosen.iter().map(|row| row.account.as_str()).collect();
        bail!(
            "{} has {} accounts: {}. Say which one: `cc-proxy pool move {} <account> {direction}`.",
            ui::provider_name(&chosen[0].entry.provider),
            chosen.len(),
            names.join(", "),
            chosen[0].entry.provider
        );
    };
    let at = pool::move_by(&row.entry.provider, row.entry.account_id.as_deref(), offset)?;
    let label = ui::provider_name(&row.entry.provider);
    let who = if row.account.is_empty() {
        label.to_string()
    } else {
        format!("{label} `{}`", row.account)
    };
    println!("{who} is now number {}.\n", at + 1);
    print_rows(&rows(&registry)?);
    Ok(())
}
