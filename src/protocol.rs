use crate::output::OutputStore;
use crate::prompts::PromptEntry;
use crate::task::TaskManager;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use rig::message::{ImageMediaType, ToolResultContent};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug)]
pub(crate) struct ProtocolHelpRequired {
    required: String,
    requested: String,
}

impl ProtocolHelpRequired {
    fn new(protocol: &str) -> Self {
        Self {
            required: protocol.to_string(),
            requested: protocol.to_string(),
        }
    }

    fn dependency(required: &str, requested: &str) -> Self {
        Self {
            required: required.to_string(),
            requested: requested.to_string(),
        }
    }
}

impl fmt::Display for ProtocolHelpRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.required == self.requested {
            write!(
                formatter,
                "Load this protocol first: call help([{:?}]) before using {}://.",
                self.required, self.required
            )
        } else {
            write!(
                formatter,
                "Load the shared prerequisite first: call help([{:?}]) before using {}://.",
                self.required, self.requested
            )
        }
    }
}

impl std::error::Error for ProtocolHelpRequired {}

#[derive(Clone)]
pub struct ProtocolContext {
    pub tasks: TaskManager,
    /// Set for operations the `protocol` tool pinned to the foreground
    /// because a later step references their result. Protocols that
    /// auto-promote long foreground work to background tasks must keep such
    /// operations in the foreground under their own deadline instead.
    pub pinned_foreground: bool,
}

impl ProtocolContext {
    pub fn new(tasks: TaskManager) -> Self {
        Self {
            tasks,
            pinned_foreground: false,
        }
    }

    /// The foreground grace before an auto-background promotion. Pinned
    /// operations never promote, so they wait under a practically unbounded
    /// grace instead of the protocol's usual one.
    pub fn foreground_grace(&self, default: Duration) -> Duration {
        if self.pinned_foreground {
            PINNED_FOREGROUND_GRACE
        } else {
            default
        }
    }
}

/// A century of foreground patience for operations a later step references;
/// `Duration::MAX` is avoided because waits saturate `Instant` arithmetic.
const PINNED_FOREGROUND_GRACE: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolDescriptor {
    pub name: String,
    pub description: String,
    pub can_read: bool,
    pub can_exec: bool,
}

pub struct ProtocolRequest<'a> {
    pub uri: &'a str,
    pub target: &'a str,
    pub input: &'a Map<String, Value>,
}

