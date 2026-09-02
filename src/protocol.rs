use crate::output::OutputStore;
use crate::prompts::PromptEntry;
use crate::session::{EventKind, SessionEvent};
use crate::task::TaskManager;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use rig::message::{ImageMediaType, ToolResultContent};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::RwLock;
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
                "Read \"{}://help\" with an empty body before using this protocol.",
                self.required
            )
        } else {
            write!(
                formatter,
                "Read \"{}://help\" with an empty body before using {}://.",
                self.required, self.requested
            )
        }
    }
}

impl std::error::Error for ProtocolHelpRequired {}

#[derive(Clone)]
pub struct ProtocolContext {
    pub tasks: TaskManager,
}

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
    pub body: &'a str,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolReadOutput {
    content: Vec<u8>,
    images: Vec<ProtocolImage>,
}

impl ProtocolReadOutput {
    pub fn new(content: Vec<u8>, images: Vec<ProtocolImage>) -> Self {
        Self { content, images }
    }

    pub fn content(&self) -> &[u8] {
        &self.content
    }

    pub fn images(&self) -> &[ProtocolImage] {
        &self.images
    }

    pub(crate) fn into_parts(self) -> (Vec<u8>, Vec<ProtocolImage>) {
        (self.content, self.images)
    }
}

impl From<Vec<u8>> for ProtocolReadOutput {
    fn from(content: Vec<u8>) -> Self {
        Self::new(content, Vec::new())
    }
}

pub(crate) struct PresentedProtocolRead {
    pub output: String,
    pub images: Vec<ProtocolImage>,
}

#[async_trait]
pub trait Protocol: Send + Sync {
    fn descriptor(&self) -> ProtocolDescriptor;

    /// Additional protocol help pages that must be read before this protocol's
    /// own help or operations. The protocol's own `<name>://help` remains
    /// mandatory and is read after these shared prerequisites.
    fn help_dependencies(&self) -> &[String] {
        &[]
    }

    async fn read(
        &self,
        _request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        bail!("this protocol does not support read")
    }

    async fn read_output(
        &self,
        request: ProtocolRequest<'_>,
        context: ProtocolContext,
    ) -> Result<ProtocolReadOutput> {
        self.read(request, context).await.map(Into::into)
    }

    async fn exec(
        &self,
        _request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
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
            context: ProtocolContext { tasks },
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

    pub fn prompt_protocols(&self) -> Vec<PromptEntry> {
        self.descriptors()
            .into_iter()
            .map(|descriptor| PromptEntry {
                name: descriptor.name,
                description: descriptor.description,
            })
            .collect()
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

    pub async fn read(&self, uri: &str, body: &str) -> Result<String> {
        Ok(self.dispatch_read(uri, body, true, true).await?.output)
    }

    pub(crate) async fn read_for_model(
        &self,
        uri: &str,
        body: &str,
    ) -> Result<PresentedProtocolRead> {
        self.dispatch_read(uri, body, true, true).await
    }

    pub async fn exec(&self, uri: &str, body: &str) -> Result<String> {
        self.dispatch_exec(uri, body, true, true).await
    }

    pub(crate) async fn read_static(&self, uri: &str, body: &str) -> Result<String> {
        Ok(self.dispatch_read(uri, body, false, false).await?.output)
    }

    pub(crate) async fn exec_static(&self, uri: &str, body: &str) -> Result<String> {
        self.dispatch_exec(uri, body, false, false).await
    }

    pub async fn restore_help_reads(&self, events: &[SessionEvent]) {
        let mut pending = HashMap::new();
        let mut restored = HashSet::new();
        for event in events {
            match &event.kind {
                EventKind::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    pending.remove(call_id);
                    if name == "read"
                        && let (Some(uri), Some("")) = (
                            arguments.get("uri").and_then(|value| value.as_str()),
                            arguments.get("body").and_then(|value| value.as_str()),
                        )
                        && let Ok((protocol, "help")) = split_address(uri)
                    {
                        pending.insert(call_id.clone(), protocol.to_string());
                    }
                }
                EventKind::ToolResult {
                    call_id,
                    name,
                    failed,
                    ..
                } => {
                    if let Some(protocol) = pending.remove(call_id)
                        && name == "read"
                        && !failed
                    {
                        restored.insert(protocol);
                    }
                }
                _ => {}
            }
        }
        self.help_read.lock().await.extend(restored);
    }

    pub async fn restore_help_read_names(&self, protocols: HashSet<String>) {
        self.help_read.lock().await.extend(protocols);
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
        body: &str,
        include_dynamic: bool,
        require_help: bool,
    ) -> Result<PresentedProtocolRead> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .find_protocol(name, include_dynamic)
            .await
            .ok_or_else(|| anyhow!("unknown protocol: {name}"))?;
        let descriptor = protocol.descriptor();
        if !descriptor.can_read {
            bail!("protocol does not support read: {name}");
        }
        if require_help {
            let help_read = self.help_read.lock().await;
            if let Some(dependency) = protocol
                .help_dependencies()
                .iter()
                .find(|dependency| !help_read.contains(*dependency))
            {
                return Err(ProtocolHelpRequired::dependency(dependency, name).into());
            }
            if target != "help" && !help_read.contains(name) {
                return Err(ProtocolHelpRequired::new(name).into());
            }
        }
        let response = protocol
            .read_output(ProtocolRequest { uri, target, body }, self.context.clone())
            .await?;
        let (content, images) = response.into_parts();
        let output = self.output.present(content, name).await?;
        if require_help && target == "help" && body.is_empty() {
            self.help_read.lock().await.insert(name.to_string());
        }
        Ok(PresentedProtocolRead { output, images })
    }

