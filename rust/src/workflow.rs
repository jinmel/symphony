use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::{self, Duration};
use tracing::error;

use crate::config::{CliOverrides, ConfigError, EffectiveConfig};

#[derive(Debug, Clone)]
pub struct WorkflowDefinition {
    pub config: Mapping,
    pub prompt_template: String,
}

#[derive(Debug, Clone)]
pub struct WorkflowRuntime {
    pub definition: WorkflowDefinition,
    pub config: EffectiveConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkflowFingerprint {
    modified_unix_secs: i64,
    file_len: u64,
    content_hash: [u8; 32],
}

#[derive(Debug)]
pub struct WorkflowStore {
    path: PathBuf,
    overrides: CliOverrides,
    runtime: Arc<RwLock<WorkflowRuntime>>,
    fingerprint: Arc<RwLock<WorkflowFingerprint>>,
}

#[derive(Debug, Error)]
pub enum WorkflowLoadError {
    #[error("missing_workflow_file: {path}: {source}")]
    MissingWorkflowFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("workflow_parse_error: {0}")]
    ParseError(String),
    #[error("workflow_front_matter_not_a_map")]
    FrontMatterNotMap,
}

#[derive(Debug, Error)]
pub enum WorkflowStoreError {
    #[error(transparent)]
    Load(#[from] WorkflowLoadError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl WorkflowStore {
    pub async fn open(
        path: PathBuf,
        overrides: CliOverrides,
    ) -> Result<Self, WorkflowStoreError> {
        let (runtime, fingerprint) = load_runtime(&path, &overrides).await?;
        Ok(Self {
            path,
            overrides,
            runtime: Arc::new(RwLock::new(runtime)),
            fingerprint: Arc::new(RwLock::new(fingerprint)),
        })
    }

    pub fn workflow_path(&self) -> &Path {
        &self.path
    }

    pub fn start_polling(self: &Arc<Self>) -> JoinHandle<()> {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;
                if let Err(error) = store.refresh_if_changed().await {
                    error!(
                        path = %store.path.display(),
                        error = %error,
                        "workflow reload failed; keeping last known good configuration"
                    );
                }
            }
        })
    }

    pub async fn current(&self) -> WorkflowRuntime {
        let _ = self.refresh_if_changed().await;
        self.runtime.read().await.clone()
    }

    pub async fn refresh_if_changed(&self) -> Result<bool, WorkflowStoreError> {
        let (fingerprint, maybe_runtime) = match fingerprint_for_path(&self.path).await {
            Ok(fingerprint) => {
                let current = self.fingerprint.read().await.clone();
                if current == fingerprint {
                    return Ok(false);
                }
                (fingerprint, Some(load_runtime(&self.path, &self.overrides).await?))
            }
            Err(error) => {
                return Err(WorkflowStoreError::Load(
                    WorkflowLoadError::MissingWorkflowFile {
                        path: self.path.clone(),
                        source: error,
                    },
                ));
            }
        };

        if let Some((runtime, _)) = maybe_runtime {
            *self.runtime.write().await = runtime;
            *self.fingerprint.write().await = fingerprint;
            return Ok(true);
        }

        Ok(false)
    }
}

pub async fn load_definition(path: &Path) -> Result<WorkflowDefinition, WorkflowLoadError> {
    let content = tokio::fs::read_to_string(path).await.map_err(|source| {
        WorkflowLoadError::MissingWorkflowFile {
            path: path.to_path_buf(),
            source,
        }
    })?;
    parse_definition(&content)
}

fn parse_definition(content: &str) -> Result<WorkflowDefinition, WorkflowLoadError> {
    let (front_matter, prompt_lines) = split_front_matter(content);
    let config = if front_matter.trim().is_empty() {
        Mapping::new()
    } else {
        match serde_yaml::from_str::<Value>(front_matter) {
            Ok(Value::Mapping(mapping)) => mapping,
            Ok(_) => return Err(WorkflowLoadError::FrontMatterNotMap),
            Err(error) => return Err(WorkflowLoadError::ParseError(error.to_string())),
        }
    };

    Ok(WorkflowDefinition {
        config,
        prompt_template: prompt_lines.trim().to_owned(),
    })
}

fn split_front_matter(content: &str) -> (&str, &str) {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return ("", content);
    };

    if let Some(index) = rest.find("\n---\n") {
        let (front, prompt) = rest.split_at(index);
        return (front, prompt.trim_start_matches("\n---\n"));
    }

    if let Some(index) = rest.find("\r\n---\r\n") {
        let (front, prompt) = rest.split_at(index);
        return (front, prompt.trim_start_matches("\r\n---\r\n"));
    }

    (rest, "")
}

async fn load_runtime(
    path: &Path,
    overrides: &CliOverrides,
) -> Result<(WorkflowRuntime, WorkflowFingerprint), WorkflowStoreError> {
    let definition = load_definition(path).await?;
    let mut config = EffectiveConfig::from_workflow_config(&definition.config)?;
    config.apply_overrides(overrides);
    let fingerprint = fingerprint_for_path(path).await.map_err(|source| {
        WorkflowStoreError::Load(WorkflowLoadError::MissingWorkflowFile {
            path: path.to_path_buf(),
            source,
        })
    })?;

    Ok((WorkflowRuntime { definition, config }, fingerprint))
}

async fn fingerprint_for_path(path: &Path) -> Result<WorkflowFingerprint, std::io::Error> {
    let metadata = tokio::fs::metadata(path).await?;
    let content = tokio::fs::read(path).await?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();

    let content_hash = Sha256::digest(content);
    let mut hash_bytes = [0_u8; 32];
    hash_bytes.copy_from_slice(&content_hash);

    Ok(WorkflowFingerprint {
        modified_unix_secs: modified,
        file_len: metadata.len(),
        content_hash: hash_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_front_matter_and_prompt_body() {
        let workflow = parse_definition(
            r#"---
tracker:
  kind: linear
---
Hello {{ issue.identifier }}
"#,
        )
        .unwrap();

        assert_eq!(
            workflow
                .config
                .get(Value::String("tracker".to_owned()))
                .is_some(),
            true
        );
        assert_eq!(workflow.prompt_template, "Hello {{ issue.identifier }}");
    }

    #[test]
    fn errors_when_front_matter_is_not_a_map() {
        let error = parse_definition(
            r#"---
- not
- a
- map
---
Prompt
"#,
        )
        .unwrap_err();

        assert!(matches!(error, WorkflowLoadError::FrontMatterNotMap));
    }
}
