use super::super::callback;
use super::super::util::{encode, generate_pkce, http_client, open_url};
use super::super::{LoginSetup, OauthLogin, OauthToken, channels, set_display};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::oneshot;

pub(in crate::oauth) fn start_openrouter()
-> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let LoginSetup {
        login: login_holder,
        mut paste_rx,
        mut cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        String::new(),
        None,
        "Complete sign-in in the browser, or paste the redirect URL / code.",
    );
    tokio::spawn(async move {
        let result = async {
            let pkce = generate_pkce()?;
            let path = format!("/oauth/callback/{}", uuid::Uuid::now_v7().simple());
            let callback = callback::bind_ephemeral("127.0.0.1", &path, None, "Signed in to OpenRouter. You may now close this page.").await?;
            let authorize = format!("https://openrouter.ai/auth?callback_url={}&code_challenge={}&code_challenge_method=S256", encode(&callback.redirect_uri), encode(&pkce.challenge));
            set_display(&display, authorize.clone(), None, "Complete sign-in in the browser, or paste the redirect URL / code.");
            open_url(&authorize);
            let (code, _) = callback::race_callback_or_paste(&callback, &mut paste_rx, &mut cancel_rx, None).await?;
            openrouter_exchange(&code, &pkce.verifier).await
        }.await;
        let _ = done_tx.send(result);
    });
    Ok((login_holder, done_rx))
}

async fn openrouter_exchange(code: &str, verifier: &str) -> Result<OauthToken> {
    let response = http_client()?
        .post("https://openrouter.ai/api/v1/auth/keys")
        .header("Accept", "application/json")
        .json(&json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}))
        .send()
        .await
        .context("OpenRouter token request failed")?;
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("OpenRouter OAuth key exchange failed ({status}): {value}");
    }
    let key = value
        .get("key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("OpenRouter OAuth response carries no key"))?;
    Ok(OauthToken {
        kind: "oauth".to_string(),
        refresh: String::new(),
        access: key.to_string(),
        expires: i64::MAX / 4,
        extra: BTreeMap::new(),
    })
}
