use crate::config::display_path;
use std::fmt::Write as _;
use std::path::Path;

pub const READ_TOOL_DESCRIPTION: &str = "Read through a registered protocol. Use this for protocol help, resources, task status, and completed results.";

pub const EXEC_TOOL_DESCRIPTION: &str = "Execute through a registered protocol. Exact behavior is protocol-specific; read `<protocol>://help` before use. Operations normally return their final result directly. Long-running operations may become managed background tasks whose completion is delivered automatically; use the tasks protocol to inspect or cancel them.";

#[derive(Clone, Debug)]
pub struct ProtocolPrompt {
    pub name: String,
    pub description: String,
}

pub fn system_prompt(protocols: &[ProtocolPrompt], fragments: &[String]) -> String {
    let mut prompt = String::from(
        "You are a general-purpose agent running in URI Agent with exactly two tools: read and exec.\n\
         Use read to retrieve information and exec to perform actions through registered protocols.\n\
         Every tool call requires a body envelope. Use `{\"kind\":\"none\",\"value\":\"\"}` for no protocol body, `{\"kind\":\"text\",\"value\":\"...\"}` for a string body, or `{\"kind\":\"json\",\"value\":\"...\"}` with complete serialized JSON for any JSON body.\n\
         Angle-bracketed values in protocol addresses, such as <protocol>, are placeholders.\n\
         The same placeholder convention applies throughout protocol help.\n\
         Replace placeholders with the required values without including the angle brackets.\n\
         Before using a protocol for the first time, call read(\"<protocol>://help\", {\"kind\":\"none\",\"value\":\"\"}) to learn its contract.\n\
         Verify relevant results before claiming work is complete.\n\n\
         Available protocols:\n",
    );

    for protocol in protocols {
        let _ = writeln!(prompt, "- {}: {}", protocol.name, protocol.description);
    }

    for fragment in fragments {
        prompt.push('\n');
        prompt.push_str(fragment);
        if !fragment.ends_with('\n') {
            prompt.push('\n');
        }
    }

    prompt
}

pub fn task_accepted(id: &str) -> String {
    format!(
        "Background task accepted: tasks://{id}\nCompletion will be delivered automatically. Read that URI only when current status or output is explicitly needed."
    )
}

pub fn truncated_output(preview: &str, complete_file: &Path) -> String {
    format!(
        "{preview}\n\n[output truncated]\nFull output: file://{}",
        display_path(complete_file)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_has_no_working_directory_or_repeated_help_addresses() {
        let prompt = system_prompt(
            &[ProtocolPrompt {
                name: "file".to_string(),
                description: "Read files.".to_string(),
            }],
            &[],
        );
        assert!(prompt.starts_with(
            "You are a general-purpose agent running in URI Agent with exactly two tools: read and exec."
        ));
        assert!(prompt.contains("Every tool call requires a body envelope"));
        assert!(prompt.contains(r#"{"kind":"none","value":""}"#));
        assert!(prompt.contains("same placeholder convention applies throughout protocol help"));
        assert!(prompt.contains(r#"call read("<protocol>://help", {"kind":"none","value":""})"#));
        assert!(prompt.contains("- file: Read files."));
        assert!(!prompt.contains("file://help"));
    }

    #[test]
    fn system_prompt_appends_plugin_fragments_after_protocols() {
        let prompt = system_prompt(
            &[ProtocolPrompt {
                name: "file".to_string(),
                description: "Read files.".to_string(),
            }],
            &["<project_rule_md>rules</project_rule_md>".to_string()],
        );

        assert!(prompt.ends_with("\n<project_rule_md>rules</project_rule_md>\n"));
        assert!(
            prompt.find("- file: Read files.").unwrap() < prompt.find("<project_rule_md>").unwrap()
        );
    }

    #[test]
    fn task_acceptance_returns_the_unified_uri_without_inviting_polling() {
        assert_eq!(
            task_accepted("001"),
            "Background task accepted: tasks://001\nCompletion will be delivered automatically. Read that URI only when current status or output is explicitly needed."
        );
    }
}
