use super::failure::*;
use super::request_transform::*;
use super::rig_backend::*;
use super::*;
use crate::catalog::ModelLimits;
use crate::config::AuthKind;
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderValue};
use rig::completion::CompletionError;
use rig::message::Message;
use serde_json::{Value, json};
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WebSocketRequest, Response as WebSocketResponse,
};

fn catalog_model(api: &str, metadata: Value) -> CatalogModel {
    let mut model = CatalogModel {
        id: "test-model".to_string(),
        name: "Test".to_string(),
        api: api.to_string(),
        provider: "test-provider".to_string(),
        base_url: "https://example.test/v1".to_string(),
        headers: Default::default(),
        metadata: Default::default(),
    };
    model.metadata = metadata.as_object().unwrap().clone().into_iter().collect();
    model
}

fn transformed(model: CatalogModel, thinking: ThinkingLevel, body: Value) -> Value {
    let bytes = serde_json::to_vec(&body).unwrap();
    serde_json::from_slice(
        &ModelRequestTransform {
            model,
            thinking,
            session_id: None,
        }
        .transform_bytes(bytes::Bytes::from(bytes)),
    )
    .unwrap()
}

fn codex_access_token() -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-123"
            }
        }))
        .unwrap(),
    );
    format!("header.{payload}.signature")
}

fn codex_model(base_url: String) -> CatalogModel {
    let mut model = catalog_model(
        "openai-codex-responses",
        json!({
            "reasoning": true,
            "thinkingLevelMap": {"high": "xhigh"},
            "contextWindow": 128000,
            "maxTokens": 32768
        }),
    );
    model.id = "gpt-5.4".to_string();
    model.provider = "openai-codex".to_string();
    model.base_url = base_url;
    model
}

async fn server_once(
    status: &str,
    content_type: &str,
    body: String,
) -> (
    String,
    oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, request_rx) = oneshot::channel();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nX-Request-Id: codex-request-1\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let task = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0, "client closed before sending HTTP headers");
                request.extend_from_slice(&buffer[..count]);
                if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            if attempt == 0 {
                assert!(headers.to_ascii_lowercase().contains("upgrade: websocket"));
                stream
                        .write_all(
                            b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                continue;
            }
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or_default();
            while request.len() < header_end + content_length {
                let count = stream.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let _ = request_tx.send(String::from_utf8(request).unwrap());
            stream.write_all(response.as_bytes()).await.unwrap();
            break;
        }
    });
    (format!("http://{address}/backend-api"), request_rx, task)
}

fn request_json(request: &str) -> Value {
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

fn codex_completed_event(response_id: &str, message_id: &str, text: &str) -> Value {
    json!({
        "type": "response.completed",
        "sequence_number": 2,
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "model": "gpt-5.4",
            "usage": {
                "input_tokens": 4,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 1,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 5
            },
            "output": [{
                "type": "message",
                "id": message_id,
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "annotations": [], "text": text}]
            }],
            "tools": []
        }
    })
}

