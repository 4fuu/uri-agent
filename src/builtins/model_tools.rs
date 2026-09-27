use crate::plugin::{ModelTool, ModelToolDescriptor, ModelToolOutput, Plugin, PluginHost};
use crate::prompts;
use crate::protocol::{ProtocolRegistry, RequestHeader};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

const BEGIN_LINE: &str = "*** Begin Request";
const END_LINE: &str = "*** End Request";
const BODY_LINE: &str = "*** Body:";
const MAX_REQUESTS: usize = 8;
const RESERVED_HEADER_NAMES: [&str; 5] = ["read", "exec", "begin", "end", "body"];
const CORRECT_FORM: &str = "*** Begin Request\n*** Read: <protocol>://<target>\n<optional body lines; omit this line when there is no body>\n*** End Request";

#[derive(Clone, Copy, Debug)]
enum ProtocolOperation {
    Read,
    Exec,
}

struct ProtocolTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolArguments {
    requests: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelpArguments {
    protocols: Vec<String>,
}

struct HelpTool;

#[derive(Debug)]
struct ParsedRequest {
    operation: ProtocolOperation,
    uri: String,
    headers: Vec<RequestHeader>,
    body: String,
}

/// Parses the fixed protocol request format:
///
/// ```text
/// *** Begin Request
/// *** Read: <protocol>://<target>
/// *** name: value
/// *** Body:
/// <raw body lines>
/// *** End Request
/// ```
///
/// After the operation line, a run of `*** name: value` header lines carries
/// per-protocol options. When any header is present, a `*** Body:` line must
/// separate the headers from the body; without headers the separator is
/// optional and simply skipped. The request ends at the last
/// `*** End Request` line, so the body may itself contain that line. Every
/// line after the separator (or the operation line) up to the final
/// `*** End Request` line is the raw body, passed verbatim and never
/// escaped. The newline that terminates the last body line belongs to the
/// format, not the body, so a body of exactly one empty line is empty and
/// one extra empty line ends the body with a newline.
fn parse_request(request: &str) -> Result<ParsedRequest> {
    let mut lines = request.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if trim_structural(first) != BEGIN_LINE {
        bail!(
            "invalid protocol request: the first line must be `{BEGIN_LINE}`; correct form:\n{CORRECT_FORM}"
        );
    }
    let (operation, uri) = parse_operation_line(lines.next().unwrap_or_default())?;
    let rest: Vec<&str> = lines.collect();
    let Some(end) = rest
        .iter()
        .rposition(|line| trim_structural(line) == END_LINE)
    else {
        bail!(
            "invalid protocol request: missing `{END_LINE}`; append the final marker after the operation line and all body lines; correct form:\n{CORRECT_FORM}"
        );
    };
    if rest[end + 1..]
        .iter()
        .any(|line| !trim_structural(line).is_empty())
    {
        bail!("invalid protocol request: unexpected text after `{END_LINE}`");
    }
    let content = &rest[..end];
    let mut headers = Vec::new();
    let mut index = 0;
    while let Some(line) = content.get(index) {
        let trimmed = trim_structural(line);
        if trimmed == BODY_LINE {
            break;
        }
        let Some(header) = parse_header_line(trimmed)? else {
            break;
        };
        headers.push(header);
        index += 1;
    }
    let body_lines: &[&str] = if headers.is_empty() {
        // Without headers a leading `*** Body:` line separates an empty
        // header section and is skipped.
        if index == 0
            && content
                .first()
                .is_some_and(|line| trim_structural(line) == BODY_LINE)
        {
            &content[1..]
        } else {
            content
        }
    } else if content
        .get(index)
        .is_some_and(|line| trim_structural(line) == BODY_LINE)
    {
        &content[index + 1..]
    } else if index == content.len() {
        // Headers running directly into the end line: no body.
        &[]
    } else {
        bail!(
            "invalid protocol request: the header section must end with a `{BODY_LINE}` line before the body"
        );
    };
    let mut body = String::new();
    for line in body_lines {
        body.push_str(line);
    }
    // The newline that terminates the last body line belongs to the format,
    // not to the body. A body of exactly one empty line is therefore empty;
    // one extra empty line ends the body with a newline.
    if let Some(stripped) = body.strip_suffix('\n') {
        body.truncate(stripped.len());
        if body.ends_with('\r') {
            body.pop();
        }
    }
    Ok(ParsedRequest {
        operation,
        uri,
        headers,
        body,
    })
}

/// Parses one `*** name: value` header line. Returns `Ok(None)` when the
/// line is not header-shaped: no `*** ` prefix, no colon, or whitespace
/// before the colon.
fn parse_header_line(line: &str) -> Result<Option<RequestHeader>> {
    let Some(rest) = line.strip_prefix("*** ") else {
        return Ok(None);
    };
    let Some((name, value)) = rest.split_once(':') else {
        return Ok(None);
    };
    if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Ok(None);
    }
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        || !characters.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "invalid protocol request: invalid header name `{name}`; header names are ASCII \
             letters, digits, `-`, and `_`, and start with a letter"
        );
    }
    let normalized = name.to_ascii_lowercase();
    if RESERVED_HEADER_NAMES.contains(&normalized.as_str()) {
        bail!(
            "invalid protocol request: `*** {name}:` is a structural line, not a header; add a \
             `{BODY_LINE}` line before the body to keep it as body text"
        );
    }
    Ok(Some(RequestHeader::new(
        &normalized,
        value.trim_matches([' ', '\t']),
    )))
}

