use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::constants::KIMI_CLI_VERSION;
use super::device_id::get_device_id;
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

/// The identity headers Kimi expects on every call. A value that cannot be
/// sent as a header (a configured user agent with a control character, say)
/// is left out rather than failing the request.
pub fn common_headers() -> Result<HeaderMap, anyhow::Error> {
    let device_id = get_device_id()?;
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
        ("x-msh-device-id", device_id),
        ("user-agent", user_agent),
    ];
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    Ok(headers)
}
