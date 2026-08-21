use std::fmt::Write as _;
use std::path::Path;

pub const READ_TOOL_DESCRIPTION: &str = "Read a resource from a registered protocol. The uri and optional body are passed to the protocol unchanged.";

pub const EXEC_TOOL_DESCRIPTION: &str = "Execute through a registered protocol. Work is asynchronous by default; protocols may expose an explicit bounded wait option. The uri and optional body are passed to the protocol unchanged. Use read with a returned task URI to inspect later progress or results.";

#[derive(Clone, Debug)]
pub struct ProtocolPrompt {
    pub name: String,
    pub description: String,
}

pub fn system_prompt(cwd: &Path, protocols: &[ProtocolPrompt]) -> String {
    let mut prompt = format!(
        "You are a coding agent working in {}.\n\
         You have exactly two tools: read and exec.\n\
         Protocol addresses use the custom form <protocol>://<opaque-target>; \
         they are not RFC URLs. The protocol receives the URI and body unchanged.\n\
         Use read for resources, help, task snapshots, and completed output.\n\
         Exec work is asynchronous by default. An accepted result does not mean \
         the work finished; inspect the returned URI with read. Some protocols \
         document a bounded wait option for work whose immediate result is useful.\n\
         Before using an unfamiliar protocol, read <protocol>://help.\n\
         When output is truncated, read the supplied file:// address for the complete content.\n\
         Verify relevant results before claiming work is complete.\n\n\
         Available protocols:\n",
        cwd.display()
    );

    for protocol in protocols {
        let _ = writeln!(
            prompt,
            "- {}: {} Help: {}://help",
            protocol.name, protocol.description, protocol.name
        );
    }

    prompt
}

pub const FILE_HELP: &str = r#"# file

Read files and directories.

- `file://relative/path` resolves from the startup working directory.
- `file:///absolute/path` reads an absolute path.
- Add `?offset=N&limit=N` to read a bounded range of text lines.
- Reading a directory returns a bounded directory listing.
- Full outputs saved by the system are exposed as `file://` addresses.

The body is passed through but is not required by this built-in protocol.
"#;

pub const EDIT_HELP: &str = r#"# edit

Submit an asynchronous file edit.

Call `exec` with `edit://path` and one of these bodies:

```json
{"old_text":"unique text to replace","new_text":"replacement"}
```

```json
{"content":"complete replacement file content"}
```

The first form requires exactly one match. The second form atomically creates or
replaces the file. Add `?wait=N` to the target path to wait up to N seconds
(maximum 300), for example `edit://src/main.rs?wait=10`. Otherwise the immediate
result contains a task URI. Read that URI to inspect completion or failure.
"#;

pub const BASH_HELP: &str = r#"# bash

Run Bash commands as managed asynchronous tasks.

Call `exec` with `bash://run`. The body may be a command string or an object
containing a `command` string. Add `?wait=N` to wait up to N seconds (maximum
300), for example `bash://?wait=30`. If the wait window expires, the command
keeps running and the result contains its task URI.

Read `bash://tasks/<id>` for status and bounded output. If that output exceeds
the system limit, the result includes a `file://` address containing the full
output.
"#;

pub const PWSH_HELP: &str = r#"# pwsh

Run PowerShell 7 commands as managed asynchronous tasks.

Call `exec` with `pwsh://run`. The body may be a command string or an object
containing a `command` string. Add `?wait=N` to wait up to N seconds (maximum
300), for example `pwsh://?wait=30`. If the wait window expires, the command
keeps running and the result contains its task URI.

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
        skill_directory.display()
    )
}

pub fn truncated_output(preview: &str, complete_file: &Path) -> String {
    format!(
        "{preview}\n\n[output truncated]\nFull output: file://{}",
        complete_file.display()
    )
}
