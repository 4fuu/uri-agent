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

pub fn system_prompt(protocols: &[ProtocolPrompt]) -> String {
    let mut prompt = String::from(
        "You are a general-purpose agent.\n\
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

    prompt
}

pub fn file_help(cwd: &Path) -> String {
    format!(
        r#"# file

Read files and directories.

Current working directory: `file://{}`

- `file://relative/path` resolves from the current working directory.
- `file:///absolute/path` reads an absolute path.
- Add `?offset=N&limit=N` to read a bounded range of text lines.
- Add `?line_numbers=true` to prefix file content with one-based line numbers. Line numbers are disabled by default.
- Reading a directory returns a bounded directory listing.
- Full outputs saved by the system are exposed as `file://` addresses.

The body is passed through but is not required by this built-in protocol.
"#,
        display_path(cwd)
    )
}

pub const REPLACE_HELP: &str = r#"# replace

Replace one exact text match asynchronously.

Call `exec` with `replace://path` and this body:

```json
{"old_text":"unique text to replace","new_text":"replacement"}
```

Relative paths resolve from the startup working directory; absolute paths are
accepted. `old_text` must be nonempty and occur exactly once. The file is
replaced atomically. The immediate result contains a task URI; read that URI to
inspect completion or failure.
"#;

pub const APPLY_PATCH_HELP: &str = r#"# apply_patch

Apply a Codex-style multi-file patch asynchronously.

Call `exec` with `apply_patch://apply`. The body must be the patch string itself:

```text
*** Begin Patch
*** Add File: path/to/new.txt
+new content
*** Update File: path/to/existing.txt
@@ optional landmark
-old line
+new line
*** Delete File: path/to/remove.txt
*** End Patch
```

An Update File may put `*** Move to: new/path` immediately after its header.
Update lines begin with a space for context, `-` for removal, or `+` for
addition. `*** End of File` anchors the preceding chunk at EOF. Add File content
lines must all begin with `+`. Relative paths resolve from the startup working
directory; absolute paths are accepted.

Operations run in patch order and each write is atomic, but the complete patch
is not transactional: a later failure does not undo earlier operations. The
immediate result contains a task URI; read that URI for the final summary or
error.
"#;

pub const BASH_HELP: &str = r#"# bash

Run Bash commands as managed asynchronous tasks.

Call `exec` with `bash://run` and pass the command string directly as the body:

```text
exec("bash://run", "cargo test")
```

Add `?wait=N` to wait up to N seconds (maximum 300), for example
`bash://?wait=30`. If the wait window expires, the command keeps running and
the result contains its task URI.

Read `bash://tasks/<id>` for status and bounded output. If that output exceeds
the system limit, the result includes a `file://` address containing the full
output.
"#;

pub const PWSH_HELP: &str = r#"# pwsh

Run PowerShell 7 commands as managed asynchronous tasks.

Call `exec` with `pwsh://run` and pass the command string directly as the body:

```text
exec("pwsh://run", "cargo test")
```

Add `?wait=N` to wait up to N seconds (maximum 300), for example
`pwsh://?wait=30`. If the wait window expires, the command keeps running and
the result contains its task URI.

Read `pwsh://tasks/<id>` for status and bounded output. If that output exceeds
the system limit, the result includes a `file://` address containing the full
output.
"#;

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

pub fn skill_help(skill_md: &str, skill_directory: &Path) -> String {
    format!(
        "{skill_md}\n\nSkill files: file://{}/\n",
        display_path(skill_directory)
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
    fn file_help_reports_display_path_and_opt_in_line_numbers() {
        let help = file_help(Path::new(r"\\?\C:\Users\4fu\project"));
        assert!(help.contains(r"Current working directory: `file://C:\Users\4fu\project`"));
        assert!(help.contains("`?line_numbers=true`"));
        assert!(help.contains("Line numbers are disabled by default."));
    }

    #[test]
    fn system_prompt_has_no_working_directory_or_repeated_help_addresses() {
        let prompt = system_prompt(&[ProtocolPrompt {
            name: "file".to_string(),
            description: "Read files.".to_string(),
        }]);
        assert!(prompt.starts_with("You are a general-purpose agent."));
        assert!(prompt.contains("Before using a protocol for the first time"));
        assert!(prompt.contains("- file: Read files."));
        assert!(!prompt.contains("file://help"));
    }
}
