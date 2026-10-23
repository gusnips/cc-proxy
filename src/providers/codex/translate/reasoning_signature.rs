use base64::Engine;
use serde_json::Value;

const PREFIX: &str = "ccp:codex:v2:";
const MAX_ACCOUNT_BYTES: usize = 64;
const MAX_ID_BYTES: usize = 4 * 1024;
const MAX_ENCRYPTED_CONTENT_BYTES: usize = 8 * 1024 * 1024;

/// Whether `signature` was produced by this proxy's Codex reasoning replay
/// encoder.
///
/// These signatures embed the opaque `encrypted_content` replay blob, so
/// traffic captures redact the whole value. Keeping the check beside the
/// encoder keeps the format definition in one place.
pub(crate) fn is_proxy_reasoning_signature(signature: &str) -> bool {
    signature.starts_with("ccp:codex:")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningReplay {
    pub id: String,
    pub encrypted_content: String,
}

#[derive(Debug, Clone, Default)]
pub struct PendingReasoning {
    id: Option<String>,
    encrypted_content: Option<String>,
}

impl PendingReasoning {
    pub fn capture(&mut self, item: &Value) {
        if let Some(id) = non_empty_string(item.get("id")) {
            self.id = Some(id.to_string());
        }
        if let Some(encrypted_content) = non_empty_string(item.get("encrypted_content")) {
            self.encrypted_content = Some(encrypted_content.to_string());
        }
    }

    pub fn replay(&self) -> Option<ReasoningReplay> {
        let replay = ReasoningReplay {
            id: self.id.clone()?,
            encrypted_content: self.encrypted_content.clone()?,
        };
        replay.is_valid().then_some(replay)
    }
}

impl ReasoningReplay {
    fn is_valid(&self) -> bool {
        valid_reasoning_id(&self.id)
            && !self.encrypted_content.is_empty()
            && self.encrypted_content.len() <= MAX_ENCRYPTED_CONTENT_BYTES
    }
}

pub fn encode_reasoning_signature(
    replay: &ReasoningReplay,
    account: Option<&str>,
) -> Option<String> {
    let account = account.filter(|account| valid_account(account))?;
    if !replay.is_valid() {
        return None;
    }
    let encoded_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(replay.id.as_bytes());
    Some(format!(
        "{PREFIX}{account}:{encoded_id}:{}",
        replay.encrypted_content
    ))
}

pub(crate) fn signature_account(signature: &str) -> Option<&str> {
    let (account, _) = signature.strip_prefix(PREFIX)?.split_once(':')?;
    valid_account(account).then_some(account)
}

fn valid_account(account: &str) -> bool {
    account.len() <= MAX_ACCOUNT_BYTES && valid_reasoning_id(account)
}

pub fn decode_reasoning_signature(signature: &str) -> Option<ReasoningReplay> {
    let payload = signature.strip_prefix(PREFIX)?;
    if payload.is_empty() || payload.len() > max_payload_len() {
        return None;
    }
    let (account, payload) = payload.split_once(':')?;
    if !valid_account(account) {
        return None;
    }
    let (encoded_id, encrypted_content) = payload.split_once(':')?;
    if encoded_id.is_empty()
        || encoded_id.len() > encoded_id_len_limit()
        || encrypted_content.is_empty()
        || encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES
    {
        return None;
    }
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded_id)
        .ok()?;
    let id = String::from_utf8(id).ok()?;
    if !valid_reasoning_id(&id) {
        return None;
    }
    Some(ReasoningReplay {
        id,
        encrypted_content: encrypted_content.to_string(),
    })
}

fn valid_reasoning_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn encoded_id_len_limit() -> usize {
    MAX_ID_BYTES.div_ceil(3) * 4
}

fn max_payload_len() -> usize {
    MAX_ACCOUNT_BYTES + 1 + encoded_id_len_limit() + 1 + MAX_ENCRYPTED_CONTENT_BYTES
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signature_round_trip_preserves_reasoning_identity() {
        let replay = ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "gAAAAopaque".to_string(),
        };
        let signature = encode_reasoning_signature(&replay, Some("account-a")).unwrap();
        assert!(signature.starts_with(PREFIX));
        assert!(signature.ends_with(":gAAAAopaque"));
        assert_eq!(decode_reasoning_signature(&signature), Some(replay));
    }

    #[test]
    fn composite_ids_are_not_encoded_or_replayed_from_existing_signatures() {
        let id = "rs_6adb18e156be73a92dc249a9:rs_01a167d93049718084aceff8309d5b7a";
        assert!(
            encode_reasoning_signature(
                &ReasoningReplay {
                    id: id.into(),
                    encrypted_content: "opaque".into(),
                },
                Some("account-a")
            )
            .is_none()
        );
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id);
        let body: crate::anthropic::schema::MessagesRequest = serde_json::from_value(json!({
            "model":"gpt-5.6-sol",
            "messages":[{"role":"assistant","content":[{
                "type":"thinking","thinking":"visible summary",
                "signature":format!("{PREFIX}account-a:{encoded}:opaque")
            }]}]
        }))
        .unwrap();
        let translated = super::super::request::translate_request(
            &body,
            super::super::request::TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".into(),
                use_responses_lite: false,
            },
        )
        .unwrap();
        assert!(!translated.input.iter().any(|item| matches!(
            item,
            super::super::request::ResponsesInputItem::Reasoning { .. }
        )));
    }

    #[test]
    fn invalid_reasoning_ids_are_rejected_in_both_directions() {
        for id in ["", "rs:1", "rs.1", "rs 1", "rs_é", "rs_\n"] {
            let replay = ReasoningReplay {
                id: id.into(),
                encrypted_content: "opaque".into(),
            };
            assert!(
                encode_reasoning_signature(&replay, Some("account-a")).is_none(),
                "{id:?}"
            );
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id);
            assert!(
                decode_reasoning_signature(&format!("{PREFIX}account-a:{encoded}:opaque"))
                    .is_none(),
                "{id:?}"
            );
        }
    }

    #[test]
    fn foreign_and_malformed_signatures_are_ignored() {
        assert_eq!(decode_reasoning_signature("anthropic-signature"), None);
        assert_eq!(decode_reasoning_signature("ccp:codex:v1:not-base64"), None);
    }

    #[test]
    fn pending_reasoning_keeps_early_metadata_when_done_omits_it() {
        let mut pending = PendingReasoning::default();
        pending.capture(&json!({
            "id": "rs_1",
            "encrypted_content": "early"
        }));
        pending.capture(&json!({"id": "rs_1"}));
        assert_eq!(
            pending.replay(),
            Some(ReasoningReplay {
                id: "rs_1".to_string(),
                encrypted_content: "early".to_string(),
            })
        );
    }

    #[test]
    fn oversized_signature_is_ignored_without_decoding() {
        let signature = format!(
            "{PREFIX}account-a:cnNfMQ:{}",
            "A".repeat(MAX_ENCRYPTED_CONTENT_BYTES + 1)
        );
        assert_eq!(decode_reasoning_signature(&signature), None);
    }
}