impl ProtocolRequest<'_> {
    /// Deserialize `input` into a typed struct. The struct must derive
    /// `#[serde(deny_unknown_fields)]` so unknown fields are rejected with
    /// the accepted field list, matching the protocol's help page.
    pub fn input_struct<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(Value::Object(self.input.clone()))
            .with_context(|| format!("invalid input for {}", self.uri))
    }

    /// Reject any `input` field on a route that takes none.
    pub fn reject_input(&self) -> Result<()> {
        if let Some(key) = self.input.keys().next() {
            bail!(
                "unknown input field `{key}` in {}; this route takes no input fields",
                self.uri
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolImageMediaType {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl ProtocolImageMediaType {
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\xff\xd8\xff") {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some(Self::Webp)
        } else {
            None
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolImage {
    bytes: Vec<u8>,
    media_type: ProtocolImageMediaType,
}

impl ProtocolImage {
    pub fn new(bytes: Vec<u8>, media_type: ProtocolImageMediaType) -> Self {
        Self { bytes, media_type }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn media_type(&self) -> ProtocolImageMediaType {
        self.media_type
    }

    pub(crate) fn into_tool_result_content(self) -> ToolResultContent {
        let media_type = match self.media_type {
            ProtocolImageMediaType::Jpeg => ImageMediaType::JPEG,
            ProtocolImageMediaType::Png => ImageMediaType::PNG,
            ProtocolImageMediaType::Gif => ImageMediaType::GIF,
            ProtocolImageMediaType::Webp => ImageMediaType::WEBP,
        };
        ToolResultContent::image_base64(
            base64::engine::general_purpose::STANDARD.encode(self.bytes),
            Some(media_type),
            None,
        )
    }
}

/// One completed protocol operation. `text` is the complete text output and
/// carries the truncation and spill logic when presented; `json` is the
/// structured output exposed to step references as `<id>.json`; `images`
/// are typed image results for models that accept them.
#[derive(Clone, Debug)]
pub struct ProtocolOutput {
    text: Vec<u8>,
    json: Option<Value>,
    images: Vec<ProtocolImage>,
}

impl ProtocolOutput {
    pub fn new(text: Vec<u8>, json: Option<Value>, images: Vec<ProtocolImage>) -> Self {
        Self { text, json, images }
    }

    pub fn text(text: impl Into<Vec<u8>>) -> Self {
        Self::new(text.into(), None, Vec::new())
    }

    pub fn text_bytes(&self) -> &[u8] {
        &self.text
    }

    pub fn json(&self) -> Option<&Value> {
        self.json.as_ref()
    }

    pub(crate) fn into_parts(self) -> (Vec<u8>, Option<Value>, Vec<ProtocolImage>) {
        (self.text, self.json, self.images)
    }
}

impl From<Vec<u8>> for ProtocolOutput {
    fn from(text: Vec<u8>) -> Self {
        Self::text(text)
    }
}

impl From<String> for ProtocolOutput {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for ProtocolOutput {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

/// A protocol operation after presentation: `output` is the bounded view the
/// model sees, `text` keeps the complete text for step references, and
/// `json` is the structured output.
#[derive(Debug)]
pub(crate) struct PresentedProtocolOutput {
    pub output: String,
    pub text: String,
    pub json: Option<Value>,
    pub images: Vec<ProtocolImage>,
}

#[async_trait]
pub trait Protocol: Send + Sync {
    fn descriptor(&self) -> ProtocolDescriptor;

    /// Additional protocol help pages that must be loaded before this
    /// protocol's own help or operations. The protocol's own page remains
    /// mandatory and is loaded after these shared prerequisites.
    ///
    /// Reserve this for help pages that genuinely build on another shared
    /// page, such as `<name>-mcp` server help on the shared `mcp://`
    /// routing page. Never force a dependency for a protocol that is merely
    /// referenced by this one's results, such as `tasks://` handles: those
    /// links are chained lazily by the prompts that mention them, which keeps
    /// the interface loaded on demand.
    fn help_dependencies(&self) -> &[String] {
        &[]
    }

    /// Top-level `input` fields whose string values this protocol executes
    /// verbatim, such as a shell `script`. The `protocol` tool never
    /// substitutes `{{ reference }}` placeholders in them; data reaches the
    /// executed text through dedicated input fields instead (for the shell,
    /// `env`). Nested values inside these fields are not substituted either.
    fn literal_input_fields(&self) -> &[&str] {
        &[]
    }

    async fn read(
        &self,
        _request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        bail!("this protocol does not support read")
    }

    async fn read_output(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        self.read(request, context).await
    }

    async fn exec(
        &self,
        _request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<ProtocolOutput> {
        bail!("this protocol does not support exec")
    }
}

#[async_trait]
pub trait DynamicProtocolSource: Send + Sync {
    async fn ready(&self) -> Result<()> {
        Ok(())
    }

    fn descriptors(&self) -> Vec<ProtocolDescriptor>;
    fn protocol(&self, name: &str) -> Option<Arc<dyn Protocol>>;
}

pub struct ProtocolRegistry {
    protocols: BTreeMap<String, Arc<dyn Protocol>>,
    dynamic: Vec<Arc<dyn DynamicProtocolSource>>,
    output: Arc<OutputStore>,
    context: ProtocolContext,
    help_read: AsyncMutex<HashSet<String>>,
    allowed: RwLock<Option<HashSet<String>>>,
}

impl ProtocolRegistry {
    pub fn new(output: Arc<OutputStore>, tasks: TaskManager) -> Self {
        Self {
            protocols: BTreeMap::new(),
            dynamic: Vec::new(),
            output,
            context: ProtocolContext::new(tasks),
            help_read: AsyncMutex::new(HashSet::new()),
            allowed: RwLock::new(None),
        }
    }

    pub fn select(&self, names: Option<&[String]>) -> Result<()> {
        let selected = names
            .map(|names| self.validate_selection(names))
            .transpose()?;
        *self
            .allowed
            .write()
            .expect("protocol selection lock poisoned") = selected;
        Ok(())
    }

    pub(crate) fn validate_selection(&self, names: &[String]) -> Result<HashSet<String>> {
        let available = self
            .all_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();
        let selected = names.iter().cloned().collect::<HashSet<_>>();
        if selected.len() != names.len() {
            bail!("Agent protocol is selected more than once");
        }
        if let Some(name) = selected.iter().find(|name| !available.contains(*name)) {
            bail!("unknown Agent protocol: {name}");
        }
        for name in &selected {
            let Some(protocol) = self.protocols.get(name) else {
                continue;
            };
            validate_help_dependencies(name, protocol.help_dependencies())?;
            for dependency in protocol.help_dependencies() {
                if !available.contains(dependency) {
                    bail!("Agent protocol {name} requires unavailable help protocol {dependency}");
                }
                if !selected.contains(dependency) {
                    bail!("Agent protocol {name} also requires selecting protocol {dependency}");
                }
            }
        }
        Ok(selected)
    }

    pub fn register(&mut self, protocol: impl Protocol + 'static) -> Result<()> {
        let protocol: Arc<dyn Protocol> = Arc::new(protocol);
        let descriptor = protocol.descriptor();
        validate_descriptor(&descriptor)?;
        validate_help_dependencies(&descriptor.name, protocol.help_dependencies())?;
        if self.protocols.contains_key(&descriptor.name) {
            bail!("protocol name is already registered: {}", descriptor.name);
        }
        self.protocols.insert(descriptor.name, protocol);
        Ok(())
    }

    pub fn set_dynamic_source(&mut self, source: Arc<dyn DynamicProtocolSource>) -> Result<()> {
        let mut names = self
            .dynamic
            .iter()
            .flat_map(|source| source.descriptors())
            .map(|descriptor| descriptor.name)
            .collect::<HashSet<_>>();
        for descriptor in source.descriptors() {
            validate_descriptor(&descriptor)?;
            if self.protocols.contains_key(&descriptor.name)
                || !names.insert(descriptor.name.clone())
            {
                bail!(
                    "dynamic protocol name is already registered: {}",
                    descriptor.name
                );
            }
        }
        self.dynamic.push(source);
        Ok(())
    }

    fn all_descriptors(&self) -> Vec<ProtocolDescriptor> {
        let mut descriptors = self
            .protocols
            .values()
            .map(|protocol| protocol.descriptor())
            .collect::<Vec<_>>();
        for dynamic in &self.dynamic {
            descriptors.extend(dynamic.descriptors());
        }
        descriptors
    }

    pub fn descriptors(&self) -> Vec<ProtocolDescriptor> {
        let mut descriptors = self.all_descriptors();
        if let Some(allowed) = self
            .allowed
            .read()
            .expect("protocol selection lock poisoned")
            .as_ref()
        {
            descriptors.retain(|descriptor| allowed.contains(&descriptor.name));
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        descriptors
    }

    pub(crate) fn prompt_protocols_for(
        &self,
        names: Option<&[String]>,
    ) -> Result<Vec<PromptEntry>> {
        let selected = names
            .map(|names| self.validate_selection(names))
            .transpose()?;
        let mut descriptors = self.all_descriptors();
        if let Some(selected) = selected {
            descriptors.retain(|descriptor| selected.contains(&descriptor.name));
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(descriptors
            .into_iter()
            .map(|descriptor| PromptEntry {
                name: descriptor.name,
                description: descriptor.description,
            })
            .collect())
    }

    pub(crate) fn tasks(&self) -> TaskManager {
        self.context.tasks.clone()
    }

    pub(crate) fn output_store(&self) -> Arc<OutputStore> {
        self.output.clone()
    }

    pub(crate) async fn record_diagnostic(&self, event: &str, fields: serde_json::Value) {
        let _ = self.output.record_diagnostic(event, fields).await;
    }

    pub(crate) async fn present(&self, content: Vec<u8>, hint: &str) -> Result<String> {
        self.output.present(content, hint).await
    }

    pub async fn read(&self, uri: &str, input: &Map<String, Value>) -> Result<String> {
        Ok(self
            .dispatch_read(uri, input, true, true, false)
            .await?
            .output)
    }

    pub(crate) async fn read_for_model(
        &self,
        uri: &str,
        input: &Map<String, Value>,
        pinned_foreground: bool,
    ) -> Result<PresentedProtocolOutput> {
        self.dispatch_read(uri, input, true, true, pinned_foreground)
            .await
    }

    pub(crate) async fn exec_for_model(
        &self,
        uri: &str,
        input: &Map<String, Value>,
        pinned_foreground: bool,
    ) -> Result<PresentedProtocolOutput> {
        self.dispatch_exec_output(uri, input, true, true, pinned_foreground)
            .await
    }

    /// Input fields the named protocol executes verbatim; the `protocol`
    /// tool leaves `{{ reference }}` placeholders in them untouched. Unknown
    /// protocols report no literal fields.
    pub(crate) async fn literal_input_fields(&self, name: &str) -> Vec<String> {
        self.find_protocol(name, true)
            .await
            .map_or_else(Vec::new, |protocol| {
                protocol
                    .literal_input_fields()
                    .iter()
                    .map(|field| (*field).to_string())
                    .collect()
            })
    }

    /// Validate one planned step operation without running it: the protocol
    /// exists and is selected, it supports the requested operation, and its
    /// help page (and every shared prerequisite) is loaded.
    pub(crate) async fn validate_step_operation(
        &self,
        name: &str,
        target: &str,
        exec: bool,
    ) -> Result<()> {
        // Resolve through the active protocol selection so validation
        // accepts exactly the protocols dispatch will find.
        let protocol = self
            .find_protocol(name, true)
            .await
            .ok_or_else(|| self.unknown_protocol_error(name, true))?;
        let descriptor = protocol.descriptor();
        if exec {
            if !descriptor.can_exec {
                bail!(
                    "protocol {name} does not support exec; call help([{name:?}]) and use its documented read operations"
                );
            }
        } else if !descriptor.can_read {
            bail!(
                "protocol does not support read: {name}; call help([{name:?}]) and use its documented exec operations"
            );
        }
        self.reject_help_address(name, target)?;
        let help_read = self.help_read.lock().await;
        let dependencies: &[String] = protocol.help_dependencies();
        if let Some(dependency) = dependencies
            .iter()
            .find(|dependency| !help_read.contains(*dependency))
        {
            return Err(ProtocolHelpRequired::dependency(dependency, name).into());
        }
        if !help_read.contains(name) {
            return Err(ProtocolHelpRequired::new(name).into());
        }
        Ok(())
    }

    /// Load help pages for the requested protocols through the help tool.
    /// Shared help prerequisites are resolved and included automatically, and
    /// every returned protocol is marked as loaded for the session.
    pub(crate) async fn load_help(&self, requested: &[String]) -> Result<String> {
        const MAX_HELP_PROTOCOLS: usize = 8;
        if requested.is_empty() {
            bail!("help requires at least one protocol name");
        }
        let limited = requested.len() > MAX_HELP_PROTOCOLS;
        let skipped: Vec<String> = if limited {
            requested[MAX_HELP_PROTOCOLS..]
                .iter()
                .map(|name| format!("{name:?}"))
                .collect()
        } else {
            Vec::new()
        };
        let requested = &requested[..requested.len().min(MAX_HELP_PROTOCOLS)];
        // Resolve each protocol exactly once, dependencies ahead of the
        // protocols that list them, so dynamic sources are readied once.
        let mut queue = requested.to_vec();
        let mut ordered = Vec::<String>::new();
        let mut resolved = Vec::<Arc<dyn Protocol>>::new();
        while let Some(name) = queue.first().cloned() {
            queue.remove(0);
            if ordered.contains(&name) {
                continue;
            }
            let protocol = self
                .find_protocol(&name, true)
                .await
                .ok_or_else(|| self.unknown_protocol_error(&name, true))?;
            for dependency in protocol.help_dependencies() {
                if !ordered.contains(dependency) && !queue.contains(dependency) {
                    queue.push(dependency.clone());
                }
            }
            ordered.push(name);
            resolved.push(protocol);
        }
        let mut sections = Vec::with_capacity(ordered.len());
        for (name, protocol) in ordered.iter().zip(resolved) {
            let uri = format!("{name}://help");
            let input = Map::new();
            let response = protocol
                .read_output(
                    ProtocolRequest {
                        uri: &uri,
                        target: "help",
                        input: &input,
                    },
                    self.context.clone(),
                )
                .await?;
            let (content, json, images) = response.into_parts();
            if !images.is_empty() {
                bail!("protocol {name} help must be text-only");
            }
            if json.is_some() {
                bail!("protocol {name} help must be plain text");
            }
            sections.push(self.output.present(content, name).await?);
        }
        self.help_read.lock().await.extend(ordered);
        let mut result = format!(
            "{}\n\nLoaded protocols stay loaded for the rest of the session; use their addresses without calling help again.",
            sections.join("\n\n---\n\n")
        );
        if limited {
            let _ = write!(
                result,
                "\n\nhelp loads at most {MAX_HELP_PROTOCOLS} protocols per call; the remaining names were skipped: [{}] — call help again with them.",
                skipped.join(", ")
            );
        }
        Ok(result)
    }

    fn reject_help_address(&self, name: &str, target: &str) -> Result<()> {
        if target == "help" {
            bail!(
                "help pages are loaded with the help tool; call help([{name:?}]) and use the documented addresses"
            );
        }
        Ok(())
    }

    pub async fn restore_help_read_names(&self, protocols: HashSet<String>) {
        let mut restored = HashSet::new();
        for name in protocols {
            restored.insert(name.clone());
            // Best-effort expansion over statically registered protocols: a
            // restored session may have loaded shared prerequisites through the
            // help tool's automatic dependency resolution.
            if let Some(protocol) = self.protocols.get(&name) {
                for dependency in protocol.help_dependencies() {
                    restored.insert(dependency.clone());
                }
            }
        }
        self.help_read.lock().await.extend(restored);
    }

    pub(crate) async fn clear_help_reads(&self) {
        self.help_read.lock().await.clear();
    }

    pub(crate) fn contains_selected(&self, name: &str) -> bool {
        if self
            .allowed
            .read()
            .expect("protocol selection lock poisoned")
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(name))
        {
            return false;
        }
        self.protocols.contains_key(name)
    }

    async fn dispatch_read(
        &self,
        uri: &str,
        input: &Map<String, Value>,
        include_dynamic: bool,
        require_help: bool,
        pinned_foreground: bool,
    ) -> Result<PresentedProtocolOutput> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .find_protocol(name, include_dynamic)
            .await
            .ok_or_else(|| self.unknown_protocol_error(name, include_dynamic))?;
        let descriptor = protocol.descriptor();
        if !descriptor.can_read {
            bail!("protocol does not support read: {name}");
        }
        if require_help {
            self.reject_help_address(name, target)?;
            let help_read = self.help_read.lock().await;
            if let Some(dependency) = protocol
                .help_dependencies()
                .iter()
                .find(|dependency| !help_read.contains(*dependency))
            {
                return Err(ProtocolHelpRequired::dependency(dependency, name).into());
            }
            if !help_read.contains(name) {
                return Err(ProtocolHelpRequired::new(name).into());
            }
        }
        let mut context = self.context.clone();
        context.pinned_foreground = pinned_foreground;
        let response = protocol
            .read_output(ProtocolRequest { uri, target, input }, context)
            .await?;
        let (content, json, images) = response.into_parts();
        let text = String::from_utf8_lossy(&content).into_owned();
        let output = self.output.present(content, name).await?;
        Ok(PresentedProtocolOutput {
            output,
            text,
            json,
            images,
        })
    }

    async fn dispatch_exec_output(
        &self,
        uri: &str,
        input: &Map<String, Value>,
        include_dynamic: bool,
        require_help: bool,
        pinned_foreground: bool,
    ) -> Result<PresentedProtocolOutput> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .find_protocol(name, include_dynamic)
            .await
            .ok_or_else(|| self.unknown_protocol_error(name, include_dynamic))?;
        if require_help {
            self.reject_help_address(name, target)?;
            let help_read = self.help_read.lock().await;
            if let Some(dependency) = protocol
                .help_dependencies()
                .iter()
                .find(|dependency| !help_read.contains(*dependency))
            {
                return Err(ProtocolHelpRequired::dependency(dependency, name).into());
            }
            if !help_read.contains(name) {
                return Err(ProtocolHelpRequired::new(name).into());
            }
        }
        let descriptor = protocol.descriptor();
        if !descriptor.can_exec {
            bail!(
                "protocol {name} does not support exec; call help([{:?}]) and use its documented read operations",
                name
            );
        }
        let mut context = self.context.clone();
        context.pinned_foreground = pinned_foreground;
        let content = protocol
            .exec(ProtocolRequest { uri, target, input }, context)
            .await?;
        let (content, json, images) = content.into_parts();
        let text = String::from_utf8_lossy(&content).into_owned();
        let output = self.output.present(content, name).await?;
        Ok(PresentedProtocolOutput {
            output,
            text,
            json,
            images,
        })
    }

    async fn find_protocol(&self, name: &str, include_dynamic: bool) -> Option<Arc<dyn Protocol>> {
        if self
            .allowed
            .read()
            .expect("protocol selection lock poisoned")
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(name))
        {
            return None;
        }
        if let Some(protocol) = self.protocols.get(name) {
            return Some(protocol.clone());
        }
        if !include_dynamic {
            return None;
        }
        for source in &self.dynamic {
            if let Some(protocol) = source.protocol(name) {
                return Some(protocol);
            }
            let _ = source.ready().await;
            if let Some(protocol) = source.protocol(name) {
                return Some(protocol);
            }
        }
        None
    }

    fn unknown_protocol_error(&self, name: &str, include_dynamic: bool) -> anyhow::Error {
        let allowed = self
            .allowed
            .read()
            .expect("protocol selection lock poisoned");
        let mut names = self
            .protocols
            .keys()
            .filter(|name| {
                allowed
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(name.as_str()))
            })
            .cloned()
            .collect::<Vec<_>>();
        if include_dynamic {
            names.extend(
                self.dynamic
                    .iter()
                    .flat_map(|source| source.descriptors())
                    .map(|descriptor| descriptor.name)
                    .filter(|name| {
                        allowed
                            .as_ref()
                            .is_none_or(|allowed| allowed.contains(name.as_str()))
                    }),
            );
        }
        drop(allowed);
        names.sort();
        if names.is_empty() {
            return anyhow!("unknown protocol: {name}");
        }
        let close = close_protocol_names(name, &names);
        if close.is_empty() {
            anyhow!("unknown protocol: {name}; available: {}", names.join(", "))
        } else {
            anyhow!(
                "unknown protocol: {name}; did you mean {}? available: {}",
                close.join(", "),
                names.join(", ")
            )
        }
    }
}

/// Registered names that differ from `name` only in case and punctuation, or
/// that contain it or are contained in it, so `Bash` and `files` point to
/// `bash` and `file`.
fn close_protocol_names<'a>(name: &str, names: &'a [String]) -> Vec<&'a str> {
    let comparable = |text: &str| {
        text.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|character| character.to_ascii_lowercase())
            .collect::<String>()
    };
    let wanted = comparable(name);
    if wanted.is_empty() {
        return Vec::new();
    }
    let exact = names
        .iter()
        .filter(|candidate| comparable(candidate) == wanted)
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return exact;
    }
    names
        .iter()
        .filter(|candidate| {
            let candidate = comparable(candidate);
            !candidate.is_empty() && (candidate.contains(&wanted) || wanted.contains(&candidate))
        })
        .take(5)
        .map(String::as_str)
        .collect()
}

pub(crate) fn validate_descriptor(descriptor: &ProtocolDescriptor) -> Result<()> {
    if descriptor.name.is_empty() || descriptor.name.contains("://") {
        bail!("invalid protocol name: {:?}", descriptor.name);
    }
    if descriptor.description.trim().is_empty() {
        bail!("protocol {} requires a description", descriptor.name);
    }
    if !descriptor.can_read {
        bail!(
            "protocol {} must support read so the help tool can load its contract",
            descriptor.name
        );
    }
    Ok(())
}

pub(crate) fn validate_help_dependencies(protocol: &str, dependencies: &[String]) -> Result<()> {
    let mut unique = HashSet::new();
    for dependency in dependencies {
        if dependency.is_empty() || dependency.contains("://") {
            bail!("invalid help dependency for protocol {protocol}: {dependency:?}");
        }
        if dependency == protocol {
            bail!("protocol {protocol} cannot depend on its own help");
        }
        if !unique.insert(dependency) {
            bail!("protocol {protocol} repeats help dependency {dependency}");
        }
    }
    Ok(())
}

pub fn split_address(uri: &str) -> Result<(&str, &str)> {
    let (name, target) = uri
        .split_once("://")
        .ok_or_else(|| anyhow!("address must have the form <protocol>://<target>"))?;
    if name.is_empty() {
        bail!("protocol name cannot be empty");
    }
    Ok((name, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, PartialEq)]
    struct CapturedRequest {
        uri: String,
        target: String,
        input: Value,
    }

    struct CaptureProtocol {
        capture: Arc<Mutex<Option<CapturedRequest>>>,
    }

    struct NamedProtocol(String);

    struct DependentProtocol {
        name: String,
        help_dependencies: Vec<String>,
    }

    #[async_trait]
    impl Protocol for NamedProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: self.0.clone(),
                description: "dynamic test".to_string(),
                can_read: true,
                can_exec: false,
            }
        }

        async fn read(
            &self,
            _request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            Ok(ProtocolOutput::text(self.0.as_bytes().to_vec()))
        }
    }

    struct DeferredSource {
        name: String,
        protocol: Mutex<Option<Arc<dyn Protocol>>>,
        ready_calls: AtomicUsize,
    }

    #[async_trait]
    impl DynamicProtocolSource for DeferredSource {
        async fn ready(&self) -> Result<()> {
            self.ready_calls.fetch_add(1, Ordering::Relaxed);
            *self.protocol.lock().unwrap() = Some(Arc::new(NamedProtocol(self.name.clone())));
            Ok(())
        }

        fn descriptors(&self) -> Vec<ProtocolDescriptor> {
            self.protocol
                .lock()
                .unwrap()
                .iter()
                .map(|protocol| protocol.descriptor())
                .collect()
        }

        fn protocol(&self, name: &str) -> Option<Arc<dyn Protocol>> {
            self.protocol
                .lock()
                .unwrap()
                .as_ref()
                .filter(|protocol| protocol.descriptor().name == name)
                .cloned()
        }
    }

    #[async_trait]
    impl Protocol for CaptureProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "capture".to_string(),
                description: "test".to_string(),
                can_read: true,
                can_exec: true,
            }
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            *self.capture.lock().unwrap() = Some(request.captured());
            Ok(ProtocolOutput::text("ok"))
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            *self.capture.lock().unwrap() = Some(request.captured());
            Ok(ProtocolOutput::text("ok"))
        }
    }

    #[async_trait]
    impl Protocol for DependentProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: self.name.clone(),
                description: "dependent test protocol".to_string(),
                can_read: true,
                can_exec: false,
            }
        }

        fn help_dependencies(&self) -> &[String] {
            &self.help_dependencies
        }

        async fn read(
            &self,
            _request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            Ok(ProtocolOutput::text(self.name.as_bytes().to_vec()))
        }
    }

    impl ProtocolRequest<'_> {
        fn captured(&self) -> CapturedRequest {
            CapturedRequest {
                uri: self.uri.to_string(),
                target: self.target.to_string(),
                input: Value::Object(self.input.clone()),
            }
        }
    }

    fn empty_input() -> Map<String, Value> {
        Map::new()
    }

    #[tokio::test]
    async fn registry_passes_input_to_every_protocol() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let capture = Arc::new(Mutex::new(None));
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: capture.clone(),
            })
            .unwrap();
        registry.load_help(&["capture".to_string()]).await.unwrap();
        let input: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "limit": 10,
            "nested": {"deep": [1, null, true]},
        }))
        .unwrap();

        registry
            .read_for_model("capture://value", &input, false)
            .await
            .unwrap();

        assert_eq!(
            capture.lock().unwrap().as_ref().unwrap(),
            &CapturedRequest {
                uri: "capture://value".to_string(),
                target: "value".to_string(),
                input: Value::Object(input.clone()),
            }
        );

        // Calls without input are unaffected.
        assert_eq!(
            registry
                .read("capture://value", &empty_input())
                .await
                .unwrap(),
            "ok"
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[test]
    fn address_split_is_deliberately_not_url_parsing() {
        assert_eq!(
            split_address("odd protocol://a://b?x=a b").unwrap(),
            ("odd protocol", "a://b?x=a b")
        );
    }

    #[test]
    fn address_requires_an_unambiguous_separator() {
        assert!(split_address("file/path").is_err());
        assert!(split_address("://path").is_err());
    }

    #[test]
    fn image_media_types_are_detected_from_signatures() {
        assert_eq!(
            ProtocolImageMediaType::detect(b"\xff\xd8\xffjpeg"),
            Some(ProtocolImageMediaType::Jpeg)
        );
        assert_eq!(
            ProtocolImageMediaType::detect(b"\x89PNG\r\n\x1a\npng"),
            Some(ProtocolImageMediaType::Png)
        );
        assert_eq!(
            ProtocolImageMediaType::detect(b"GIF89agif"),
            Some(ProtocolImageMediaType::Gif)
        );
        assert_eq!(
            ProtocolImageMediaType::detect(b"RIFF\x04\0\0\0WEBPdata"),
            Some(ProtocolImageMediaType::Webp)
        );
        assert_eq!(ProtocolImageMediaType::detect(b"not an image"), None);
    }

    #[test]
    fn protocol_descriptors_require_help_reads_and_descriptions() {
        let descriptor = ProtocolDescriptor {
            name: "example".to_string(),
            description: "Example protocol".to_string(),
            can_read: true,
            can_exec: false,
        };
        assert!(validate_descriptor(&descriptor).is_ok());
        assert!(
            validate_descriptor(&ProtocolDescriptor {
                can_read: false,
                ..descriptor.clone()
            })
            .unwrap_err()
            .to_string()
            .contains("must support read")
        );
        assert!(
            validate_descriptor(&ProtocolDescriptor {
                description: "  ".to_string(),
                ..descriptor
            })
            .unwrap_err()
            .to_string()
            .contains("requires a description")
        );
    }

    #[tokio::test]
    async fn registry_passes_opaque_uri_and_input_unchanged() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let capture = Arc::new(Mutex::new(None));
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: capture.clone(),
            })
            .unwrap();
        let input: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "markdown is fine": "a://b?not=a url",
            "nested": [1, null, true],
        }))
        .unwrap();
        registry.load_help(&["capture".to_string()]).await.unwrap();

        let result = registry
            .read("capture://a://b?not=a url", &input)
            .await
            .unwrap();

        assert_eq!(result, "ok");
        assert_eq!(
            capture.lock().unwrap().as_ref().unwrap(),
            &CapturedRequest {
                uri: "capture://a://b?not=a url".to_string(),
                target: "a://b?not=a url".to_string(),
                input: Value::Object(input.clone()),
            }
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn registry_does_not_interpret_exec_query_options() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let capture = Arc::new(Mutex::new(None));
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: capture.clone(),
            })
            .unwrap();
        let input: Map<String, Value> =
            serde_json::from_value(serde_json::json!({"wait": 30})).unwrap();
        registry.load_help(&["capture".to_string()]).await.unwrap();

        let result = registry
            .exec_for_model("capture://run?wait=30", &input, false)
            .await
            .unwrap();

        assert_eq!(result.output, "ok");
        assert_eq!(
            capture.lock().unwrap().as_ref().unwrap(),
            &CapturedRequest {
                uri: "capture://run?wait=30".to_string(),
                target: "run?wait=30".to_string(),
                input: Value::Object(input.clone()),
            }
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn protocol_names_cannot_collide() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: Arc::new(Mutex::new(None)),
            })
            .unwrap();
        let error = registry
            .register(CaptureProtocol {
                capture: Arc::new(Mutex::new(None)),
            })
            .unwrap_err();
        assert!(error.to_string().contains("already registered"));
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn selection_limits_protocol_descriptors_and_dispatch() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(NamedProtocol("first".to_string()))
            .unwrap();
        registry
            .register(NamedProtocol("second".to_string()))
            .unwrap();

        registry.select(Some(&["second".to_string()])).unwrap();

        assert_eq!(
            registry
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>(),
            vec!["second"]
        );
        registry.load_help(&["second".to_string()]).await.unwrap();
        assert_eq!(
            registry
                .load_help(&["first".to_string()])
                .await
                .unwrap_err()
                .to_string(),
            "unknown protocol: first; available: second"
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[test]
    fn unknown_protocol_names_close_matches() {
        let names = ["bash", "file", "github-mcp", "search"].map(String::from);
        assert_eq!(close_protocol_names("Bash", &names), ["bash"]);
        assert_eq!(close_protocol_names("files", &names), ["file"]);
        assert_eq!(close_protocol_names("github_mcp", &names), ["github-mcp"]);
        assert_eq!(close_protocol_names("github", &names), ["github-mcp"]);
        assert!(close_protocol_names("tasks", &names).is_empty());
        assert!(close_protocol_names("://", &names).is_empty());
    }

    #[tokio::test]
    async fn first_model_call_must_load_protocol_help() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let capture = Arc::new(Mutex::new(None));
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: capture.clone(),
            })
            .unwrap();

        for error in [
            registry
                .read("capture://value", &empty_input())
                .await
                .unwrap_err(),
            registry
                .exec_for_model("capture://run", &empty_input(), false)
                .await
                .unwrap_err(),
        ] {
            assert!(error.downcast_ref::<ProtocolHelpRequired>().is_some());
            assert_eq!(
                error.to_string(),
                "Load this protocol first: call help([\"capture\"]) before using capture://."
            );
        }
        assert!(capture.lock().unwrap().is_none());

        let help = registry.load_help(&["capture".to_string()]).await.unwrap();
        assert!(help.contains("Loaded protocols stay loaded"));
        assert_eq!(
            registry
                .read("capture://value", &empty_input())
                .await
                .unwrap(),
            "ok"
        );
        assert_eq!(
            registry
                .exec_for_model("capture://run", &empty_input(), false)
                .await
                .unwrap()
                .output,
            "ok"
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn help_tool_rejects_help_addresses_and_empty_batches() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: Arc::new(Mutex::new(None)),
            })
            .unwrap();

        for error in [
            registry
                .read("capture://help", &empty_input())
                .await
                .unwrap_err(),
            registry
                .exec_for_model("capture://help", &empty_input(), false)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(
                error.to_string(),
                "help pages are loaded with the help tool; call help([\"capture\"]) and use the documented addresses"
            );
        }
        assert!(
            registry
                .load_help(&[])
                .await
                .unwrap_err()
                .to_string()
                .contains("at least one protocol name")
        );
        assert!(
            registry
                .load_help(&["missing".to_string()])
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown protocol: missing")
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn help_loads_only_the_first_eight_protocols_when_more_are_requested() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        for name in (1..=9).map(|index| format!("p{index}")) {
            registry.register(NamedProtocol(name)).unwrap();
        }

        let requested = (1..=9).map(|index| format!("p{index}")).collect::<Vec<_>>();
        let help = registry.load_help(&requested).await.unwrap();
        assert!(help.contains("Loaded protocols stay loaded"));
        assert!(help.contains(r#"the remaining names were skipped: ["p9"]"#));

        assert_eq!(
            registry.read("p1://value", &empty_input()).await.unwrap(),
            "p1",
            "the first requested protocol is loaded and unlocked"
        );
        assert_eq!(
            registry.read("p8://value", &empty_input()).await.unwrap(),
            "p8",
            "the eighth requested protocol is loaded and unlocked"
        );
        let error = registry
            .read("p9://value", &empty_input())
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<ProtocolHelpRequired>().is_some());
        assert_eq!(
            error.to_string(),
            "Load this protocol first: call help([\"p9\"]) before using p9://."
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn shared_help_is_loaded_automatically_before_a_dependent_protocol() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(NamedProtocol("shared".to_string()))
            .unwrap();
        registry
            .register(DependentProtocol {
                name: "dependent".to_string(),
                help_dependencies: vec!["shared".to_string()],
            })
            .unwrap();

        assert!(
            registry
                .select(Some(&["dependent".to_string()]))
                .unwrap_err()
                .to_string()
                .contains("also requires selecting protocol shared")
        );
        registry
            .select(Some(&["shared".to_string(), "dependent".to_string()]))
            .unwrap();

        let error = registry
            .read("dependent://value", &empty_input())
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<ProtocolHelpRequired>().is_some());
        assert_eq!(
            error.to_string(),
            "Load the shared prerequisite first: call help([\"shared\"]) before using dependent://."
        );

        let help = registry
            .load_help(&["dependent".to_string(), "dependent".to_string()])
            .await
            .unwrap();
        assert!(help.contains("shared"));
        assert!(help.contains("dependent"));
        assert_eq!(
            registry
                .read("dependent://value", &empty_input())
                .await
                .unwrap(),
            "dependent"
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn registry_supports_multiple_deferred_dynamic_sources() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let first = Arc::new(DeferredSource {
            name: "first".to_string(),
            protocol: Mutex::new(None),
            ready_calls: AtomicUsize::new(0),
        });
        let second = Arc::new(DeferredSource {
            name: "second".to_string(),
            protocol: Mutex::new(None),
            ready_calls: AtomicUsize::new(0),
        });
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry.set_dynamic_source(first.clone()).unwrap();
        registry.set_dynamic_source(second.clone()).unwrap();

        let help = registry.load_help(&["second".to_string()]).await.unwrap();
        assert!(help.contains("second"));
        assert_eq!(first.ready_calls.load(Ordering::Relaxed), 1);
        assert_eq!(second.ready_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            registry
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[tokio::test]
    async fn validate_step_operation_rejects_unknown_unsupported_and_unloaded_protocols() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(NamedProtocol("reader".to_string()))
            .unwrap();
        registry
            .register(CaptureProtocol {
                capture: Arc::new(Mutex::new(None)),
            })
            .unwrap();

        assert!(
            registry
                .validate_step_operation("missing", "x", false)
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown protocol: missing")
        );
        assert!(
            registry
                .validate_step_operation("reader", "x", true)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not support exec")
        );
        assert!(
            registry
                .validate_step_operation("capture", "x", false)
                .await
                .unwrap_err()
                .downcast_ref::<ProtocolHelpRequired>()
                .is_some()
        );
        registry.load_help(&["capture".to_string()]).await.unwrap();
        registry
            .validate_step_operation("capture", "x", false)
            .await
            .unwrap();
        registry
            .validate_step_operation("capture", "x", true)
            .await
            .unwrap();
        assert!(
            registry
                .validate_step_operation("capture", "help", false)
                .await
                .unwrap_err()
                .to_string()
                .contains("help pages are loaded with the help tool")
        );
        registry.select(Some(&["reader".to_string()])).unwrap();
        assert!(
            registry
                .validate_step_operation("capture", "x", false)
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown protocol: capture"),
            "validation honors the active protocol selection"
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }
}