    async fn dispatch_exec(
        &self,
        uri: &str,
        body: &str,
        include_dynamic: bool,
        require_help: bool,
    ) -> Result<String> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .find_protocol(name, include_dynamic)
            .await
            .ok_or_else(|| anyhow!("unknown protocol: {name}"))?;
        if require_help {
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
                r#"protocol {name} does not support exec; read("{name}://help", "") for its supported operations"#
            );
        }
        let content = protocol
            .exec(ProtocolRequest { uri, target, body }, self.context.clone())
            .await?;
        self.output.present(content, name).await
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
            "protocol {} must support read so <protocol>://help is available",
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
        body: String,
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
        ) -> Result<Vec<u8>> {
            Ok(self.0.as_bytes().to_vec())
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
        ) -> Result<Vec<u8>> {
            *self.capture.lock().unwrap() = Some(CapturedRequest {
                uri: request.uri.to_string(),
                target: request.target.to_string(),
                body: request.body.to_string(),
            });
            Ok(b"ok".to_vec())
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<Vec<u8>> {
            *self.capture.lock().unwrap() = Some(CapturedRequest {
                uri: request.uri.to_string(),
                target: request.target.to_string(),
                body: request.body.to_string(),
            });
            Ok(b"ok".to_vec())
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
        ) -> Result<Vec<u8>> {
            Ok(self.name.as_bytes().to_vec())
        }
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
    async fn registry_passes_opaque_uri_and_string_body_unchanged() {
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
        let body = r#"["markdown is fine",{"nested":[1,null,true]}]"#;
        registry.read("capture://help", "").await.unwrap();

        let result = registry
            .read("capture://a://b?not=a url", body)
            .await
            .unwrap();

        assert_eq!(result, "ok");
        assert_eq!(
            capture.lock().unwrap().as_ref().unwrap(),
            &CapturedRequest {
                uri: "capture://a://b?not=a url".to_string(),
                target: "a://b?not=a url".to_string(),
                body: body.to_string(),
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
        let body = "unchanged";
        registry.read("capture://help", "").await.unwrap();

        let result = registry.exec("capture://run?wait=30", body).await.unwrap();

        assert_eq!(result, "ok");
        assert_eq!(
            capture.lock().unwrap().as_ref().unwrap(),
            &CapturedRequest {
                uri: "capture://run?wait=30".to_string(),
                target: "run?wait=30".to_string(),
                body: body.to_string(),
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
        assert_eq!(registry.read("second://help", "").await.unwrap(), "second");
        assert_eq!(
            registry
                .read("first://help", "")
                .await
                .unwrap_err()
                .to_string(),
            "unknown protocol: first"
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn first_model_call_must_read_protocol_help() {
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
            registry.read("capture://value", "").await.unwrap_err(),
            registry.exec("capture://run", "").await.unwrap_err(),
        ] {
            assert!(error.downcast_ref::<ProtocolHelpRequired>().is_some());
            assert_eq!(
                error.to_string(),
                "Read \"capture://help\" with an empty body before using this protocol."
            );
        }
        assert!(capture.lock().unwrap().is_none());

        registry.read("capture://help", "").await.unwrap();
        assert_eq!(registry.read("capture://value", "").await.unwrap(), "ok");
        assert_eq!(registry.exec("capture://run", "").await.unwrap(), "ok");
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn shared_help_is_required_before_a_dependent_protocols_own_help() {
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

        let error = registry.read("dependent://help", "").await.unwrap_err();
        assert!(error.downcast_ref::<ProtocolHelpRequired>().is_some());
        assert_eq!(
            error.to_string(),
            "Read \"shared://help\" with an empty body before using dependent://."
        );

        registry.read("shared://help", "").await.unwrap();
        assert_eq!(
            registry
                .read("dependent://value", "")
                .await
                .unwrap_err()
                .to_string(),
            "Read \"dependent://help\" with an empty body before using this protocol."
        );
        registry.read("dependent://help", "").await.unwrap();
        assert_eq!(
            registry.read("dependent://value", "").await.unwrap(),
            "dependent"
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn malformed_help_and_static_calls_do_not_unlock_model_calls() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: Arc::new(Mutex::new(None)),
            })
            .unwrap();

        registry.read("capture://help", "unexpected").await.unwrap();
        assert!(
            registry
                .read("capture://value", "")
                .await
                .unwrap_err()
                .downcast_ref::<ProtocolHelpRequired>()
                .is_some()
        );
        registry.read_static("capture://value", "").await.unwrap();
        registry.exec_static("capture://run", "").await.unwrap();
        assert!(
            registry
                .read("capture://value", "")
                .await
                .unwrap_err()
                .downcast_ref::<ProtocolHelpRequired>()
                .is_some()
        );
        let _ = tokio::fs::remove_dir_all(output_directory).await;
    }

    #[tokio::test]
    async fn restored_successful_help_read_unlocks_protocol() {
        let session_id = format!("test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        registry
            .register(CaptureProtocol {
                capture: Arc::new(Mutex::new(None)),
            })
            .unwrap();
        let events = vec![
            SessionEvent {
                sequence: 1,
                at: chrono::Utc::now(),
                kind: EventKind::ToolCall {
                    call_id: "help-call".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({
                        "uri": "capture://help",
                        "body": ""
                    }),
                },
            },
            SessionEvent {
                sequence: 2,
                at: chrono::Utc::now(),
                kind: EventKind::ToolResult {
                    call_id: "help-call".to_string(),
                    name: "read".to_string(),
                    output: "help".to_string(),
                    failed: false,
                    protocol_help_required: false,
                },
            },
        ];

        registry.restore_help_reads(&events).await;

        assert_eq!(registry.read("capture://value", "").await.unwrap(), "ok");
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

        assert_eq!(registry.read("second://help", "").await.unwrap(), "second");
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
}