async fn read_codex_websocket_request(
    websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Value {
    let request = websocket.next().await.unwrap().unwrap();
    let WebSocketMessage::Text(request) = request else {
        panic!("expected WebSocket text request")
    };
    serde_json::from_str(&request).unwrap()
}

async fn send_codex_websocket_response(
    websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    response_id: &str,
    message_id: &str,
    text: &str,
) {
    websocket
        .send(WebSocketMessage::Text(
            json!({
                "type": "response.output_text.delta",
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "sequence_number": 1,
                "delta": text
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    websocket
        .send(WebSocketMessage::Text(
            codex_completed_event(response_id, message_id, text)
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}

async fn complete_codex_history(
    backend: &RigBackend,
    history: Vec<Message>,
) -> Result<ModelResponse> {
    let (deltas, _) = mpsc::unbounded_channel();
    backend
        .complete(
            ModelRequest {
                system: "system".to_string(),
                history,
                tools: false,
                estimated_context: 0,
                max_output_tokens: None,
            },
            deltas,
        )
        .await
}

#[test]
fn default_auth_client_is_a_pass_through() {
    let request = http::Request::builder()
        .header("x-api-key", "test-key")
        .body(bytes::Bytes::from_static(br#"{"input":"unchanged"}"#))
        .unwrap();

    let prepared = AuthClient::default().prepare(request);

    assert_eq!(prepared.headers()["x-api-key"], "test-key");
    assert_eq!(prepared.body(), br#"{"input":"unchanged"}"#.as_slice());
}

#[test]
fn configured_auth_client_applies_request_transforms() {
    let client = AuthClient {
        transform: Some(ModelRequestTransform {
            model: catalog_model("openai-responses", json!({})),
            thinking: ThinkingLevel::Off,
            session_id: None,
        }),
        ..Default::default()
    };
    let request = http::Request::builder()
        .body(bytes::Bytes::from_static(br#"{"input":"hello"}"#))
        .unwrap();

    let prepared = client.prepare(request);
    let body: Value = serde_json::from_slice(prepared.body()).unwrap();

    assert_eq!(body["input"], "hello");
    assert_eq!(body["store"], false);
}

#[test]
fn codex_request_transform_matches_pi_sse_contract() {
    let model = codex_model("https://chatgpt.com/backend-api".to_string());
    let session_id = "x".repeat(80);
    let transform = ModelRequestTransform {
        model,
        thinking: ThinkingLevel::High,
        session_id: Some(session_id),
    };
    let mut headers = HeaderMap::new();
    headers.insert("session_id", HeaderValue::from_static("random-rig-id"));
    transform.transform_headers(&mut headers);
    let body: Value = serde_json::from_slice(
        &transform.transform_bytes(bytes::Bytes::from(
            serde_json::to_vec(&json!({
                "model": "gpt-5.4",
                "tools": [{"type": "function", "name": "read"}]
            }))
            .unwrap(),
        )),
    )
    .unwrap();

    assert_eq!(headers["openai-beta"], "responses=experimental");
    assert_eq!(headers["session-id"], "x".repeat(64));
    assert_eq!(headers["x-client-request-id"], "x".repeat(64));
    assert!(!headers.contains_key("session_id"));
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["prompt_cache_key"], "x".repeat(64));
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["text"]["verbosity"], "low");
    assert_eq!(body["tools"][0]["strict"], Value::Null);
    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["reasoning"]["summary"], "auto");

    let off = transformed(
        codex_model("https://chatgpt.com/backend-api".to_string()),
        ThinkingLevel::Off,
        json!({}),
    );
    assert!(off.get("reasoning").is_none());
}

#[test]
fn codex_base_url_adds_the_backend_path_once() {
    assert_eq!(
        normalize_chatgpt_codex_base_url("https://chatgpt.com/backend-api"),
        "https://chatgpt.com/backend-api/codex"
    );
    assert_eq!(
        normalize_chatgpt_codex_base_url("https://chatgpt.com/backend-api/codex/"),
        "https://chatgpt.com/backend-api/codex"
    );
    assert_eq!(
        normalize_chatgpt_codex_base_url("https://chatgpt.com/backend-api/codex/responses/"),
        "https://chatgpt.com/backend-api/codex"
    );
    assert_eq!(
        normalize_chatgpt_codex_base_url(""),
        "https://chatgpt.com/backend-api/codex"
    );
}

#[tokio::test]
#[allow(clippy::result_large_err)] // Required by tungstenite's handshake callback result type.
async fn codex_websocket_reuses_connection_and_sends_only_new_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (handshake_tx, handshake_rx) = oneshot::channel();
    let (requests_tx, requests_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut handshake_tx = Some(handshake_tx);
        let mut websocket = accept_hdr_async(
            stream,
            move |request: &WebSocketRequest, response: WebSocketResponse| {
                let capture = (request.uri().clone(), request.headers().clone());
                let _ = handshake_tx.take().unwrap().send(capture);
                Ok(response)
            },
        )
        .await
        .unwrap();
        let mut requests = Vec::new();
        for (index, (response_id, message_id, text)) in [
            ("resp_1", "msg_1", "first answer"),
            ("resp_2", "msg_2", "second answer"),
        ]
        .into_iter()
        .enumerate()
        {
            requests.push(read_codex_websocket_request(&mut websocket).await);
            send_codex_websocket_response(&mut websocket, response_id, message_id, text).await;
            assert_eq!(index, requests.len() - 1);
        }
        let _ = requests_tx.send(requests);
        let _ = websocket.close(None).await;
    });
    let backend = RigBackend::new(
        &codex_model(format!("http://{address}/backend-api")),
        &codex_access_token(),
        &Default::default(),
        AuthKind::Oauth,
        ThinkingLevel::Off,
        Some("codex-reuse-session"),
    )
    .await
    .unwrap();
    let (deltas, _) = mpsc::unbounded_channel();
    let first = backend
        .complete(
            ModelRequest {
                system: "system".to_string(),
                history: vec![Message::user("first question")],
                tools: false,
                estimated_context: 0,
                max_output_tokens: None,
            },
            deltas,
        )
        .await
        .unwrap();
    let resumed_backend = RigBackend::new(
        &codex_model(format!("http://{address}/backend-api")),
        &codex_access_token(),
        &Default::default(),
        AuthKind::Oauth,
        ThinkingLevel::Off,
        Some("codex-reuse-session"),
    )
    .await
    .unwrap();
    let (deltas, _) = mpsc::unbounded_channel();
    let second = resumed_backend
        .complete(
            ModelRequest {
                system: "system".to_string(),
                history: vec![
                    Message::user("first question"),
                    Message::Assistant {
                        id: None,
                        content: first.content,
                    },
                    Message::user("second question"),
                ],
                tools: false,
                estimated_context: 0,
                max_output_tokens: None,
            },
            deltas,
        )
        .await
        .unwrap();
    let (uri, headers) = handshake_rx.await.unwrap();
    let requests = requests_rx.await.unwrap();
    server.await.unwrap();

    assert_eq!(uri.path(), "/backend-api/codex/responses");
    assert_eq!(headers["chatgpt-account-id"], "account-123");
    assert_eq!(headers["originator"], "pi");
    assert_eq!(headers["openai-beta"], "responses_websockets=2026-02-06");
    assert_eq!(headers["session-id"], "codex-reuse-session");
    assert_eq!(headers["x-client-request-id"], "codex-reuse-session");
    assert!(
        headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("Bearer header.")
    );
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["type"], "response.create");
    assert_eq!(requests[0]["store"], false);
    assert!(requests[0].get("previous_response_id").is_none());
    assert_eq!(
        requests[1]["previous_response_id"], "resp_1",
        "second request did not use continuation: {:#?}",
        requests[1]
    );
    assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
    assert_eq!(requests[1]["input"][0]["role"], "user");
    assert_eq!(
        requests[1]["input"][0]["content"][0]["text"],
        "second question"
    );
    assert!(second.content.iter().any(
        |content| matches!(content, AssistantContent::Text(text) if text.text == "second answer")
    ));
}

#[tokio::test]
async fn codex_websocket_retries_stale_continuation_with_full_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests_tx, requests_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let first = read_codex_websocket_request(&mut websocket).await;
        send_codex_websocket_response(&mut websocket, "resp_1", "msg_1", "first answer").await;
        let continuation = read_codex_websocket_request(&mut websocket).await;
        websocket
            .send(WebSocketMessage::Text(
                json!({"type": "codex.rate_limits", "remaining": 1})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        websocket
            .send(WebSocketMessage::Text(
                json!({
                    "type": "error",
                    "error": {
                        "code": "previous_response_not_found",
                        "message": "continuation expired"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let (stream, _) = listener.accept().await.unwrap();
        let mut retry_websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let retry = read_codex_websocket_request(&mut retry_websocket).await;
        send_codex_websocket_response(&mut retry_websocket, "resp_2", "msg_2", "second answer")
            .await;
        let _ = requests_tx.send(vec![first, continuation, retry]);
    });
    let backend = RigBackend::new(
        &codex_model(format!("http://{address}/backend-api")),
        &codex_access_token(),
        &Default::default(),
        AuthKind::Oauth,
        ThinkingLevel::Off,
        Some("codex-stale-session"),
    )
    .await
    .unwrap();
    let first = complete_codex_history(&backend, vec![Message::user("first question")])
        .await
        .unwrap();
    let second = complete_codex_history(
        &backend,
        vec![
            Message::user("first question"),
            Message::Assistant {
                id: None,
                content: first.content,
            },
            Message::user("second question"),
        ],
    )
    .await
    .unwrap();
    let requests = requests_rx.await.unwrap();
    server.await.unwrap();

    assert_eq!(requests[1]["previous_response_id"], "resp_1");
    assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
    assert!(requests[2].get("previous_response_id").is_none());
    let retry_input = requests[2]["input"].as_array().unwrap();
    assert_eq!(retry_input.len(), 3);
    assert_eq!(retry_input[0]["role"], "user");
    assert_eq!(retry_input[1]["role"], "assistant");
    assert_eq!(retry_input[2]["role"], "user");
    assert!(second.content.iter().any(
        |content| matches!(content, AssistantContent::Text(text) if text.text == "second answer")
    ));
}

#[tokio::test]
async fn codex_websocket_retries_connection_limit_on_a_new_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests_tx, requests_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let first = read_codex_websocket_request(&mut websocket).await;
        websocket
            .send(WebSocketMessage::Text(
                json!({
                    "type": "error",
                    "error": {
                        "code": "websocket_connection_limit_reached",
                        "message": "retry on another connection"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

        let (stream, _) = listener.accept().await.unwrap();
        let mut retry_websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let retry = read_codex_websocket_request(&mut retry_websocket).await;
        send_codex_websocket_response(&mut retry_websocket, "resp_1", "msg_1", "retried answer")
            .await;
        let _ = requests_tx.send(vec![first, retry]);
    });
    let backend = RigBackend::new(
        &codex_model(format!("http://{address}/backend-api")),
        &codex_access_token(),
        &Default::default(),
        AuthKind::Oauth,
        ThinkingLevel::Off,
        Some("codex-limit-session"),
    )
    .await
    .unwrap();
    let response = complete_codex_history(&backend, vec![Message::user("question")])
        .await
        .unwrap();
    let requests = requests_rx.await.unwrap();
    server.await.unwrap();

    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("previous_response_id").is_none());
    assert!(requests[1].get("previous_response_id").is_none());
    assert_eq!(requests[0]["input"], requests[1]["input"]);
    assert!(response.content.iter().any(
        |content| matches!(content, AssistantContent::Text(text) if text.text == "retried answer")
    ));
}

#[tokio::test]
async fn codex_websocket_does_not_fall_back_after_output_starts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (reconnected_tx, reconnected_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _request = read_codex_websocket_request(&mut websocket).await;
        websocket
            .send(WebSocketMessage::Text(
                json!({
                    "type": "response.output_text.delta",
                    "item_id": "msg_1",
                    "output_index": 0,
                    "content_index": 0,
                    "sequence_number": 1,
                    "delta": "partial"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        websocket.close(None).await.unwrap();
        let reconnected = tokio::time::timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_ok();
        let _ = reconnected_tx.send(reconnected);
    });
    let backend = RigBackend::new(
        &codex_model(format!("http://{address}/backend-api")),
        &codex_access_token(),
        &Default::default(),
        AuthKind::Oauth,
        ThinkingLevel::Off,
        Some("codex-no-replay-session"),
    )
    .await
    .unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        complete_codex_history(&backend, vec![Message::user("question")]),
    )
    .await
    .expect("Codex stream did not terminate after WebSocket close");
    let reconnected = reconnected_rx.await.unwrap();
    server.await.unwrap();

    assert!(result.is_err());
    assert!(!reconnected, "request was replayed after output started");
}

#[tokio::test]
async fn codex_backend_sends_oauth_request_and_streams_text_tools_and_usage() {
    let terminal_response = json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "model": "gpt-5.4",
        "usage": {
            "input_tokens": 11,
            "input_tokens_details": {"cached_tokens": 3},
            "output_tokens": 5,
            "output_tokens_details": {"reasoning_tokens": 2},
            "total_tokens": 16
        },
        "output": [
            {
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "annotations": [], "text": "你好"}]
            },
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{\"uri\":\"file://README.md\",\"body\":{\"kind\":\"none\",\"value\":\"\"}}",
                "status": "completed"
            }
        ],
        "tools": []
    });
    let events = [
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_1",
            "output_index": 0,
            "summary_index": 0,
            "sequence_number": 1,
            "delta": "检查"
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 1,
            "content_index": 0,
            "sequence_number": 2,
            "delta": "你"
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 1,
            "content_index": 0,
            "sequence_number": 3,
            "delta": "好"
        }),
        json!({
            "type": "response.output_item.added",
            "item_id": "fc_1",
            "output_index": 2,
            "sequence_number": 4,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1",
            "output_index": 2,
            "sequence_number": 5,
            "delta": "{\"uri\":\"file://README.md\",\"body\":{\"kind\":\"none\",\"value\":\"\"}}"
        }),
        json!({
            "type": "response.output_item.done",
            "item_id": "fc_1",
            "output_index": 2,
            "sequence_number": 6,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{\"uri\":\"file://README.md\",\"body\":{\"kind\":\"none\",\"value\":\"\"}}",
                "status": "completed"
            }
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 7,
            "response": terminal_response
        }),
    ];
    let mut sse = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    sse.push_str("data: [DONE]\n\n");
    let (base_url, request_rx, server) = server_once("200 OK", "text/event-stream", sse).await;
    let backend = RigBackend::new(
        &codex_model(base_url),
        &codex_access_token(),
        &Default::default(),
        AuthKind::Oauth,
        ThinkingLevel::High,
        Some("codex-sse-stream-session"),
    )
    .await
    .unwrap();
    let (deltas_tx, mut deltas_rx) = mpsc::unbounded_channel();

    let response = backend
        .complete(
            ModelRequest {
                system: "System instructions".to_string(),
                history: vec![Message::user("hello")],
                tools: true,
                estimated_context: 100,
                max_output_tokens: Some(4096),
            },
            deltas_tx,
        )
        .await
        .unwrap();
    let request = request_rx.await.unwrap();
    server.await.unwrap();
    let request_body = request_json(&request);
    let request_headers = request
        .split_once("\r\n\r\n")
        .unwrap()
        .0
        .to_ascii_lowercase();

    assert!(request.starts_with("POST /backend-api/codex/responses HTTP/1.1"));
    assert!(request_headers.contains("authorization: bearer header."));
    assert!(request_headers.contains("chatgpt-account-id: account-123"));
    assert!(request_headers.contains("originator: pi"));
    assert!(request_headers.contains("openai-beta: responses=experimental"));
    assert!(request_headers.contains("session-id: codex-sse-stream-session"));
    assert!(request_headers.contains("x-client-request-id: codex-sse-stream-session"));
    assert!(!request_headers.contains("session_id:"));
    assert!(request_headers.contains("user-agent: uri-agent/"));
    assert_eq!(request_body["model"], "gpt-5.4");
    assert_eq!(request_body["instructions"], "System instructions");
    assert_eq!(request_body["input"][0]["role"], "user");
    assert_eq!(request_body["tools"].as_array().unwrap().len(), 2);
    assert!(
        request_body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool.get("strict") == Some(&Value::Null))
    );
    assert_eq!(request_body["reasoning"]["effort"], "xhigh");
    assert_eq!(request_body["prompt_cache_key"], "codex-sse-stream-session");
    assert_eq!(request_body["store"], false);
    assert_eq!(request_body["stream"], true);
    assert_eq!(request_body["parallel_tool_calls"], true);
    assert_eq!(request_body["text"]["verbosity"], "low");
    assert!(request_body.get("max_output_tokens").is_none());

    let deltas = std::iter::from_fn(|| deltas_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(matches!(&deltas[0], ModelDelta::Reasoning(text) if text == "检查"));
    assert_eq!(
        deltas
            .iter()
            .filter_map(|delta| match delta {
                ModelDelta::Text(text) => Some(text.as_str()),
                ModelDelta::Reasoning(_) => None,
            })
            .collect::<String>(),
        "你好"
    );
    assert!(response.content.iter().any(|content| {
        matches!(content, AssistantContent::ToolCall(call)
        if call.function.name == "read"
            && call.function.arguments == json!({
                "uri": "file://README.md",
                "body": {"kind": "none", "value": ""}
            }))
    }));
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 8);
    assert_eq!(usage.cached_input_tokens, 3);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.reasoning_tokens, 2);
    assert_eq!(response.context_tokens, Some(16));
    assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
}

#[tokio::test]
async fn codex_backend_requires_oauth_and_preserves_subscription_errors() {
    let mut model = codex_model("https://chatgpt.com/backend-api".to_string());
    model.provider = "custom-codex".to_string();
    let error = RigBackend::new(
        &model,
        &codex_access_token(),
        &Default::default(),
        AuthKind::Oauth,
        ThinkingLevel::Off,
        Some("session-123"),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("openai-codex provider"));

    model.provider = "openai-codex".to_string();
    let error = RigBackend::new(
        &model,
        "ordinary-openai-api-key",
        &Default::default(),
        AuthKind::ApiKey,
        ThinkingLevel::Off,
        Some("session-123"),
    )
    .await
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("requires ChatGPT/Codex subscription OAuth")
    );

    for (index, (status, body, expected)) in [
        (
            "401 Unauthorized",
            r#"{"error":{"message":"expired access token"}}"#,
            ModelFailureKind::Authentication,
        ),
        (
            "429 Too Many Requests",
            r#"{"error":{"message":"Monthly usage limit reached"}}"#,
            ModelFailureKind::Quota,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (base_url, _request_rx, server) =
            server_once(status, "application/json", body.to_string()).await;
        let session_id = format!("codex-provider-error-{index}");
        let backend = RigBackend::new(
            &codex_model(base_url),
            &codex_access_token(),
            &Default::default(),
            AuthKind::Oauth,
            ThinkingLevel::Off,
            Some(&session_id),
        )
        .await
        .unwrap();
        let (deltas, _) = mpsc::unbounded_channel();
        let error = backend
            .complete(
                ModelRequest {
                    system: "system".to_string(),
                    history: vec![Message::user("hello")],
                    tools: false,
                    estimated_context: 0,
                    max_output_tokens: None,
                },
                deltas,
            )
            .await
            .err()
            .unwrap();
        server.await.unwrap();
        let failure = error.downcast_ref::<ModelFailure>().unwrap();
        assert_eq!(failure.kind(), expected);
        assert!(
            error
                .to_string()
                .contains(if expected == ModelFailureKind::Quota {
                    "Monthly usage limit reached"
                } else {
                    "expired access token"
                })
        );
    }
}

#[test]
fn max_tokens_follows_pi_catalog_and_context_clamping() {
    let limits = ModelLimits {
        context_window: 100_000,
        max_tokens: 32_000,
        cost: Default::default(),
    };
    assert_eq!(clamp_max_tokens_to_context(&limits, 10_000), 32_000);
    // 100_000 - 80_000 - 4_096 safety margin = 15_904 available.
    assert_eq!(clamp_max_tokens_to_context(&limits, 80_000), 15_904);
    // Never drops below one even when the estimate exceeds the window.
    assert_eq!(clamp_max_tokens_to_context(&limits, 200_000), 1);
    let unbounded = ModelLimits {
        context_window: 0,
        ..limits
    };
    assert_eq!(clamp_max_tokens_to_context(&unbounded, 200_000), 32_000);
}

#[test]
fn provider_usage_is_normalized_before_catalog_pricing() {
    let mut openai = rig::completion::Usage {
        input_tokens: 1_000,
        output_tokens: 200,
        cached_input_tokens: 300,
        cache_creation_input_tokens: 100,
        ..rig::completion::Usage::new()
    };
    normalize_usage_for_api("openai-responses", &mut openai);
    assert_eq!(openai.input_tokens, 600);
    assert_eq!(openai.output_tokens, 200);

    let mut gemini = rig::completion::Usage {
        input_tokens: 1_000,
        output_tokens: 200,
        cached_input_tokens: 300,
        reasoning_tokens: 50,
        ..rig::completion::Usage::new()
    };
    normalize_usage_for_api("google-generative-ai", &mut gemini);
    assert_eq!(gemini.input_tokens, 700);
    assert_eq!(gemini.output_tokens, 250);

    let mut anthropic = rig::completion::Usage {
        input_tokens: 600,
        output_tokens: 200,
        cached_input_tokens: 300,
        cache_creation_input_tokens: 100,
        ..rig::completion::Usage::new()
    };
    normalize_usage_for_api("anthropic-messages", &mut anthropic);
    assert_eq!(anthropic.input_tokens, 600);
    assert_eq!(anthropic.output_tokens, 200);
}

#[test]
fn provider_failure_keeps_status_retry_after_and_request_id() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after-ms", HeaderValue::from_static("1500"));
    headers.insert("x-request-id", HeaderValue::from_static("request-123"));
    let error = CompletionError::from_http_response_with_request_id(
        http::StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":"rate limited"}"#,
        Some("request-123".to_string()),
    )
    .with_response_headers(Some(Box::new(headers)));

    let failure = ModelFailure::from_completion_error(error, ModelFailurePhase::Request);

    assert_eq!(failure.kind(), ModelFailureKind::RateLimit);
    assert_eq!(failure.status(), Some(http::StatusCode::TOO_MANY_REQUESTS));
    assert_eq!(failure.retry_after(), Some(Duration::from_millis(1_500)));
    assert_eq!(failure.provider_request_id(), Some("request-123"));
}

#[test]
fn quota_and_stream_transport_errors_are_classified_separately() {
    let quota = CompletionError::from_http_response(
        http::StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"code":"insufficient_quota"}}"#,
    );
    assert_eq!(
        ModelFailure::from_completion_error(quota, ModelFailurePhase::Request).kind(),
        ModelFailureKind::Quota
    );

    let disconnected = CompletionError::ProviderError(
        "Network connection lost before the terminal event".to_string(),
    );
    let failure = ModelFailure::from_completion_error(disconnected, ModelFailurePhase::Stream);
    assert_eq!(failure.kind(), ModelFailureKind::Network);
    assert_eq!(failure.phase(), ModelFailurePhase::Stream);
}

#[test]
fn statusless_provider_envelopes_keep_transient_error_types() {
    for (body, expected) in [
        (
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ModelFailureKind::Server,
        ),
        (
            r#"{"type":"error","error":{"code":"server_error","message":"response failed"}}"#,
            ModelFailureKind::Server,
        ),
        (
            r#"{"type":"error","error":{"code":"rate_limit_exceeded","message":"slow down"}}"#,
            ModelFailureKind::RateLimit,
        ),
    ] {
        let failure = ModelFailure::from_completion_error(
            CompletionError::from_provider_body(body),
            ModelFailurePhase::Stream,
        );
        assert_eq!(failure.kind(), expected, "body: {body}");
    }
}

#[test]
fn whitespace_and_reasoning_without_an_answer_are_empty_responses() {
    assert!(!has_usable_assistant_content(&[
        AssistantContent::text(" \n"),
        AssistantContent::reasoning("unfinished thought"),
    ]));
    assert!(has_usable_assistant_content(&[AssistantContent::text(
        "answer"
    ),]));
}

#[test]
fn retry_after_accepts_seconds_and_http_dates() {
    let now = DateTime::parse_from_rfc2822("Wed, 21 Oct 2015 07:28:00 GMT")
        .unwrap()
        .with_timezone(&Utc);
    let mut headers = HeaderMap::new();
    headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("2.5"));
    assert_eq!(
        parse_retry_after_at(Some(&headers), now),
        Some(Duration::from_millis(2_500))
    );

    headers.insert(
        http::header::RETRY_AFTER,
        HeaderValue::from_static("Wed, 21 Oct 2015 07:28:30 GMT"),
    );
    assert_eq!(
        parse_retry_after_at(Some(&headers), now),
        Some(Duration::from_secs(30))
    );
}