fn parse_operation_line(line: &str) -> Result<(ProtocolOperation, String)> {
    let trimmed = trim_structural(line);
    for (prefix, operation) in [
        ("*** Read: ", ProtocolOperation::Read),
        ("*** Exec: ", ProtocolOperation::Exec),
    ] {
        if let Some(uri) = trimmed.strip_prefix(prefix) {
            return Ok((operation, uri.to_string()));
        }
    }
    bail!(
        "invalid protocol request: the second line must be `*** Read: <protocol>://<target>` or \
         `*** Exec: <protocol>://<target>`; correct form:\n{CORRECT_FORM}"
    )
}

fn trim_structural(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r', ' ', '\t'])
}

#[async_trait]
impl ModelTool for ProtocolTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "protocol".to_string(),
            description: prompts::PROTOCOL_TOOL_DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "requests": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "maxItems": 8,
                        "description": "One to eight fixed-format requests, executed in order; requests in one call must be independent, because no request can use another's result. Each request is a `*** Begin Request` line, one `*** Read: <protocol>://<target>` or `*** Exec: <protocol>://<target>` line, optional `*** name: value` header lines, a `*** Body:` line (required when headers are present, skipped otherwise), raw body lines, and a final `*** End Request` line. The request ends at the last `*** End Request` line; the lines between the header section (or the operation line) and it are the request body, passed verbatim and never escaped, so a body line exactly matching `*** End Request` can be sent. The newline before the final `*** End Request` line belongs to the request format, not the body: add one extra empty line before it to end the body with a newline. Omit the body when the operation takes no body. Header names are ASCII letters, digits, `-`, and `_`; a protocol's help page lists the headers it accepts, and comparable numeric headers accept a comparison prefix such as `>=10`. When a protocol's help page requires JSON, that JSON is the request body. Structural lines must match exactly. Invoke every registered protocol only through this tool with its `<protocol>://` address; a protocol loaded through `help` never becomes a callable tool under its own name. With more than one request the results arrive in order as `*** Result <n> of <m>: ok` or `*** Result <n> of <m>: error` sections; one failing request does not affect the others. Examples:\n\n*** Begin Request\n*** Read: file://src/main.rs\n*** End Request\n\n*** Begin Request\n*** Exec: pwsh://run\ncargo test\n*** End Request\n\n*** Begin Request\n*** Read: search://src\n*** mode: hybrid\n*** limit: >=10\n*** Body:\ncredential refresh flow\n*** End Request"
                    }
                },
                "required": ["requests"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: ProtocolArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| anyhow!("invalid protocol arguments: {error}"))?;
        if arguments.requests.is_empty() {
            bail!("invalid protocol arguments: at least one request is required");
        }
        if arguments.requests.len() > MAX_REQUESTS {
            bail!(
                "invalid protocol arguments: at most {MAX_REQUESTS} requests per call, got {}",
                arguments.requests.len()
            );
        }
        if arguments.requests.len() == 1 {
            return Self::execute_one(&arguments.requests[0], protocols).await;
        }
        let total = arguments.requests.len();
        let mut sections = String::new();
        let mut images = Vec::new();
        for (index, request) in arguments.requests.iter().enumerate() {
            if !sections.is_empty() {
                sections.push_str("\n\n");
            }
            match Self::execute_one(request, protocols).await {
                Ok(output) => {
                    let (text, mut read_images) = output.into_parts();
                    sections.push_str(&format!("*** Result {} of {total}: ok\n{text}", index + 1));
                    images.append(&mut read_images);
                }
                Err(error) => {
                    sections.push_str(&format!(
                        "*** Result {} of {total}: error\n{error:#}",
                        index + 1
                    ));
                }
            }
        }
        Ok(ModelToolOutput::new(sections, images))
    }
}

