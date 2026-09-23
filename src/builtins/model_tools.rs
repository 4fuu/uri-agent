use crate::plugin::{ModelTool, ModelToolDescriptor, ModelToolOutput, Plugin, PluginHost};
use crate::prompts;
use crate::protocol::ProtocolRegistry;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

const BEGIN_LINE: &str = "*** Begin Request";
const END_LINE: &str = "*** End Request";
const LEGACY_BODY_LINE: &str = "*** Body:";
const CORRECT_FORM: &str = "*** Begin Request\n*** Read: <protocol>://<target>\n*** End Request";

#[derive(Clone, Copy, Debug)]
enum ProtocolOperation {
    Read,
    Exec,
}

struct ProtocolTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolArguments {
    request: String,
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
    body: String,
}

/// Parses the fixed protocol request format:
///
/// ```text
/// *** Begin Request
/// *** Read: <protocol>://<target>
/// <raw body lines>
/// *** End Request
/// ```
///
/// Every line between the operation line and `*** End Request` is the raw
/// body, passed verbatim and never escaped. A leading `*** Body:` line is
/// ignored so requests written for the earlier format still parse.
fn parse_request(request: &str) -> Result<ParsedRequest> {
    let mut lines = request.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if trim_structural(first) != BEGIN_LINE {
        bail!(
            "invalid protocol request: the first line must be `{BEGIN_LINE}`; correct form:\n{CORRECT_FORM}"
        );
    }
    let (operation, uri) = parse_operation_line(lines.next().unwrap_or_default())?;
    let mut body = String::new();
    let mut ended = false;
    let mut at_body_start = true;
    for line in lines {
        let trimmed = trim_structural(line);
        if ended {
            if !trimmed.is_empty() {
                bail!("invalid protocol request: unexpected text after `{END_LINE}`");
            }
            continue;
        }
        if trimmed == END_LINE {
            ended = true;
            continue;
        }
        if at_body_start && trimmed == LEGACY_BODY_LINE {
            at_body_start = false;
            continue;
        }
        at_body_start = false;
        body.push_str(line);
    }
    if !ended {
        bail!("invalid protocol request: missing `{END_LINE}`");
    }
    // The newline that terminates the last body line belongs to the format,
    // not to the body.
    if let Some(stripped) = body.strip_suffix('\n') {
        body.truncate(stripped.len());
        if body.ends_with('\r') {
            body.pop();
        }
    }
    if body
        .chars()
        .all(|character| character == '\n' || character == '\r')
    {
        body.clear();
    }
    Ok(ParsedRequest {
        operation,
        uri,
        body,
    })
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
                    "request": {
                        "type": "string",
                        "description": "Fixed-format request: a `*** Begin Request` line, one `*** Read: <protocol>://<target>` or `*** Exec: <protocol>://<target>` line, optional raw body lines, and a `*** End Request` line. The lines between the operation line and `*** End Request` are the request body, passed verbatim and never escaped. Leave no lines there when the operation takes no body. When a protocol's help page requires JSON, that JSON is the request body. The four structural lines must match byte for byte. Invoke every registered protocol only through this tool with its `<protocol>://` address; a protocol loaded through `help` never becomes a callable tool under its own name. Examples:\n\n*** Begin Request\n*** Read: file://src/main.rs\n*** End Request\n\n*** Begin Request\n*** Exec: pwsh://run\ncargo test\n*** End Request\n\nOnly the lines `*** Begin Request`, `*** Read:`, `*** Exec:`, and `*** End Request` are structural; every other line is body content passed verbatim."
                    }
                },
                "required": ["request"],
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
        let parsed = parse_request(&arguments.request)?;
        match parsed.operation {
            ProtocolOperation::Read => {
                let result = protocols.read_for_model(&parsed.uri, &parsed.body).await?;
                Ok(ModelToolOutput::new(result.output, result.images))
            }
            ProtocolOperation::Exec => protocols
                .exec(&parsed.uri, &parsed.body)
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
    fn protocol_tool_describes_the_fixed_request_format() {
        let descriptor = ProtocolTool.descriptor();
        assert_eq!(descriptor.name, "protocol");
        assert_eq!(descriptor.parameters["required"], json!(["request"]));
        assert_eq!(
            descriptor.parameters["properties"]["request"]["type"],
            "string"
        );
        for fragment in [
            "*** Begin Request",
            "*** Read: <protocol>://<target>",
            "*** Exec: <protocol>://<target>",
            "*** End Request",
            "optional raw body lines",
        ] {
            assert!(
                descriptor.description.contains(fragment),
                "description is missing: {fragment}"
            );
        }
        assert!(
            !descriptor.description.contains("*** Body:"),
            "the tool description should not teach the retired body header"
        );
        let request_description = descriptor.parameters["properties"]["request"]["description"]
            .as_str()
            .expect("the request parameter keeps its description");
        for fragment in [
            "Leave no lines there when the operation takes no body",
            "that JSON is the request body",
            "*** Begin Request\n*** Read: file://src/main.rs\n*** End Request",
            "*** Begin Request\n*** Exec: pwsh://run\ncargo test\n*** End Request",
            "must match byte for byte",
            "never becomes a callable tool under its own name",
        ] {
            assert!(
                request_description.contains(fragment),
                "request description is missing: {fragment}"
            );
        }
    }

    #[test]
    fn parse_request_reads_without_a_body_section() {
        let parsed =
            parse_request("*** Begin Request\n*** Read: file://src/main.rs\n*** End Request")
                .unwrap();
        assert!(matches!(parsed.operation, ProtocolOperation::Read));
        assert_eq!(parsed.uri, "file://src/main.rs");
        assert!(parsed.body.is_empty());
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
    fn parse_request_ignores_a_legacy_body_header() {
        let parsed = parse_request(
            "*** Begin Request\r\n*** Exec: pwsh://run\r\n*** Body:\r\nline one\r\n*** End Request\r\n",
        )
        .unwrap();
        assert_eq!(parsed.uri, "pwsh://run");
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
    fn parse_request_treats_a_newline_only_body_as_empty() {
        for request in [
            "*** Begin Request\n*** Read: search://src\n\n*** End Request",
            "*** Begin Request\n*** Read: search://src\n*** Body:\n\n*** End Request",
        ] {
            let parsed = parse_request(request).unwrap();
            assert!(parsed.body.is_empty(), "{request:?}");
        }
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
                &json!({"request": "*** Begin Request\n*** Read: capture://value\n*** End Request"}),
                &protocols,
            )
            .await
            .unwrap();
        let exec = ProtocolTool
            .execute(
                &json!({"request": "*** Begin Request\n*** Exec: capture://value\n{\"answer\":42}\n*** End Request"}),
                &protocols,
            )
            .await
            .unwrap();

        assert_eq!(read.output(), "read:");
        assert_eq!(exec.output(), "exec:{\"answer\":42}");
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
            .execute(&json!({"request": "read capture://value"}), &protocols)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("invalid protocol request"));

        let error = ProtocolTool
            .execute(&json!({"request": 42}), &protocols)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("invalid protocol arguments"));
        let _ = tokio::fs::remove_dir_all(output).await;
    }
}