#[test]
fn model_only_sees_two_tools_with_a_required_typed_body_envelope() {
    let tools = tool_definitions();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "exec"]
    );
    assert!(
        tools[1]
            .description
            .contains("Exact behavior is protocol-specific")
    );
    assert!(
        tools[1]
            .description
            .contains("return their final result directly")
    );
    for tool in tools {
        assert_eq!(tool.parameters["required"], json!(["uri", "body"]));
        let body = &tool.parameters["properties"]["body"];
        assert_eq!(body["type"], "object");
        assert_eq!(body["required"], json!(["kind", "value"]));
        assert_eq!(
            body["properties"]["kind"]["enum"],
            json!(["none", "text", "json"])
        );
        assert_eq!(body["properties"]["value"]["type"], "string");
        assert_eq!(body["additionalProperties"], false);
    }
}

#[test]
fn responses_uses_catalog_thinking_map_and_sampling_params() {
    let model = catalog_model(
        "openai-responses",
        json!({
            "reasoning": true,
            "thinkingLevelMap": {"high": "xhigh"},
            "samplingParams": {"service_tier": "priority"}
        }),
    );
    let body = transformed(
        model,
        ThinkingLevel::High,
        json!({"model": "test-model", "stream": true}),
    );
    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["service_tier"], "priority");
}

