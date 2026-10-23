use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::constants::KIMI_CLI_VERSION;
use crate::config;

fn device_model() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    format!("{} {}", os, arch)
}

fn ascii_only(value: &str, fallback: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|&c| c.is_ascii() && !c.is_control())
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}

/// The identity headers Kimi expects on every call. An invalid optional value
/// (a configured user agent with a control character, say) is left out. The
/// account's device ID is required and never replaced with another ID.
pub fn common_headers(device_id: &str) -> Result<HeaderMap, anyhow::Error> {
    if device_id.trim().is_empty() {
        anyhow::bail!("The saved Kimi sign-in has no device ID. Sign in again to replace it.");
    }
    let hostname_str = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let user_agent = config::kimi_user_agent(&format!("KimiCLI/{KIMI_CLI_VERSION}"));
    let pairs = [
        ("x-msh-platform", "kimi_cli".to_string()),
        ("x-msh-version", KIMI_CLI_VERSION.to_string()),
        ("x-msh-device-name", ascii_only(&hostname_str, "unknown")),
        ("x-msh-device-model", ascii_only(&device_model(), "unknown")),
        (
            "x-msh-os-version",
            ascii_only(std::env::consts::ARCH, "unknown"),
        ),
        ("user-agent", user_agent),
    ];
    let mut headers = HeaderMap::new();
    headers.insert("x-msh-device-id", HeaderValue::from_str(device_id)?);
    for (name, value) in pairs {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_use_only_the_selected_accounts_device_id() {
        assert_eq!(
            common_headers("device-first").unwrap()["x-msh-device-id"],
            "device-first"
        );
        assert_eq!(
            common_headers("device-second").unwrap()["x-msh-device-id"],
            "device-second"
        );
        assert!(common_headers("").is_err());
        assert!(common_headers("invalid\ndevice").is_err());
    }
}
