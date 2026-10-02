use crate::plugin::{Plugin, PluginHost};
use crate::protocol::{
    Protocol, ProtocolContext, ProtocolDescriptor, ProtocolOutput, ProtocolRequest,
};
use anyhow::{Result, bail};
use async_trait::async_trait;
use std::fmt::Write as _;

const PROTOCOL_NAME: &str = "uri-agent-docs";
const DOCUMENTS: &[(&str, &str)] = &[
    ("README.md", include_str!("../../docs/README.md")),
    ("acp.md", include_str!("../../docs/acp.md")),
    (
        "configuration.md",
        include_str!("../../docs/configuration.md"),
    ),
    ("context.md", include_str!("../../docs/context.md")),
    ("development.md", include_str!("../../docs/development.md")),
    ("interface.md", include_str!("../../docs/interface.md")),
    ("protocols.md", include_str!("../../docs/protocols.md")),
    ("release.md", include_str!("../../docs/release.md")),
    ("sessions.md", include_str!("../../docs/sessions.md")),
    ("terminal.md", include_str!("../../docs/terminal.md")),
];

fn help() -> String {
    let mut output = String::from(
        r#"# uri-agent-docs

Read the version-matched URI Agent documentation embedded in this binary.

- Read `uri-agent-docs://README.md` for the documentation index.
- Read `uri-agent-docs://<filename>` to load a document linked by the index.
- Targets are exact, case-sensitive filenames and do not accept paths.
- These reads take no input fields.

Available documents:
"#,
    );
    for (name, _) in DOCUMENTS {
        let _ = writeln!(output, "- `{name}`");
    }
    output
}

/// Render the `uri-agent docs` output: the topic listing when `topic` is
/// `None`, or the named document's content. The documents are the same
/// embedded set the [`uri-agent-docs`] protocol serves.
pub fn docs_output(topic: Option<&str>) -> Result<String> {
    let Some(topic) = topic else {
        let mut output = String::from(
            "Documentation embedded in this binary; print one with `uri-agent docs <topic>`:\n\n",
        );
        for (name, _) in DOCUMENTS {
            let _ = writeln!(output, "  {name}");
        }
        let _ = writeln!(
            output,
            "\n`README.md` is the index; `uri-agent-docs://README.md` lists every topic."
        );
        return Ok(output);
    };
    match DOCUMENTS.iter().find(|(name, _)| *name == topic) {
        Some((_, content)) => Ok((*content).to_string()),
        None => bail!(
            "unknown documentation topic: {topic}; run `uri-agent docs` with no topic to list the exact filenames"
        ),
    }
}

#[derive(Clone, Copy)]
pub(super) struct UriAgentDocsProtocol;

impl Plugin for UriAgentDocsProtocol {
    fn protocol_descriptors(&self) -> Vec<ProtocolDescriptor> {
        vec![self.descriptor()]
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        host.protocols.register(*self)
    }
}

#[async_trait]
impl Protocol for UriAgentDocsProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: PROTOCOL_NAME.to_string(),
            description: "Read URI Agent documentation bundled with this binary.".to_string(),
            can_read: true,
            can_exec: false,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        request.reject_input()?;
        if request.target == "help" {
            return Ok(help().into());
        }
        if let Some((_, content)) = DOCUMENTS.iter().find(|(name, _)| *name == request.target) {
            return Ok(content.as_bytes().to_vec().into());
        }
        bail!(
            r#"unknown {PROTOCOL_NAME} read target: {}; call help(["{PROTOCOL_NAME}"]) for the exact filename list"#,
            request.target
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskManager;
    use serde_json::{Map, Value, json};

    async fn read(target: &str) -> Result<ProtocolOutput> {
        let input = Map::new();
        UriAgentDocsProtocol
            .read(
                ProtocolRequest {
                    uri: &format!("{PROTOCOL_NAME}://{target}"),
                    target,
                    input: &input,
                },
                ProtocolContext::new(TaskManager::new()),
            )
            .await
    }

    #[tokio::test]
    async fn reads_embedded_documentation_and_reports_the_complete_index() {
        assert_eq!(
            read("README.md").await.unwrap().text_bytes(),
            include_bytes!("../../docs/README.md")
        );

        let help = String::from_utf8(read("help").await.unwrap().text_bytes().to_vec()).unwrap();
        for (name, _) in DOCUMENTS {
            assert!(help.contains(&format!("`{name}`")));
        }
    }

    #[tokio::test]
    async fn every_document_linked_by_the_index_is_readable() {
        let index =
            String::from_utf8(read("README.md").await.unwrap().text_bytes().to_vec()).unwrap();
        let mut linked = Vec::new();
        let mut rest = index.as_str();
        while let Some(position) = rest.find("](") {
            rest = &rest[position + 2..];
            let target = rest.split(')').next().unwrap_or_default();
            if target.ends_with(".md") && !target.contains('/') {
                linked.push(target.to_string());
            }
        }
        assert!(!linked.is_empty(), "index links no documents");
        for target in linked {
            read(&target).await.unwrap_or_else(|error| {
                panic!("index links unreadable document {target}: {error}")
            });
        }
    }

    #[tokio::test]
    async fn rejects_paths_outside_the_embedded_document_set() {
        let error = read("../README.md").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown uri-agent-docs read target")
        );
    }

    #[tokio::test]
    async fn rejects_any_input_field() {
        let input: Map<String, Value> = serde_json::from_value(json!({"offset": 1})).unwrap();
        let error = UriAgentDocsProtocol
            .read(
                ProtocolRequest {
                    uri: "uri-agent-docs://README.md",
                    target: "README.md",
                    input: &input,
                },
                ProtocolContext::new(TaskManager::new()),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("takes no input fields"));
    }

    #[test]
    fn docs_output_lists_every_embedded_topic() {
        let listing = docs_output(None).unwrap();
        for (name, _) in DOCUMENTS {
            assert!(listing.contains(&format!("\n  {name}\n")), "{listing}");
        }
    }

    #[test]
    fn docs_output_prints_one_document_without_modification() {
        let document = docs_output(Some("configuration.md")).unwrap();
        assert_eq!(document, include_str!("../../docs/configuration.md"));
    }

    #[test]
    fn docs_output_rejects_unknown_topics() {
        let error = docs_output(Some("missing.md")).unwrap_err();
        assert!(error.to_string().contains("unknown documentation topic"));
    }
}