#[test]
fn completions_compat_renames_output_cap_and_controls_reasoning() {
    let model = catalog_model(
        "openai-completions",
        json!({
            "reasoning": true,
            "thinkingLevelMap": {"off": null, "low": "small"},
            "compat": {
                "maxTokensField": "max_tokens",
                "supportsReasoningEffort": true,
                "supportsUsageInStreaming": false
            }
        }),
    );
    let body = transformed(
        model,
        ThinkingLevel::Low,
        json!({
            "max_completion_tokens": 4096,
            "stream_options": {"include_usage": true}
        }),
    );
    assert_eq!(body["max_tokens"], 4096);
    assert!(body.get("max_completion_tokens").is_none());
    assert!(body.get("stream_options").is_none());
    assert_eq!(body["reasoning_effort"], "small");
}

#[test]
fn completions_cache_control_and_session_affinity_match_pi() {
    let model = catalog_model(
        "openai-completions",
        json!({
            "compat": {
                "cacheControlFormat": "anthropic",
                "sendSessionAffinityHeaders": true
            }
        }),
    );
    let transform = ModelRequestTransform {
        model: model.clone(),
        thinking: ThinkingLevel::Off,
        session_id: Some("session-1".to_string()),
    };
    let mut headers = HeaderMap::new();
    transform.transform_headers(&mut headers);
    assert_eq!(headers["session_id"], "session-1");
    assert_eq!(headers["x-client-request-id"], "session-1");
    assert_eq!(headers["x-session-affinity"], "session-1");

    let body = transformed(
        model,
        ThinkingLevel::Off,
        json!({
            "messages": [
                {"role": "system", "content": "instructions"},
                {"role": "user", "content": "hello"}
            ],
            "tools": [{"type": "function", "function": {"name": "read"}}]
        }),
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn anthropic_affinity_uses_only_its_documented_header() {
    let model = catalog_model(
        "anthropic-messages",
        json!({"compat": {"sendSessionAffinityHeaders": true}}),
    );
    let transform = ModelRequestTransform {
        model,
        thinking: ThinkingLevel::Off,
        session_id: Some("session-1".to_string()),
    };
    let mut headers = HeaderMap::new();
    transform.transform_headers(&mut headers);
    assert_eq!(headers["x-session-affinity"], "session-1");
    assert!(!headers.contains_key("session_id"));
    assert!(!headers.contains_key("x-client-request-id"));
}

#[tokio::test]
async fn anthropic_strict_tools_follow_catalog_compat() {
    for (supports, expected) in [(false, false), (true, true)] {
        let model = catalog_model(
            "anthropic-messages",
            json!({"compat": {"supportsStrictTools": supports}}),
        );
        let backend = RigBackend::new(
            &model,
            "test-key",
            &Default::default(),
            AuthKind::ApiKey,
            ThinkingLevel::Off,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            backend.client,
            RigClient::Anthropic(client) if client.strict_tools == expected
        ));
    }
}

#[test]
fn anthropic_force_adaptive_thinking_matches_pi_request_shape() {
    let model = catalog_model(
        "anthropic-messages",
        json!({
            "reasoning": true,
            "thinkingLevelMap": {"xhigh": "max"},
            "compat": {"forceAdaptiveThinking": true}
        }),
    );
    let body = transformed(
        model,
        ThinkingLevel::Xhigh,
        json!({"max_tokens": 16384, "temperature": 0.4}),
    );
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["thinking"]["display"], "summarized");
    assert_eq!(body["output_config"]["effort"], "max");
    assert!(body.get("temperature").is_none());
}

