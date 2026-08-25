use crate::plugin::{Plugin, PluginHost};
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Result, bail};
use async_trait::async_trait;
use std::fmt::Write as _;

const PROTOCOL_NAME: &str = "uri-agent-docs";
const DOCUMENTS: &[(&str, &str)] = &[
    ("README.md", include_str!("../../docs/README.md")),
    (
        "configuration.md",
        include_str!("../../docs/configuration.md"),
    ),
    ("context.md", include_str!("../../docs/context.md")),
    ("development.md", include_str!("../../docs/development.md")),
    ("interface.md", include_str!("../../docs/interface.md")),
    ("plugins.md", include_str!("../../docs/plugins.md")),
    ("protocols.md", include_str!("../../docs/protocols.md")),
    ("release.md", include_str!("../../docs/release.md")),
    ("sessions.md", include_str!("../../docs/sessions.md")),
    ("terminal.md", include_str!("../../docs/terminal.md")),
];

fn help() -> String {
    let mut output = String::from(
        r#"# uri-agent-docs

Read the version-matched URI Agent documentation embedded in this binary.
The documents are available regardless of the startup working directory.

- Read `uri-agent-docs://README.md` for the documentation index.
- Read `uri-agent-docs://<filename>` to load a document linked by the index.
- Targets are exact, case-sensitive filenames and do not accept paths or query parameters.
- Pass an empty string body. This protocol does not support `exec`.

Available documents:
"#,
    );
    for (name, _) in DOCUMENTS {
        let _ = writeln!(output, "- `{name}`");
    }
    output
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
    ) -> Result<Vec<u8>> {
        if !request.body.is_empty() {
            if request.target == "help" {
                bail!(
                    r#"uri-agent-docs://help requires an empty body; retry read("uri-agent-docs://help", "")"#
                );
            }
            bail!(
                "uri-agent-docs reads require an empty body; retry read({:?}, \"\")",
                request.uri
            );
        }
        if request.target == "help" {
            return Ok(help().into_bytes());
        }
        if let Some((_, content)) = DOCUMENTS.iter().find(|(name, _)| *name == request.target) {
            return Ok(content.as_bytes().to_vec());
        }
        bail!(
            r#"unknown {PROTOCOL_NAME} read target: {}; use read("{PROTOCOL_NAME}://help", "") for the exact filename list"#,
            request.target
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskManager;

    async fn read(target: &str) -> Result<Vec<u8>> {
        UriAgentDocsProtocol
            .read(
                ProtocolRequest {
                    uri: &format!("{PROTOCOL_NAME}://{target}"),
                    target,
                    body: "",
                },
                ProtocolContext {
                    tasks: TaskManager::new(),
                },
            )
            .await
    }

    #[tokio::test]
    async fn reads_embedded_documentation_and_reports_the_complete_index() {
        assert_eq!(
            read("README.md").await.unwrap(),
            include_bytes!("../../docs/README.md")
        );

        let help = String::from_utf8(read("help").await.unwrap()).unwrap();
        for (name, _) in DOCUMENTS {
            assert!(help.contains(&format!("`{name}`")));
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
}
