use anyhow::{Result, anyhow};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Result<Pkce> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow!("cannot generate PKCE verifier: {error}"))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

pub fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn decode(value: &str) -> String {
    let mut bytes = Vec::new();
    let characters: Vec<char> = value.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '%' if index + 2 < characters.len() => {
                let hex: String = characters[index + 1..=index + 2].iter().collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    bytes.push(byte);
                    index += 3;
                    continue;
                }
                bytes.extend(characters[index].to_string().as_bytes());
                index += 1;
            }
            '+' => {
                bytes.push(b' ');
                index += 1;
            }
            character => {
                bytes.extend(character.to_string().as_bytes());
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn form_pairs(query: &str) -> impl Iterator<Item = (String, String)> + '_ {
    query.split('&').filter_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        Some((decode(key), decode(value)))
    })
}

pub fn parse_authorization_input(input: &str) -> Option<(String, Option<String>)> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = http::Uri::try_from(value)
        && let Some(query) = url.query()
    {
        return code_from_pairs(query);
    }
    if value.contains('#') {
        let (code, state) = value.split_once('#')?;
        if code.is_empty() {
            return None;
        }
        return Some((code.to_string(), Some(state.to_string())));
    }
    if value.contains("code=") {
        return code_from_pairs(value);
    }
    Some((value.to_string(), None))
}

fn code_from_pairs(query: &str) -> Option<(String, Option<String>)> {
    let mut code = None;
    let mut state = None;
    for (key, item) in form_pairs(query) {
        match key.as_str() {
            "code" => code = Some(item),
            "state" => state = Some(item),
            _ => {}
        }
    }
    code.map(|code| (code, state))
}

pub fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("uri-agent/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

pub fn open_url(url: &str) {
    let url = url.to_string();
    std::thread::spawn(move || {
        let _ = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "start", "", &url])
                .status()
        } else if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(&url).status()
        } else {
            std::process::Command::new("xdg-open").arg(&url).status()
        };
    });
}

pub fn trusted_http_url(value: &str) -> Result<String> {
    let url =
        http::Uri::try_from(value.trim()).map_err(|_| anyhow!("untrusted verification URL"))?;
    match url.scheme_str() {
        Some("https") | Some("http") => Ok(url.to_string()),
        _ => Err(anyhow!("untrusted verification URL")),
    }
}

pub fn extra_string(extra: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<String> {
    extra
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub fn decode_b64(value: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| anyhow!("invalid OAuth client id"))?;
    String::from_utf8(bytes).map_err(|_| anyhow!("invalid OAuth client id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_input_accepts_url_code_and_hash() {
        let url =
            parse_authorization_input("http://localhost:53692/callback?code=abc&state=verifier")
                .unwrap();
        assert_eq!(url.0, "abc");
        assert_eq!(url.1.as_deref(), Some("verifier"));
        assert_eq!(
            parse_authorization_input("code=xyz&state=one").unwrap().0,
            "xyz"
        );
        assert_eq!(
            parse_authorization_input("token#state").unwrap(),
            ("token".to_string(), Some("state".to_string()))
        );
        assert_eq!(parse_authorization_input("plain").unwrap().0, "plain");
        assert!(parse_authorization_input("   ").is_none());
    }

    #[test]
    fn pkce_challenge_is_url_safe() {
        let pkce = generate_pkce().unwrap();
        assert!(pkce.verifier.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
        }));
        assert_ne!(pkce.verifier, pkce.challenge);
        assert_eq!(pkce.challenge.len(), 43);
    }
}