impl ProtocolTool {
    async fn execute_one(request: &str, protocols: &ProtocolRegistry) -> Result<ModelToolOutput> {
        let parsed = parse_request(request)?;
        match parsed.operation {
            ProtocolOperation::Read => {
                let result = protocols
                    .read_for_model(&parsed.uri, &parsed.headers, &parsed.body)
                    .await?;
                Ok(ModelToolOutput::new(result.output, result.images))
            }
            ProtocolOperation::Exec => protocols
                .exec_for_model(&parsed.uri, &parsed.headers, &parsed.body)
                .await
                .map(Into::into),
        }
    }
}

#[async_trait]
impl ModelTool for HelpTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "help".to_string(),
            description: prompts::HELP_TOOL_DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "protocols": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Names of protocols to load from the Available protocols list, for example [\"file\", \"search\"]. Shared prerequisites such as the MCP routing page are included automatically."
                    }
                },
                "required": ["protocols"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: HelpArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| anyhow!("invalid help arguments: {error}"))?;
        Ok(protocols.load_help(&arguments.protocols).await?.into())
    }
}

pub(super) struct ProtocolToolsPlugin;

pub(crate) fn register_protocol_tools(
    registry: &mut crate::plugin::ModelToolRegistry,
) -> Result<()> {
    registry.register(ProtocolTool)?;
    registry.register(HelpTool)
}

impl Plugin for ProtocolToolsPlugin {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        [ProtocolTool.descriptor(), HelpTool.descriptor()].to_vec()
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        register_protocol_tools(host.model_tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputStore;
    use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
    use crate::task::TaskManager;

    struct CaptureProtocol;

    #[async_trait]
    impl Protocol for CaptureProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "capture".to_string(),
                description: "Capture test protocol".to_string(),
                can_read: true,
                can_exec: true,
            }
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<Vec<u8>> {
            Ok(format!("read:{}", request.body).into_bytes())
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<Vec<u8>> {
            Ok(format!("exec:{}", request.body).into_bytes())
        }
    }

    async fn protocols() -> (ProtocolRegistry, std::path::PathBuf) {
        let session_id = format!("model-tools-{}", uuid::Uuid::now_v7().simple());
        let output = std::sync::Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let directory = output.directory().to_path_buf();
        let mut protocols = ProtocolRegistry::new(output, TaskManager::new());
        protocols.register(CaptureProtocol).unwrap();
        (protocols, directory)
    }