#[test]
fn gemini_uses_level_for_v3_and_budget_for_v25() {
    let mut v3 = catalog_model("google-generative-ai", json!({"reasoning": true}));
    v3.id = "gemini-3-flash".to_string();
    let v3_body = transformed(v3, ThinkingLevel::Medium, json!({}));
    assert_eq!(
        v3_body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "MEDIUM"
    );
    assert_eq!(
        v3_body["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );

    let mut v25 = catalog_model("google-generative-ai", json!({"reasoning": true}));
    v25.id = "gemini-2.5-pro".to_string();
    let v25_body = transformed(v25, ThinkingLevel::High, json!({}));
    assert_eq!(
        v25_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        32_768
    );

    let mut v31 = catalog_model(
        "google-generative-ai",
        json!({
            "reasoning": true,
            "thinkingLevelMap": {"off": null, "low": "LOW", "high": "HIGH"}
        }),
    );
    v31.id = "gemini-3.1-pro-preview".to_string();
    let v31_body = transformed(v31, ThinkingLevel::Low, json!({}));
    assert_eq!(
        v31_body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "LOW"
    );

    let mut flash = catalog_model("google-generative-ai", json!({"reasoning": true}));
    flash.id = "gemini-2.5-flash".to_string();
    let flash_body = transformed(flash, ThinkingLevel::Minimal, json!({}));
    assert_eq!(
        flash_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        128
    );

    let mut automatic = catalog_model("google-generative-ai", json!({"reasoning": true}));
    automatic.id = "deep-research-preview-04-2026".to_string();
    let automatic_body = transformed(automatic, ThinkingLevel::High, json!({}));
    assert_eq!(
        automatic_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        -1
    );
}

#[test]
fn antigravity_uses_numeric_route_budgets_and_v1internal_generation_fields() {
    let mut gemini = catalog_model(
        "antigravity",
        json!({
            "reasoning": true,
            "maxTokens": 65535,
            "thinkingLevelMap": {"off": null, "low": "low", "medium": "medium", "high": "high"},
            "compat": {
                "antigravityRoutes": {
                    "low": {"model": "gemini-3.1-pro-low", "thinkingBudget": 1001, "maxOutputTokens": 65535},
                    "medium": {"model": "gemini-pro-agent", "thinkingBudget": 10001, "maxOutputTokens": 65535},
                    "high": {"model": "gemini-pro-agent", "thinkingBudget": 10001, "maxOutputTokens": 65535}
                }
            }
        }),
    );
    gemini.id = "gemini-3.1-pro".to_string();
    let gemini_body = transformed(
        gemini,
        ThinkingLevel::High,
        json!({"generationConfig": {"topK": 20, "maxOutputTokens": 999999}}),
    );
    assert_eq!(
        gemini_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        10_001
    );
    assert!(
        gemini_body["generationConfig"]["thinkingConfig"]
            .get("thinkingLevel")
            .is_none()
    );
    assert_eq!(gemini_body["generationConfig"]["topK"], 20);
    assert_eq!(gemini_body["generationConfig"]["topP"], 1.0);
    assert_eq!(gemini_body["generationConfig"]["maxOutputTokens"], 65_535);

    let mut claude = catalog_model(
        "antigravity",
        json!({
            "reasoning": true,
            "maxTokens": 64000,
            "compat": {
                "antigravityRoutes": {
                    "medium": {"model": "claude-opus-4-6-thinking", "thinkingBudget": 16384, "maxOutputTokens": 64000}
                }
            }
        }),
    );
    claude.id = "claude-opus-4-6".to_string();
    let claude_body = transformed(claude, ThinkingLevel::Medium, json!({}));
    assert_eq!(
        claude_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        16_384
    );
    assert_eq!(
        claude_body["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
}
