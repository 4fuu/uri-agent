use crate::prompts;
use crate::protocol::{Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct SkillProtocol {
    protocol: String,
    name: String,
    description: String,
    root: PathBuf,
    skill_md: String,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

impl SkillProtocol {
    fn load(skill_md_path: &Path) -> Result<Self> {
        let skill_md = fs::read_to_string(skill_md_path)
            .with_context(|| format!("cannot read {}", skill_md_path.display()))?;
        let frontmatter = parse_frontmatter(&skill_md)
            .with_context(|| format!("invalid metadata in {}", skill_md_path.display()))?;
        let name = frontmatter.name.trim().to_string();
        let description = frontmatter.description.trim().to_string();
        if name.is_empty() || description.is_empty() {
            bail!("skill name and description cannot be empty");
        }
        let root = skill_md_path
            .parent()
            .ok_or_else(|| anyhow!("SKILL.md has no parent directory"))?
            .canonicalize()?;
        Ok(Self {
            protocol: skill_protocol_name(&name)?,
            name,
            description,
            root,
            skill_md,
        })
    }

    pub fn protocol_name(&self) -> &str {
        &self.protocol
    }

    pub fn display_name(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl Protocol for SkillProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: self.protocol.clone(),
            description: format!("Skill “{}”: {}", self.name, self.description),
            can_read: true,
            can_exec: false,
        }
    }

    async fn read(
        &self,
        request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        if request.target == "help" {
            return Ok(prompts::skill_help(&self.skill_md, &self.root).into_bytes());
        }
        let relative = Path::new(request.target);
        if relative.is_absolute() {
            bail!("skill resource paths must be relative");
        }
        let candidate = self
            .root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("skill resource not found: {}", request.target))?;
        if !candidate.starts_with(&self.root) {
            bail!("skill resource escapes its skill directory");
        }
        let metadata = fs::metadata(&candidate)?;
        if !metadata.is_file() {
            bail!("skill resource is not a file: {}", request.target);
        }
        fs::read(&candidate).with_context(|| format!("cannot read {}", candidate.display()))
    }
}

pub fn discover(cwd: &Path) -> (Vec<SkillProtocol>, Vec<String>) {
    let mut roots = vec![
        cwd.join(".agents/skills"),
        cwd.join(".claude/skills"),
        cwd.join(".codex/skills"),
    ];
    if let Some(home) = dirs::home_dir() {
        roots.extend([
            home.join(".agents/skills"),
            home.join(".claude/skills"),
            home.join(".codex/skills"),
        ]);
    }
    discover_in(roots)
}

fn discover_in(roots: Vec<PathBuf>) -> (Vec<SkillProtocol>, Vec<String>) {
    let mut skill_files = Vec::new();
    let mut seen_paths = HashSet::new();
    for root in roots.into_iter().filter(|root| root.is_dir()) {
        let mut files = skill_files_in(&root);
        files.sort();
        for path in files {
            let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen_paths.insert(identity) {
                skill_files.push(path);
            }
        }
    }

    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    let mut protocols = HashSet::new();
    for path in skill_files {
        match SkillProtocol::load(&path) {
            Ok(skill) if protocols.insert(skill.protocol.clone()) => skills.push(skill),
            Ok(skill) => warnings.push(format!(
                "skipped duplicate skill protocol {}:// from {}",
                skill.protocol,
                path.display()
            )),
            Err(error) => warnings.push(format!("skipped skill {}: {error:#}", path.display())),
        }
    }
    (skills, warnings)
}

fn skill_files_in(root: &Path) -> Vec<PathBuf> {
    let mut output = Vec::new();
    let direct = root.join("SKILL.md");
    if direct.is_file() {
        output.push(direct);
    }
    let Ok(entries) = fs::read_dir(root) else {
        return output;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let skill_md = entry.path().join("SKILL.md");
            if skill_md.is_file() {
                output.push(skill_md);
            }
        }
    }
    output
}

fn parse_frontmatter(content: &str) -> Result<Frontmatter> {
    let content = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("SKILL.md must start with YAML frontmatter"))?;
    let end = content
        .find("\n---\n")
        .or_else(|| content.find("\r\n---\r\n"))
        .ok_or_else(|| anyhow!("SKILL.md frontmatter is not closed"))?;
    serde_yaml::from_str(&content[..end]).context("cannot parse YAML frontmatter")
}

fn skill_protocol_name(name: &str) -> Result<String> {
    let mut protocol = String::new();
    let mut separated = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            protocol.push(character.to_ascii_lowercase());
            separated = false;
        } else if !separated && !protocol.is_empty() {
            protocol.push('-');
            separated = true;
        }
    }
    while protocol.ends_with('-') {
        protocol.pop();
    }
    if protocol.is_empty() {
        bail!("skill name must contain an ASCII letter or number");
    }
    if !protocol.ends_with("-skill") {
        protocol.push_str("-skill");
    }
    Ok(protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_skill_gets_one_stable_protocol() {
        assert_eq!(
            skill_protocol_name("Code Review").unwrap(),
            "code-review-skill"
        );
        assert_eq!(
            skill_protocol_name("code-review-skill").unwrap(),
            "code-review-skill"
        );
    }

    #[test]
    fn discovery_generates_protocols_from_one_directory_scan() {
        let root = tempfile::tempdir().unwrap();
        let review = root.path().join("review");
        let nested = root.path().join("category/nested");
        fs::create_dir_all(&review).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            review.join("SKILL.md"),
            "---\nname: Review\ndescription: Review code.\n---\n",
        )
        .unwrap();
        fs::write(
            nested.join("SKILL.md"),
            "---\nname: Nested\ndescription: Must not leak in recursively.\n---\n",
        )
        .unwrap();

        let (skills, warnings) = discover_in(vec![root.path().to_path_buf()]);
        assert!(warnings.is_empty());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].protocol_name(), "review-skill");
    }

    #[tokio::test]
    async fn help_includes_files_and_resources_cannot_escape() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("SKILL.md"),
            "---\nname: Code Review\ndescription: Review code.\n---\n# Review\n",
        )
        .unwrap();
        fs::write(directory.path().join("check.sh"), "echo ok").unwrap();
        let skill = SkillProtocol::load(&directory.path().join("SKILL.md")).unwrap();
        let context = ProtocolContext {
            tasks: crate::task::TaskManager::new(),
        };
        let help = skill
            .read(
                ProtocolRequest {
                    uri: "code-review-skill://help",
                    target: "help",
                    body: None,
                },
                context.clone(),
            )
            .await
            .unwrap();
        assert!(
            String::from_utf8(help)
                .unwrap()
                .contains("Skill files: file://")
        );
        let resource = skill
            .read(
                ProtocolRequest {
                    uri: "code-review-skill://check.sh",
                    target: "check.sh",
                    body: None,
                },
                context.clone(),
            )
            .await
            .unwrap();
        assert_eq!(resource, b"echo ok");

        let error = skill
            .read(
                ProtocolRequest {
                    uri: "code-review-skill://../outside",
                    target: "../outside",
                    body: None,
                },
                context,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not found") || error.to_string().contains("escapes"));

        #[cfg(unix)]
        {
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::os::unix::fs::symlink(outside.path(), directory.path().join("outside-link"))
                .unwrap();
            let error = skill
                .read(
                    ProtocolRequest {
                        uri: "code-review-skill://outside-link",
                        target: "outside-link",
                        body: None,
                    },
                    ProtocolContext {
                        tasks: crate::task::TaskManager::new(),
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("escapes"));
        }
    }
}
