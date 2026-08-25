use crate::config::display_path;
use std::fmt::Write as _;
use std::path::Path;

pub const READ_TOOL_DESCRIPTION: &str = "Read through a registered protocol. Use this for protocol help, resources, task status, and completed results.";

pub const EXEC_TOOL_DESCRIPTION: &str = "Execute through a registered protocol. Exact behavior is protocol-specific; read `<protocol>://help` before use. Operations normally return their final result directly. Long-running operations may become managed background tasks whose completion is delivered automatically; use the tasks protocol to inspect or cancel them.";

#[derive(Clone, Debug)]
pub struct PromptEntry {
    pub name: String,
    pub description: String,
}

pub fn system_prompt(
    tools: &[PromptEntry],
    protocols: &[PromptEntry],
    fragments: &[String],
) -> String {
    let mut prompt = String::from(
        "You are a general-purpose agent running in URI Agent.\n\n\
         Available direct tools:\n",
    );

    write_entries(&mut prompt, tools);
    prompt.push_str("\nAvailable protocols:\n");
    write_entries(&mut prompt, protocols);
    prompt.push_str(
        "\nUse a direct tool when its typed arguments match the operation. Use read or exec for capabilities exposed as protocols.\n\n\
         The read and exec body is always a string. Pass \"\" when a protocol takes no body, pass plain text for textual input, and pass complete serialized JSON text when a protocol requires structured input.\n\n\
         Protocol addresses use the custom form <protocol>://<opaque-target>. Angle-bracketed values are placeholders: replace them with actual values without including the angle brackets.\n\n\
         Before using any protocol in a session, you MUST first call read(\"<protocol>://help\", \"\") for that protocol and follow its contract. The help read is the only permitted first call to a protocol.\n\n\
         Verify relevant results before claiming work is complete.\n",
    );

    for fragment in fragments {
        prompt.push('\n');
        prompt.push_str(fragment);
        if !fragment.ends_with('\n') {
            prompt.push('\n');
        }
    }

    prompt
}

fn write_entries(prompt: &mut String, entries: &[PromptEntry]) {
    for entry in entries {
        let description = entry
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(prompt, "- {}: {description}", entry.name);
    }
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
    fn system_prompt_separates_direct_tools_from_protocols() {
        let prompt = system_prompt(
            &[PromptEntry {
                name: "read".to_string(),
                description: "Read through a\nregistered protocol.".to_string(),
            }],
            &[PromptEntry {
                name: "file".to_string(),
                description: "Read files.".to_string(),
            }],
            &[],
        );
        assert!(prompt.starts_with("You are a general-purpose agent running in URI Agent."));
        assert!(prompt.contains("body is always a string"));
        assert!(
            prompt.contains("Available direct tools:\n- read: Read through a registered protocol.")
        );
        assert!(prompt.contains("- file: Read files."));
        assert!(
            prompt.find("Available direct tools:").unwrap()
                < prompt.find("Available protocols:").unwrap()
        );
        assert!(
            prompt
                .contains(r#"you MUST first call read("<protocol>://help", "") for that protocol"#)
        );
        assert!(prompt.contains("The help read is the only permitted first call to a protocol."));
        assert!(!prompt.contains("file://help"));
    }

    #[test]
    fn system_prompt_appends_plugin_fragments_after_protocols() {
        let prompt = system_prompt(
            &[PromptEntry {
                name: "read".to_string(),
                description: "Read resources.".to_string(),
            }],
            &[PromptEntry {
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