    #[test]
    fn protocol_tool_leaves_the_request_format_to_the_requests_parameter() {
        let descriptor = ProtocolTool.descriptor();
        assert_eq!(descriptor.name, "protocol");
        assert_eq!(descriptor.parameters["required"], json!(["requests"]));
        let requests = &descriptor.parameters["properties"]["requests"];
        assert_eq!(requests["type"], "array");
        assert_eq!(requests["minItems"], json!(1));
        assert_eq!(requests["maxItems"], json!(8));
        // The tool description states the purpose only; the requests
        // parameter is the single definition of the format.
        assert!(
            descriptor
                .description
                .contains("Call a registered protocol")
        );
        assert!(
            descriptor
                .description
                .contains("`requests` parameter defines the fixed request format")
        );
        assert!(
            !descriptor.description.contains("*** Begin Request"),
            "the tool description should not repeat the request format"
        );
        let request_description = requests["description"]
            .as_str()
            .expect("the requests parameter keeps its description");
        for fragment in [
            "a `*** Begin Request` line, one `*** Read: <protocol>://<target>` or `*** Exec: <protocol>://<target>` line",
            "optional `*** name: value` header lines",
            "a `*** Body:` line (required when headers are present, skipped otherwise)",
            "The request ends at the last `*** End Request` line",
            "passed verbatim and never escaped",
            "a body line exactly matching `*** End Request` can be sent",
            "The newline before the final `*** End Request` line belongs to the request format, not the body",
            "add one extra empty line before it to end the body with a newline",
            "Omit the body when the operation takes no body",
            "that JSON is the request body",
            "Structural lines must match exactly",
            "never becomes a callable tool under its own name",
            "*** Begin Request\n*** Read: file://src/main.rs\n*** End Request",
            "*** Begin Request\n*** Exec: pwsh://run\ncargo test\n*** End Request",
            "*** Begin Request\n*** Read: search://src\n*** mode: hybrid\n*** limit: >=10\n*** Body:\ncredential refresh flow\n*** End Request",
            "One to eight fixed-format requests, executed in order",
            "*** Result <n> of <m>: ok",
            "*** Result <n> of <m>: error",
            "one failing request does not affect the others",
        ] {
            assert!(
                request_description.contains(fragment),
                "requests description is missing: {fragment}"
            );
        }
        assert!(!request_description.contains("byte for byte"));
        assert!(!request_description.contains("Only the lines"));
    }

