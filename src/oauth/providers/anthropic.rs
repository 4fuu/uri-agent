use super::super::callback;
use super::super::util::{decode_b64, encode, generate_pkce, http_client, open_url};
use super::super::{LoginSetup, OauthLogin, OauthToken, channels};
use super::shared::read_token_json;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use tokio::sync::oneshot;

const ANTHROPIC_CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";

pub(in crate::oauth) fn start_anthropic()
-> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let pkce = generate_pkce()?;
    let client_id = decode_b64(ANTHROPIC_CLIENT_ID_B64)?;
    let url = format!(
        "https://claude.ai/oauth/authorize?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        encode(&client_id),
        encode("http://localhost:53692/callback"),
        encode(
            "org:create_api_key user:[REDACTED:api-key] user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
        ),
        encode(&pkce.challenge),
        encode(&pkce.verifier)
    );
    let LoginSetup {
        login,
        mut paste_rx,
        mut cancel_rx,
        done_tx,
        done_rx,
        display: _,
    } = channels(
        url.clone(),
        None,
        "Complete login in the browser, or paste the redirect URL / code.",
    );
    open_url(&url);
    tokio::spawn(async move {
        let result = async {
            let callback = callback::bind(
                "127.0.0.1",
                53692,
                "/callback",
                "http://localhost:53692/callback",
                Some(&pkce.verifier),
                "Anthropic authentication completed. You can close this window.",
            )
            .await;
            let (code, state) = callback::race_callback_or_paste(
                &callback,
                &mut paste_rx,
                &mut cancel_rx,
                Some(&pkce.verifier),
            )
            .await?;
            anthropic_exchange(
                &code,
                state.as_deref().unwrap_or(&pkce.verifier),
                &pkce.verifier,
            )
            .await
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

pub(in crate::oauth) async fn refresh_anthropic(refresh: &str) -> Result<OauthToken> {
    anthropic_token(&[
        ("grant_type", "refresh_token"),
        ("client_id", &decode_b64(ANTHROPIC_CLIENT_ID_B64)?),
        ("refresh_token", refresh),
    ])
    .await
}

async fn anthropic_exchange(code: &str, state: &str, verifier: &str) -> Result<OauthToken> {
    anthropic_token(&[
        ("grant_type", "authorization_code"),
        ("client_id", &decode_b64(ANTHROPIC_CLIENT_ID_B64)?),
        ("code", code),
        ("state", state),
        ("redirect_uri", "http://localhost:53692/callback"),
        ("code_verifier", verifier),
    ])
    .await
}

async fn anthropic_token(body: &[(&str, &str)]) -> Result<OauthToken> {
    let response = http_client()?
        .post("https://platform.claude.com/v1/oauth/token")
        .header("Accept", "application/json")
        .json(&body.iter().copied().collect::<BTreeMap<_, _>>())
        .send()
        .await
        .context("Anthropic token request failed")?;
    read_token_json(response, "Anthropic").await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn client_id_matches_pi() {
        assert_eq!(
            decode_b64(ANTHROPIC_CLIENT_ID_B64).unwrap(),
            "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
        );
    }
}
