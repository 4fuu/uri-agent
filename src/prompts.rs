use crate::config::display_path;
use std::fmt::Write as _;
use std::path::Path;

pub const HELP_TOOL_DESCRIPTION: &str = "Load the usage contract of one or more protocols. Call this once before the first read or exec call to any protocol.";

pub const READ_TOOL_DESCRIPTION: &str = "Read through a registered protocol. Use this for resources, task status, and completed results.";

pub const EXEC_TOOL_DESCRIPTION: &str = "Execute through a registered protocol. Operations normally return their final result directly. Long-running operations may become managed background tasks whose completion is delivered automatically.";

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

    let mut tools = tools.to_vec();
    tools.sort_by(|left, right| {
        tool_order(&left.name)
            .cmp(&tool_order(&right.name))
            .then_with(|| left.name.cmp(&right.name))
    });
    write_entries(&mut prompt, &tools);
    prompt.push_str("\nAvailable protocols:\n");
    write_entries(&mut prompt, protocols);
    prompt.push_str(
        "\nChoose a direct tool or a protocol as appropriate for the operation.\n\n\
         Protocol rules:\n\
         - Load help first. Before the first read or exec call to any protocol, you MUST call help with that protocol's name; batch several protocols in one call.\n\
         - Follow the loaded help pages exactly. Only they define a protocol's valid addresses, parameters, and body formats; never guess them.\n\
         - Protocol addresses use the custom form <protocol>://<opaque-target>. Angle-bracketed values are placeholders: replace them with actual values.\n",
    );
    prompt.push_str(
        "\nOperating rules:\n\
         - For clear requests, inspect relevant sources, carry the work through, and verify the result.\n\
         - Treat user reports and proposed causes as claims to check, not established facts.\n\
         - Make the smallest complete change. Preserve unrelated existing work and remove temporary artifacts you create.\n\
         - If an action fails, diagnose the failure before retrying it or changing approach.\n\
         - Ask before modifying shared or external state or taking irreversible actions unless the user explicitly requested that specific action. Routine local reads, requested workspace edits, builds, and tests need no separate approval.\n\
         - Report verification honestly. Never claim a check passed unless it ran successfully; state why relevant verification could not be completed.\n",
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

/// Presentation order for the core direct tools in the system prompt; any
/// other tool sorts after them alphabetically.
fn tool_order(name: &str) -> usize {
    match name {
        "help" => 0,
        "read" => 1,
        "exec" => 2,
        "replace" => 3,
        "apply_patch" => 4,
        _ => 5,
    }
}

pub fn task_accepted(id: &str) -> String {
    format!(
        "Background task started: tasks://{id}\nCompletion will be delivered automatically. Continue any independent work. If progress depends on this result, load the tasks protocol with help([\"tasks\"]) if needed, then use one bounded wait. Do not poll or rerun the operation."
    )
}

pub fn interactive_task_accepted(id: &str) -> String {
    format!(
        "Interactive task started: tasks://{id}\nCompletion will be delivered automatically. Load the tasks protocol with help([\"tasks\"]) if needed, then send input with exec(\"tasks://{id}/send\", \"<input>\"); the input is written exactly, so end each line with \\n. Close stdin with exec(\"tasks://{id}/eof\", \"\") and interrupt with exec(\"tasks://{id}/interrupt\", \"\"). Read current output with read(\"tasks://{id}\", \"\") and use one bounded wait when the result is needed. Do not poll or rerun the operation."
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
        assert!(
            prompt.contains("Available direct tools:\n- read: Read through a registered protocol.")
        );
        assert!(prompt.contains("- file: Read files."));
        assert!(
            prompt.find("Available direct tools:").unwrap()
                < prompt.find("Available protocols:").unwrap()
        );
        assert!(prompt.contains("you MUST call help with that protocol's name"));
        assert!(prompt.contains("never guess them."));
        assert!(prompt.contains("Choose a direct tool or a protocol as appropriate"));
        assert!(prompt.contains("Operating rules:\n- For clear requests"));
        assert!(prompt.contains("Treat user reports and proposed causes as claims to check"));
        assert!(prompt.contains("Make the smallest complete change"));
        assert!(prompt.contains("diagnose the failure before retrying"));
        assert!(prompt.contains("Ask before modifying shared or external state"));
        assert!(prompt.contains("Never claim a check passed unless it ran successfully"));
        assert!(!prompt.contains("search://"));
    }

    #[test]
    fn system_prompt_lists_core_tools_in_preferred_order() {
        let prompt = system_prompt(
            &[
                PromptEntry {
                    name: "replace".to_string(),
                    description: "Replace.".to_string(),
                },
                PromptEntry {
                    name: "exec".to_string(),
                    description: "Execute.".to_string(),
                },
                PromptEntry {
                    name: "apply_patch".to_string(),
                    description: "Patch.".to_string(),
                },
                PromptEntry {
                    name: "help".to_string(),
                    description: "Load contracts.".to_string(),
                },
                PromptEntry {
                    name: "read".to_string(),
                    description: "Read.".to_string(),
                },
                PromptEntry {
                    name: "a-wasm-tool".to_string(),
                    description: "Dynamic.".to_string(),
                },
            ],
            &[],
            &[],
        );

        let positions = [
            "- help:",
            "- read:",
            "- exec:",
            "- replace:",
            "- apply_patch:",
        ]
        .map(|marker| prompt.find(marker).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(prompt.find("- apply_patch:").unwrap() < prompt.find("- a-wasm-tool:").unwrap());
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
    fn task_acceptance_points_to_bounded_wait_without_inviting_polling() {
        assert_eq!(
            task_accepted("001"),
            "Background task started: tasks://001\nCompletion will be delivered automatically. Continue any independent work. If progress depends on this result, load the tasks protocol with help([\"tasks\"]) if needed, then use one bounded wait. Do not poll or rerun the operation."
        );
    }

    #[test]
    fn interactive_task_acceptance_points_to_input_routes() {
        let message = interactive_task_accepted("002");
        assert!(message.starts_with("Interactive task started: tasks://002"));
        assert!(message.contains(r#"help(["tasks"])"#));
        assert!(message.contains(r#"exec("tasks://002/send", "<input>")"#));
        assert!(message.contains("end each line with \\n"));
        assert!(message.contains(r#"exec("tasks://002/eof", "")"#));
        assert!(message.contains(r#"exec("tasks://002/interrupt", "")"#));
        assert!(message.contains(r#"read("tasks://002", "")"#));
        assert!(message.contains("Do not poll or rerun the operation"));
    }
}
