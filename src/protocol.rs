use crate::output::OutputStore;
use crate::prompts::PromptEntry;
use crate::task::TaskManager;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

#[derive(Clone)]
pub struct ProtocolContext {
    pub tasks: TaskManager,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[async_trait]
pub trait Protocol: Send + Sync {
    fn descriptor(&self) -> ProtocolDescriptor;

    async fn read(
        &self,
        _request: ProtocolRequest<'_>,
        _context: ProtocolContext,
    ) -> Result<Vec<u8>> {
        bail!("this protocol does not support read")
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
}

impl ProtocolRegistry {
    pub fn new(output: Arc<OutputStore>, tasks: TaskManager) -> Self {
        Self {
            protocols: BTreeMap::new(),
            dynamic: Vec::new(),
            output,
            context: ProtocolContext { tasks },
        }
    }

    pub fn register(&mut self, protocol: impl Protocol + 'static) -> Result<()> {
        let protocol: Arc<dyn Protocol> = Arc::new(protocol);
        let descriptor = protocol.descriptor();
        validate_descriptor(&descriptor)?;
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

    pub fn descriptors(&self) -> Vec<ProtocolDescriptor> {
        let mut descriptors = self
            .protocols
            .values()
            .map(|protocol| protocol.descriptor())
            .collect::<Vec<_>>();
        for dynamic in &self.dynamic {
            descriptors.extend(dynamic.descriptors());
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

    pub(crate) fn tasks(&self) -> TaskManager {
        self.context.tasks.clone()
    }

    pub(crate) async fn present(&self, content: Vec<u8>, hint: &str) -> Result<String> {
        self.output.present(content, hint).await
    }

    pub async fn read(&self, uri: &str, body: &str) -> Result<String> {
        self.dispatch_read(uri, body, true).await
    }

    pub async fn exec(&self, uri: &str, body: &str) -> Result<String> {
        self.dispatch_exec(uri, body, true).await
    }

    pub(crate) async fn read_static(&self, uri: &str, body: &str) -> Result<String> {
        self.dispatch_read(uri, body, false).await
    }

    pub(crate) async fn exec_static(&self, uri: &str, body: &str) -> Result<String> {
        self.dispatch_exec(uri, body, false).await
    }

    async fn dispatch_read(&self, uri: &str, body: &str, include_dynamic: bool) -> Result<String> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .find_protocol(name, include_dynamic)
            .await
            .ok_or_else(|| anyhow!("unknown protocol: {name}"))?;
        let descriptor = protocol.descriptor();
        if !descriptor.can_read {
            bail!("protocol does not support read: {name}");
        }
        let content = protocol
            .read(ProtocolRequest { uri, target, body }, self.context.clone())
            .await?;
        self.output.present(content, name).await
    }

    async fn dispatch_exec(&self, uri: &str, body: &str, include_dynamic: bool) -> Result<String> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .find_protocol(name, include_dynamic)
            .await
            .ok_or_else(|| anyhow!("unknown protocol: {name}"))?;
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
