use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;
use url::Url;

use super::auth::{CLIENT_ID, StoredAuth, TOKEN_URL, TokenResponse};
use crate::providers::grok::auth::pkce::{PkceCodes, generate_pkce, generate_state};

const CALLBACK_PORT: u16 = 54545;
const MANUAL_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";

fn authorize_url(redirect: &str, pkce: &PkceCodes, state: &str) -> Result<Url> {
    let mut url = Url::parse("https://claude.ai/oauth/authorize")?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", "user:profile user:inference")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url)
}

pub fn sign_in(manual: bool) -> Result<StoredAuth> {
    let pkce = generate_pkce();
    let state = generate_state();
    let listener = if manual {
        None
    } else {
        TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).ok()
    };
    let redirect = if listener.is_some() {
        format!("http://localhost:{CALLBACK_PORT}/callback")
    } else {
        MANUAL_REDIRECT.to_string()
    };
    let url = authorize_url(&redirect, &pkce, &state)?;
    println!("Sign in with your Claude subscription:\n\n  {url}\n");
    if listener.is_none() {
        println!("After approving access, paste the code shown in your browser.");
    }
    let _ = crate::browser::open(url.as_str());
    let code = match listener {
        Some(listener) => wait_for_code(&listener, &state)?,
        None => pasted_code(
            &crate::prompt::read_hidden_line("Authorization code: ")?,
            &state,
        )?,
    };
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client.post(TOKEN_URL).json(&json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect,
        "code_verifier": pkce.verifier,
    })).send().context("Could not finish Claude sign-in. Check your connection and run `cc-proxy auth login claude` again.")?;
    if !response.status().is_success() {
        bail!(
            "Claude sign-in failed (HTTP {}). Run `cc-proxy auth login claude` again.",
            response.status()
        );
    }
    response.json::<TokenResponse>()?.into_auth(None)
}

fn pasted_code(raw: &str, expected_state: &str) -> Result<String> {
    let raw = raw.trim();
    let (code, state) = if let Ok(url) = Url::parse(raw) {
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        (
            params.get("code").cloned().unwrap_or_default(),
            params.get("state").cloned(),
        )
    } else {
        let (code, state) = raw
            .split_once('#')
            .map_or((raw, None), |(code, state)| (code, Some(state)));
        (code.trim().to_string(), state.map(str::to_string))
    };
    if code.is_empty()
        || state
            .as_deref()
            .is_some_and(|state| state != expected_state)
    {
        bail!(
            "The authorization code is empty or belongs to another sign-in. Run `cc-proxy auth login claude` again."
        );
    }
    Ok(code)
}

fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Some(code) = callback(&mut stream, state)? {
                    return Ok(code);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!("Claude sign-in timed out after 5 minutes. Run `cc-proxy auth login claude` again.");
}

fn callback(stream: &mut TcpStream, state: &str) -> Result<Option<String>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = [0; 8192];
    let Ok(length) = stream.read(&mut bytes) else {
        return Ok(None);
    };
    let request = String::from_utf8_lossy(&bytes[..length]);
    let mut first_line = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    if first_line.next() != Some("GET") {
        return Ok(None);
    }
    let Ok(url) = Url::parse(&format!(
        "http://localhost{}",
        first_line.next().unwrap_or_default()
    )) else {
        return Ok(None);
    };
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    if url.path() != "/callback" || params.get("state").map(String::as_str) != Some(state) {
        reply(
            stream,
            "400 Bad Request",
            "This sign-in does not match. Return to the terminal and try again.",
        );
        return Ok(None);
    }
    if params.contains_key("error") {
        reply(
            stream,
            "400 Bad Request",
            "Access was not approved. Return to the terminal to sign in again.",
        );
        bail!("Claude access was not approved. Run `cc-proxy auth login claude` again.");
    }
    let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
        reply(
            stream,
            "400 Bad Request",
            "No authorization code was received. Return to the terminal and try again.",
        );
        return Ok(None);
    };
    reply(
        stream,
        "200 OK",
        "Signed in to Claude. You can close this tab and return to the terminal.",
    );
    Ok(Some(code.clone()))
}

fn reply(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_uses_pkce_and_only_profile_and_inference_scopes() {
        let pkce = generate_pkce();
        let url = authorize_url("http://localhost:54545/callback", &pkce, "random-state").unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("claude.ai"));
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(params["scope"], "user:profile user:inference");
        assert_eq!(params["code_challenge"], pkce.challenge);
        assert_eq!(params["state"], "random-state");
        assert!(!url.as_str().contains(&pkce.verifier));
    }

    #[test]
    fn manual_codes_validate_state_when_provided() {
        assert_eq!(pasted_code("code#state", "state").unwrap(), "code");
        assert_eq!(
            pasted_code(
                "http://localhost:54545/callback?code=test&state=state",
                "state"
            )
            .unwrap(),
            "test"
        );
        assert!(pasted_code("code#other", "state").is_err());
        assert!(pasted_code("", "state").is_err());
    }
}
