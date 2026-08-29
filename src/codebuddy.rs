use crate::oauth::{
    CODEBUDDY_ACCOUNT_EXTRA, CODEBUDDY_DOMAIN_EXTRA, CODEBUDDY_ENDPOINT_EXTRA,
    CODEBUDDY_ENVIRONMENT_EXTRA, CODEBUDDY_METHOD_EXTRA, OauthToken, codebuddy_default_endpoint,
    normalize_codebuddy_endpoint,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};

pub(crate) const ENVIRONMENT_VARIABLE: &str = "CODEBUDDY_INTERNET_ENVIRONMENT";
pub(crate) const BASE_URL_VARIABLE: &str = "CODEBUDDY_BASE_URL";
pub(crate) const AUTH_TOKEN_VARIABLE: &str = "CODEBUDDY_AUTH_TOKEN";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Session {
    pub endpoint: String,
    pub domain: Option<String>,
    pub method: Option<String>,
    pub account: Option<Value>,
}

pub(crate) fn session_from_oauth(token: &OauthToken, base_url: Option<&str>) -> Result<Session> {
    let environment =
        extra_string(token, CODEBUDDY_ENVIRONMENT_EXTRA).unwrap_or_else(|| "external".to_string());
    let endpoint = base_url
        .map(str::to_string)
        .or_else(|| extra_string(token, CODEBUDDY_ENDPOINT_EXTRA))
        .or_else(|| codebuddy_default_endpoint(&environment).map(str::to_string))
        .ok_or_else(|| anyhow!("CodeBuddy OAuth session has no endpoint; run :login again"))?;
    Ok(Session {
        endpoint: normalize_codebuddy_endpoint(&endpoint)?,
        domain: extra_string(token, CODEBUDDY_DOMAIN_EXTRA),
        method: extra_string(token, CODEBUDDY_METHOD_EXTRA),
        account: token.extra.get(CODEBUDDY_ACCOUNT_EXTRA).cloned(),
    })
}

pub(crate) fn process_session_from(
    base_url: Option<String>,
    environment: Option<String>,
) -> Result<Session> {
    let endpoint = if let Some(value) = base_url.filter(|value| !value.trim().is_empty()) {
        normalize_codebuddy_endpoint(&value)?
    } else {
        let environment = environment
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "external".to_string());
        match environment.as_str() {
            "external" | "internal" => codebuddy_default_endpoint(&environment)
                .expect("built-in CodeBuddy environment has an endpoint")
                .to_string(),
            "selfhosted" => bail!("self-hosted CodeBuddy requires {BASE_URL_VARIABLE}"),
            "iOA" | "ioa" => bail!("CodeBuddy iOA is not supported"),
            other => bail!("unsupported CodeBuddy environment {other:?}"),
        }
    };
    Ok(Session {
        endpoint,
        domain: None,
        method: None,
        account: None,
    })
}

pub(crate) fn authenticated_headers(
    token: &str,
    api_key: bool,
    session: &Session,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    insert_header(
        &mut headers,
        http::header::AUTHORIZATION,
        &format!("Bearer {token}"),
        "CodeBuddy bearer token",
    )?;
    headers.insert(
        HeaderName::from_static("x-requested-with"),
        HeaderValue::from_static("XMLHttpRequest"),
    );
    if api_key {
        insert_static(&mut headers, "x-api-key", token, "CodeBuddy API key")?;
    }
    apply_session_headers(&mut headers, session)?;
    Ok(headers)
}

fn apply_session_headers(headers: &mut HeaderMap, session: &Session) -> Result<()> {
    if let Some(domain) = &session.domain {
        insert_static(headers, "x-domain", domain, "CodeBuddy domain")?;
    }
    let Some(account) = session.account.as_ref() else {
        return Ok(());
    };
    let uid = value_string(account, "uid");
    let enterprise = value_string(account, "enterpriseId");
    let department = value_string(account, "departmentFullName");
    let id_source = value_string(account, "idSource");
    if let Some(uid) = uid {
        insert_static(headers, "x-user-id", uid, "CodeBuddy user ID")?;
    }
    if let Some(enterprise) = enterprise {
        insert_static(
            headers,
            "x-enterprise-id",
            enterprise,
            "CodeBuddy enterprise ID",
        )?;
        insert_static(headers, "x-tenant-id", enterprise, "CodeBuddy tenant ID")?;
    }
    if let Some(department) = department {
        insert_static(
            headers,
            "x-department-info",
            department,
            "CodeBuddy department",
        )?;
    }
    if let Some(method) = &session.method {
        insert_static(headers, "x-auth-method", method, "CodeBuddy auth method")?;
    }
    if let Some(id_source) = id_source {
        insert_static(
            headers,
            "x-id-source",
            id_source,
            "CodeBuddy identity source",
        )?;
    }
    if let Some(uid) = uid
        && (enterprise.is_some() || id_source.is_some() || session.method.is_some())
    {
        let mut userinfo = Map::from_iter([("uin".to_string(), Value::String(uid.to_string()))]);
        if let Some(enterprise) = enterprise {
            userinfo.insert(
                "owner_uin".to_string(),
                Value::String(enterprise.to_string()),
            );
        }
        if let Some(id_source) = id_source {
            userinfo.insert(
                "id_source".to_string(),
                Value::String(id_source.to_string()),
            );
        }
        if let Some(method) = &session.method {
            userinfo.insert(
                "token_source".to_string(),
                Value::String(method.to_string()),
            );
        }
        insert_static(
            headers,
            "x-userinfo",
            &BASE64.encode(serde_json::to_vec(&userinfo)?),
            "CodeBuddy encoded user info",
        )?;
    }
    Ok(())
}

fn value_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn extra_string(token: &OauthToken, key: &str) -> Option<String> {
    token
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn insert_static(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
    label: &str,
) -> Result<()> {
    insert_header(headers, HeaderName::from_static(name), value, label)
}

fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
    label: &str,
) -> Result<()> {
    headers.insert(
        name,
        HeaderValue::from_str(value).with_context(|| format!("invalid {label} header value"))?,
    );
    Ok(())
}
