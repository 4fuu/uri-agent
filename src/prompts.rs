use crate::config::display_path;
use std::fmt::Write as _;
use std::path::Path;

pub const READ_TOOL_DESCRIPTION: &str = "Read through a registered protocol. Use this for protocol help, resources, task status, and completed results.";

pub const EXEC_TOOL_DESCRIPTION: &str = "Execute through a registered protocol. Use this to start protocol operations. Execution may finish immediately or continue asynchronously. If the returned content includes a task URI, use read on that URI to inspect status and final output; task acceptance alone is not completion.";

#[derive(Clone, Debug)]
pub struct ProtocolPrompt {
    pub name: String,
    pub description: String,
}

pub fn system_prompt(protocols: &[ProtocolPrompt], fragments: &[String]) -> String {
    let mut prompt = String::from(
        "You are a general-purpose agent.\n\
         You are running in URI Agent.\n\
         You have exactly two tools: read and exec.\n\
         Use read to retrieve information through registered protocols.\n\
         Use exec to perform actions through registered protocols.\n\
         Before using a protocol for the first time, you must call read on \
         <protocol>://help to learn its contract.\n\
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

pub fn task_accepted(protocol: &str, id: &str) -> String {
    format!("Task accepted: {id}\nRead status: {protocol}://tasks/{id}")
}

pub fn task_snapshot(
    id: &str,
    status: &str,
    started_at: &str,
    finished_at: Option<&str>,
    content: &str,
) -> String {
    let mut output = format!("Task: {id}\nStatus: {status}\nStarted: {started_at}\n");
    if let Some(finished_at) = finished_at {
        let _ = writeln!(output, "Finished: {finished_at}");
    }
    if !content.is_empty() {
        output.push('\n');
        output.push_str(content);
    }
    output
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
            "You are a general-purpose agent.\n\
             You are running in URI Agent.\n\
             You have exactly two tools: read and exec."
        ));
        assert!(prompt.contains("Before using a protocol for the first time"));
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
}
