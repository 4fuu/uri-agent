use crate::config::{display_path, path_is_within};
use crate::protocol::{
    DynamicProtocolSource, Protocol, ProtocolContext, ProtocolDescriptor, ProtocolRequest,
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

fn help(skill_md: &str, skill_directory: &Path, protocol: &str) -> String {
    format!(
        "{skill_md}\n\nSkill files: file://{}/\nBundled resource route: {protocol}://<relative-path>\n`<relative-path>` is relative to this Skill directory.\n",
        display_path(skill_directory)
    )
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillSnapshot {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SkillProtocol {
    protocol: String,
    snapshot: SkillSnapshot,
}

#[derive(Clone, Default)]
pub struct SkillProtocolSource {
    protocols: Arc<RwLock<BTreeMap<String, Arc<dyn Protocol>>>>,
}

impl SkillProtocolSource {
    pub fn replace(&self, skills: Vec<SkillProtocol>) {
        *self
            .protocols
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = skills
            .into_iter()
            .map(|skill| {
                (
                    skill.protocol_name().to_string(),
                    Arc::new(skill) as Arc<dyn Protocol>,
                )
            })
            .collect();
    }
}

#[async_trait]
impl DynamicProtocolSource for SkillProtocolSource {
    fn descriptors(&self) -> Vec<ProtocolDescriptor> {
        self.protocols
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|protocol| protocol.descriptor())
            .collect()
    }

    fn protocol(&self, name: &str) -> Option<Arc<dyn Protocol>> {
        self.protocols
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
    }
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
        let path = skill_md_path.canonicalize()?;
        Ok(Self {
            protocol: skill_protocol_name(&name)?,
            snapshot: SkillSnapshot {
                name,
                description,
                path,
            },
        })
    }

    pub fn from_snapshot(snapshot: SkillSnapshot) -> Result<Self> {
        let protocol = skill_protocol_name(&snapshot.name)?;
        if snapshot.description.trim().is_empty() {
            bail!("skill description cannot be empty");
        }
        if !snapshot.path.is_absolute() {
            bail!("skill path must be absolute: {}", snapshot.path.display());
        }
        Ok(Self { protocol, snapshot })
    }

    pub fn snapshot(&self) -> SkillSnapshot {
        self.snapshot.clone()
    }

    pub fn protocol_name(&self) -> &str {
        &self.protocol
    }
}

#[async_trait]
impl Protocol for SkillProtocol {
    fn descriptor(&self) -> ProtocolDescriptor {
        ProtocolDescriptor {
            name: self.protocol.clone(),
            description: format!(
                "Skill “{}”: {}",
                self.snapshot.name, self.snapshot.description
            ),
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
            bail!("skill reads do not accept a body");
        }
        let root = self
            .snapshot
            .path
            .parent()
            .ok_or_else(|| anyhow!("saved SKILL.md path has no parent directory"))?;
        if request.target == "help" {
            let skill_md = fs::read_to_string(&self.snapshot.path).with_context(|| {
                format!(
                    "skill {} is no longer available at {}",
                    self.snapshot.name,
                    self.snapshot.path.display()
                )
            })?;
            return Ok(help(&skill_md, root, &self.protocol).into_bytes());
        }
        let relative = Path::new(request.target);
        if relative.is_absolute() {
            bail!("skill resource paths must be relative");
        }
        let candidate = root.join(relative).canonicalize().with_context(|| {
            format!(
                "skill resource is no longer available at {}",
                root.join(relative).display()
            )
        })?;
        if !path_is_within(&candidate, root) {
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
            Ok(skill) if protocols.insert(skill.protocol_name().to_string()) => skills.push(skill),
            Ok(skill) => warnings.push(format!(
                "skipped duplicate skill protocol {}:// from {}",
                skill.protocol_name(),
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
        assert_eq!(
            skills[0].snapshot(),
            SkillSnapshot {
                name: "Review".to_string(),
                description: "Review code.".to_string(),
                path: review.join("SKILL.md").canonicalize().unwrap(),
            }
        );
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
                    body: "",
                },
                context.clone(),
            )
            .await
            .unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("Skill files: file://"));
        assert!(help.contains("code-review-skill://<relative-path>"));
        assert!(help.contains("relative to this Skill directory"));
        let resource = skill
            .read(
                ProtocolRequest {
                    uri: "code-review-skill://check.sh",
                    target: "check.sh",
                    body: "",
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
                    body: "",
                },
                context,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no longer available")
                || error.to_string().contains("escapes")
        );

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
                        body: "",
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

    #[tokio::test]
    async fn saved_metadata_does_not_cache_or_rebind_skill_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("SKILL.md");
        fs::write(
            &path,
            "---\nname: Review\ndescription: First description.\n---\nfirst body\n",
        )
        .unwrap();
        let discovered = SkillProtocol::load(&path).unwrap();
        let saved = discovered.snapshot();
        fs::write(
            &path,
            "---\nname: Review\ndescription: Changed description.\n---\nchanged body\n",
        )
        .unwrap();
        let restored = SkillProtocol::from_snapshot(saved).unwrap();

        assert!(
            restored
                .descriptor()
                .description
                .contains("First description")
        );
        let help = restored
            .read(
                ProtocolRequest {
                    uri: "review-skill://help",
                    target: "help",
                    body: "",
                },
                ProtocolContext {
                    tasks: crate::task::TaskManager::new(),
                },
            )
            .await
            .unwrap();
        assert!(String::from_utf8(help).unwrap().contains("changed body"));

        fs::remove_file(path).unwrap();
        let replacement_directory = directory.path().join("replacement");
        fs::create_dir(&replacement_directory).unwrap();
        fs::write(
            replacement_directory.join("SKILL.md"),
            "---\nname: Review\ndescription: Replacement.\n---\nreplacement body\n",
        )
        .unwrap();
        assert_eq!(
            SkillProtocol::load(&replacement_directory.join("SKILL.md"))
                .unwrap()
                .protocol_name(),
            restored.protocol_name()
        );
        let error = restored
            .read(
                ProtocolRequest {
                    uri: "review-skill://help",
                    target: "help",
                    body: "",
                },
                ProtocolContext {
                    tasks: crate::task::TaskManager::new(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no longer available"));
    }
}
