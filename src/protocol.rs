use crate::output::OutputStore;
use crate::prompts::ProtocolPrompt;
use crate::task::TaskManager;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
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
    pub body: Option<&'a Value>,
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

pub struct ProtocolRegistry {
    protocols: BTreeMap<String, Arc<dyn Protocol>>,
    output: Arc<OutputStore>,
    context: ProtocolContext,
}

impl ProtocolRegistry {
    pub fn new(output: Arc<OutputStore>, tasks: TaskManager) -> Self {
        Self {
            protocols: BTreeMap::new(),
            output,
            context: ProtocolContext { tasks },
        }
    }

    pub fn register(&mut self, protocol: impl Protocol + 'static) -> Result<()> {
        self.register_arc(Arc::new(protocol))
    }

    pub fn register_boxed(&mut self, protocol: Box<dyn Protocol>) -> Result<()> {
        self.register_arc(Arc::from(protocol))
    }

    fn register_arc(&mut self, protocol: Arc<dyn Protocol>) -> Result<()> {
        let descriptor = protocol.descriptor();
        validate_descriptor(&descriptor)?;
        if self.protocols.contains_key(&descriptor.name) {
            bail!("protocol name is already registered: {}", descriptor.name);
        }
        self.protocols.insert(descriptor.name, protocol);
        Ok(())
    }

    pub fn descriptors(&self) -> Vec<ProtocolDescriptor> {
        self.protocols
            .values()
            .map(|protocol| protocol.descriptor())
            .collect()
    }

    pub fn prompt_protocols(&self) -> Vec<ProtocolPrompt> {
        self.descriptors()
            .into_iter()
            .map(|descriptor| ProtocolPrompt {
                name: descriptor.name,
                description: descriptor.description,
            })
            .collect()
    }

    pub async fn read(&self, uri: &str, body: Option<&Value>) -> Result<String> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .protocols
            .get(name)
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

    pub async fn exec(&self, uri: &str, body: Option<&Value>) -> Result<String> {
        let (name, target) = split_address(uri)?;
        let protocol = self
            .protocols
            .get(name)
            .ok_or_else(|| anyhow!("unknown protocol: {name}"))?;
        let descriptor = protocol.descriptor();
        if !descriptor.can_exec {
            bail!("protocol does not support exec: {name}");
        }
        let content = protocol
            .exec(ProtocolRequest { uri, target, body }, self.context.clone())
            .await?;
        self.output.present(content, name).await
    }
}

pub(crate) fn validate_descriptor(descriptor: &ProtocolDescriptor) -> Result<()> {
    if descriptor.name.is_empty() || descriptor.name.contains("://") {
        bail!("invalid protocol name: {:?}", descriptor.name);
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

    #[derive(Debug, PartialEq)]
    struct CapturedRequest {
        uri: String,
        target: String,
        body: Option<Value>,
    }

    struct CaptureProtocol {
        capture: Arc<Mutex<Option<CapturedRequest>>>,
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
                body: request.body.cloned(),
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
                body: request.body.cloned(),
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

    #[tokio::test]
    async fn registry_passes_opaque_uri_and_arbitrary_body_unchanged() {
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
        let body = serde_json::json!(["markdown is fine", {"nested": [1, null, true]}]);

        let result = registry
            .read("capture://a://b?not=a url", Some(&body))
            .await
            .unwrap();

        assert_eq!(result, "ok");
        assert_eq!(
            capture.lock().unwrap().as_ref().unwrap(),
            &CapturedRequest {
                uri: "capture://a://b?not=a url".to_string(),
                target: "a://b?not=a url".to_string(),
                body: Some(body),
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
        let body = serde_json::json!("unchanged");

        let result = registry
            .exec("capture://run?wait=30", Some(&body))
            .await
            .unwrap();

        assert_eq!(result, "ok");
        assert_eq!(
            capture.lock().unwrap().as_ref().unwrap(),
            &CapturedRequest {
                uri: "capture://run?wait=30".to_string(),
                target: "run?wait=30".to_string(),
                body: Some(body),
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
}
