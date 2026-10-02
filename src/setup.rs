//! `cc-proxy setup`: sign in to a provider, pick the models and, if you want,
//! add the shell hook. A line-based wizard: each question is one prompt, and
//! piped input answers it the same way a keyboard does.

use anyhow::{Context, Result, bail};

use crate::provider::{AuthState, KeyCheck};
use crate::registry::Registry;
use crate::shell::{self, HookStatus};
use crate::ui::{self, Mood};
use crate::{config, config_keys, prompt, providers};

/// Providers that sign in with a plan you already pay for.
const PLAN_PROVIDERS: [&str; 5] = ["codex", "kimi", "cursor", "grok", "copilot"];
/// Providers that take an API key.
const KEY_PROVIDERS: [&str; 2] = ["glm", "opencode"];

pub fn run() -> Result<()> {
    ui::print_note(
        Mood::Awake,
        &[
            "Let's set up cc-proxy.".into(),
            "You sign in, pick a model, and Claude Code is ready.".into(),
        ],
    );
    let registry = Registry::with_default_alias();
    let provider = pick_provider()?;
    let provider_name = ui::provider_name(provider);
    let cli = registry
        .provider(provider)
        .with_context(|| format!("{provider_name} isn't a known provider"))?
        .cli();

    let sign_in_again = match describe_state(&cli.auth_state()) {
        Some(state) => prompt::confirm(&format!("{provider_name}: {state}. Replace it?"), false)?,
        None => true,
    };
    if sign_in_again {
        if KEY_PROVIDERS.contains(&provider) {
            save_key(provider)?;
        } else {
            cli.login()?;
        }
    }

    let (main, fast) = pick_models(&registry, provider)?;
    config_keys::write_value("claude.model", main.clone().into())?;
    config_keys::write_value("claude.fastModel", fast.clone().into())?;

    let hook_ready = match shell::hook_status() {
        HookStatus::Installed => true,
        HookStatus::Installable => {
            let add = prompt::confirm(
                "Send plain `claude` through cc-proxy? This adds a hook to your shell startup file.",
                true,
            )?;
            if add {
                shell::install()?;
            }
            add
        }
        HookStatus::Unsupported => false,
    };
    let command = if hook_ready {
        "claude"
    } else {
        "cc-proxy claude"
    };
    let mut lines = vec![format!("Run `{command}` to start Claude Code on {main}.")];
    if fast != main {
        lines.push(format!("Its small background requests use {fast}."));
    }
    ui::print_note(Mood::Glad, &lines);
    Ok(())
}

fn pick_provider() -> Result<&'static str> {
    let kinds = ["A plan you already pay for", "An API key"].map(String::from);
    let ids = match prompt::choose("What do you sign in with?", &kinds, 0)? {
        0 => PLAN_PROVIDERS.as_slice(),
        _ => KEY_PROVIDERS.as_slice(),
    };
    let names: Vec<String> = ids
        .iter()
        .map(|id| ui::provider_name(id).to_string())
        .collect();
    Ok(ids[prompt::choose("Which provider?", &names, 0)?])
}

/// "signed in as ada", "an API key is set", or None when there's nothing.
fn describe_state(state: &AuthState) -> Option<String> {
    match state {
        AuthState::SignedIn { account, .. } => Some(match account {
            Some(account) => format!("signed in as {account}"),
            None => "signed in".to_string(),
        }),
        AuthState::KeySaved => Some("an API key is set".to_string()),
        AuthState::Missing => None,
    }
}

/// Reads the key, asks the provider whether it works, then saves it. Only a
/// refusal stops the save: a provider that's offline shouldn't block setup.
fn save_key(provider: &str) -> Result<()> {
    let name = ui::provider_name(provider);
    println!("Paste your {name} API key (input is hidden) and press Enter:");
    let key = prompt::read_hidden_line("API key: ")?;
    if key.is_empty() {
        bail!("no API key provided. Run `cc-proxy setup` again and paste your {name} key.");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let check = ui::waiting(&format!("checking the {name} key"), || {
        runtime.block_on(async {
            if provider == "glm" {
                providers::glm::check_key(&key).await
            } else {
                providers::opencode::check_key(&key).await
            }
        })
    });
    match check {
        KeyCheck::Rejected(reason) => bail!(
            "{name} refused the key ({reason}), so cc-proxy didn't save it. \
             Check the key in your {name} account, then run `cc-proxy setup` again."
        ),
        KeyCheck::Unverified(reason) => ui::print_note(
            Mood::Unsure,
            &[
                format!("cc-proxy couldn't check the {name} key: {reason}."),
                "It saved the key anyway. If requests fail with 401, run `cc-proxy setup` again."
                    .into(),
            ],
        ),
        KeyCheck::Accepted => {}
    }
    let path = if provider == "glm" {
        providers::glm::auth::save_glm_api_key(key)?;
        providers::glm::auth::auth_location()
    } else {
        config::save_opencode_api_key(&key)?.display().to_string()
    };
    println!("{name} key saved to {path}.");
    Ok(())
}

/// The main model, then the fast one. Enter takes the provider's first
/// model, then keeps the fast model the same.
fn pick_models(registry: &Registry, provider: &str) -> Result<(String, String)> {
    let models = registry.supported_models_for(provider);
    if models.is_empty() {
        bail!(
            "cc-proxy has no models listed for {}. Run `cc-proxy models` to see what it knows.",
            ui::provider_name(provider)
        );
    }
    // The list is sorted; the provider's own first model is its main one.
    let preferred = registry
        .provider(provider)
        .and_then(|handler| handler.supported_models().into_iter().next())
        .and_then(|model| models.iter().position(|listed| *listed == model))
        .unwrap_or(0);
    let main = prompt::choose("Which model should Claude Code use?", &models, preferred)?;
    let fast = prompt::choose(
        "And for its small background requests? Enter keeps the same model.",
        &models,
        main,
    )?;
    Ok((models[main].clone(), models[fast].clone()))
}