    #[test]
    fn parse_request_reads_without_a_body_section() {
        let parsed =
            parse_request("*** Begin Request\n*** Read: file://src/main.rs\n*** End Request")
                .unwrap();
        assert!(matches!(parsed.operation, ProtocolOperation::Read));
        assert_eq!(parsed.uri, "file://src/main.rs");
        assert!(parsed.headers.is_empty());
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn parse_request_parses_header_lines() {
        let parsed = parse_request(
            "*** Begin Request\n*** Read: search://src\n*** mode: hybrid\n*** Limit:  >=10 \n*** Body:\ncredential flow\n*** End Request",
        )
        .unwrap();
        assert_eq!(parsed.uri, "search://src");
        assert_eq!(
            parsed.headers,
            vec![
                RequestHeader::new("mode", "hybrid"),
                RequestHeader::new("limit", ">=10"),
            ]
        );
        assert_eq!(parsed.body, "credential flow");
    }

    #[test]
    fn parse_request_accepts_headers_without_a_body() {
        let parsed = parse_request(
            "*** Begin Request\n*** Read: file://src/main.rs\n*** limit: <=50\n*** End Request",
        )
        .unwrap();
        assert_eq!(parsed.headers, vec![RequestHeader::new("limit", "<=50")]);
        assert!(parsed.body.is_empty());
        let parsed = parse_request(
            "*** Begin Request\n*** Read: file://src/main.rs\n*** limit: <=50\n*** Body:\n*** End Request",
        )
        .unwrap();
        assert_eq!(parsed.headers, vec![RequestHeader::new("limit", "<=50")]);
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn parse_request_requires_a_body_separator_after_headers() {
        let error = parse_request(
            "*** Begin Request\n*** Read: search://src\n*** mode: hybrid\ncredential flow\n*** End Request",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must end with a `*** Body:` line"),
            "{error:#}"
        );
    }

    #[test]
    fn parse_request_rejects_structural_lines_as_headers() {
        let error = parse_request(
            "*** Begin Request\n*** Read: search://src\n*** Read: file://a\n*** End Request",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is a structural line, not a header"),
            "{error:#}"
        );
    }

    #[test]
    fn parse_request_rejects_invalid_header_names() {
        let error = parse_request(
            "*** Begin Request\n*** Read: search://src\n*** 1mode: hybrid\n*** End Request",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("invalid header name `1mode`"),
            "{error:#}"
        );
    }

    #[test]
    fn parse_request_keeps_header_shaped_body_lines_after_the_separator() {
        let parsed = parse_request(
            "*** Begin Request\n*** Exec: pwsh://run\n*** Body:\n*** Note: not a header\necho done\n*** End Request",
        )
        .unwrap();
        assert!(parsed.headers.is_empty());
        assert_eq!(parsed.body, "*** Note: not a header\necho done");
    }

    #[test]
    fn parse_request_keeps_multiline_bodies_verbatim() {
        let request =
            "*** Begin Request\n*** Exec: pwsh://run\nline one\n\nline \"three\"\n*** End Request";
        let parsed = parse_request(request).unwrap();
        assert!(matches!(parsed.operation, ProtocolOperation::Exec));
        assert_eq!(parsed.uri, "pwsh://run");
        assert_eq!(parsed.body, "line one\n\nline \"three\"");
    }

    #[test]
    fn parse_request_skips_the_body_separator_without_headers() {
        let parsed = parse_request(
            "*** Begin Request\r\n*** Exec: pwsh://run\r\n*** Body:\r\nline one\r\n*** End Request\r\n",
        )
        .unwrap();
        assert_eq!(parsed.uri, "pwsh://run");
        assert!(parsed.headers.is_empty());
        assert_eq!(parsed.body, "line one");
    }

    #[test]
    fn parse_request_accepts_crlf_lines_and_spaces_in_the_uri() {
        let parsed = parse_request(
            "*** Begin Request\r\n*** Read: file://my docs/a.md\r\n*** End Request\r\n",
        )
        .unwrap();
        assert_eq!(parsed.uri, "file://my docs/a.md");
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn parse_request_treats_exactly_one_empty_body_line_as_no_body() {
        for request in [
            "*** Begin Request\n*** Read: search://src\n\n*** End Request",
            "*** Begin Request\n*** Read: search://src\n*** Body:\n\n*** End Request",
        ] {
            let parsed = parse_request(request).unwrap();
            assert!(parsed.body.is_empty(), "{request:?}");
        }
    }

    #[test]
    fn parse_request_keeps_a_deliberate_trailing_newline() {
        // One extra empty line before the end line means the body itself
        // ends with a newline, which interactive `send` routes rely on.
        let parsed =
            parse_request("*** Begin Request\n*** Exec: tasks://001/send\nyes\n\n*** End Request")
                .unwrap();
        assert_eq!(parsed.body, "yes\n");
        let parsed =
            parse_request("*** Begin Request\n*** Exec: tasks://001/send\n\n\n*** End Request")
                .unwrap();
        assert_eq!(parsed.body, "\n");
    }

    #[test]
    fn parse_request_ends_at_the_last_end_line() {
        // The request ends at the last `*** End Request` line, so body
        // content may itself contain that line verbatim.
        let parsed = parse_request(
            "*** Begin Request\n*** Exec: tasks://001/send\n*** End Request\n*** End Request",
        )
        .unwrap();
        assert_eq!(parsed.body, "*** End Request");
        let parsed = parse_request(
            "*** Begin Request\n*** Exec: tasks://001/send\nfirst\n*** End Request\nlast\n*** End Request\n\n",
        )
        .unwrap();
        assert_eq!(parsed.body, "first\n*** End Request\nlast");
    }

    #[test]
    fn parse_request_rejects_malformed_requests() {
        for request in [
            "",
            "read file://src/main.rs",
            "*** Begin Request\n*** Read file://src/main.rs\n*** End Request",
            "*** Begin Request\n*** Read:\n*** End Request",
            "*** Begin Request\n*** Read: file://a\n",
            "*** Begin Request\n*** Read: file://a\n*** End Request\ntrailing",
        ] {
            let error = parse_request(request).unwrap_err();
            assert!(
                error.to_string().contains("invalid protocol request"),
                "{request:?} produced {error:#}"
            );
        }

        let error = parse_request("*** Begin Request\n*** Read: file://a\n").unwrap_err();
        assert!(error.to_string().contains(CORRECT_FORM));

        let error =
            parse_request("*** Begin Request\n*** Exec: file://a\nbody line\n").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("all body lines"));
        assert!(message.contains("<optional body lines"));
    }

    #[test]
    fn help_tool_describes_a_nonempty_protocol_list() {
        let descriptor = HelpTool.descriptor();
        assert_eq!(descriptor.name, "help");
        assert_eq!(descriptor.parameters["required"], json!(["protocols"]));
        assert_eq!(
            descriptor.parameters["properties"]["protocols"]["minItems"],
            json!(1)
        );
    }

    #[tokio::test]
    async fn help_tool_loads_protocols_and_unlocks_model_calls() {
        let (protocols, output) = protocols().await;
        let loaded = HelpTool
            .execute(&json!({"protocols": ["capture"]}), &protocols)
            .await
            .unwrap();
        assert!(loaded.output().contains("Loaded protocols stay loaded"));

        let read = ProtocolTool
            .execute(
                &json!({"requests": ["*** Begin Request\n*** Read: capture://value\n*** End Request"]}),
                &protocols,
            )
            .await
            .unwrap();
        let exec = ProtocolTool
            .execute(
                &json!({"requests": ["*** Begin Request\n*** Exec: capture://value\n{\"answer\":42}\n*** End Request"]}),
                &protocols,
            )
            .await
            .unwrap();

        assert_eq!(read.output(), "read:");
        assert_eq!(exec.output(), "exec:{\"answer\":42}");
        let newline = ProtocolTool
            .execute(
                &json!({"requests": ["*** Begin Request\n*** Exec: capture://value\nyes\n\n*** End Request"]}),
                &protocols,
            )
            .await
            .unwrap();
        assert_eq!(newline.output(), "exec:yes\n");
        let embedded_end = ProtocolTool
            .execute(
                &json!({"requests": ["*** Begin Request\n*** Exec: capture://value\n*** End Request\nbody\n*** End Request"]}),
                &protocols,
            )
            .await
            .unwrap();
        assert_eq!(embedded_end.output(), "exec:*** End Request\nbody");
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn protocol_tool_batches_requests_with_sectioned_results() {
        let (protocols, output) = protocols().await;
        HelpTool
            .execute(&json!({"protocols": ["capture"]}), &protocols)
            .await
            .unwrap();
        let result = ProtocolTool
            .execute(
                &json!({"requests": [
                    "*** Begin Request\n*** Read: capture://one\n*** End Request",
                    "*** Begin Request\n*** Exec: capture://two\nbody two\n*** End Request",
                    "not a request",
                ]}),
                &protocols,
            )
            .await
            .unwrap();
        assert_eq!(
            result.output(),
            "*** Result 1 of 3: ok\nread:\n\n*** Result 2 of 3: ok\nexec:body two\n\n*** Result 3 of 3: error\ninvalid protocol request: the first line must be `*** Begin Request`; correct form:\n*** Begin Request\n*** Read: <protocol>://<target>\n<optional body lines; omit this line when there is no body>\n*** End Request"
        );
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn protocol_tool_rejects_empty_and_oversized_batches() {
        let (protocols, output) = protocols().await;
        let error = ProtocolTool
            .execute(&json!({"requests": []}), &protocols)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("at least one request"));

        let oversized =
            vec!["*** Begin Request\n*** Read: capture://value\n*** End Request".to_string(); 9];
        let error = ProtocolTool
            .execute(&json!({"requests": oversized}), &protocols)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("at most 8 requests per call, got 9"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn help_tool_rejects_malformed_and_unknown_requests() {
        let (protocols, output) = protocols().await;
        let oversized: Vec<String> = ('a'..='j').map(|c| c.to_string()).collect();
        let error = HelpTool
            .execute(&json!({"protocols": oversized}), &protocols)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("unknown protocol: a"));

        let error = HelpTool
            .execute(&json!({"protocols": "capture"}), &protocols)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("invalid help arguments"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }

    #[tokio::test]
    async fn protocol_tool_rejects_malformed_requests() {
        let (protocols, output) = protocols().await;
        let error = ProtocolTool
            .execute(&json!({"requests": ["read capture://value"]}), &protocols)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("invalid protocol request"));

        let error = ProtocolTool
            .execute(&json!({"requests": 42}), &protocols)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("invalid protocol arguments"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }
}
