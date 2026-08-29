use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;

const MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_MODEL_COUNT: usize = 64;

#[derive(Debug, Default)]
pub(crate) struct PreloadedModels {
    entries: HashMap<String, Arc<[u8]>>,
    total_bytes: usize,
}

impl PreloadedModels {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    #[cfg(test)]
    pub(crate) fn get(&self, id: &str) -> Option<Arc<[u8]>> {
        self.entries.get(id).cloned()
    }
}

#[derive(Debug, Default)]
pub(crate) struct ModelPreloadResult {
    pub(crate) models: PreloadedModels,
    pub(crate) errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelManifest {
    version: u32,
    models: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelEntry {
    id: String,
    file: PathBuf,
    #[serde(default)]
    expected_bytes: Option<u64>,
}

pub(crate) fn packaged_model_directory() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MAELSTROM_MODEL_DIR") {
        return Some(PathBuf::from(path));
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("models")))
}

pub(crate) fn preload_models(root: Option<&Path>) -> ModelPreloadResult {
    let Some(root) = root else {
        return ModelPreloadResult::default();
    };
    let manifest_path = root.join(MANIFEST_FILE);
    if !manifest_path.is_file() {
        return ModelPreloadResult::default();
    }

    let manifest_bytes = match fs::metadata(&manifest_path) {
        Ok(metadata) if metadata.len() <= MAX_MANIFEST_BYTES => match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return preload_error(format!(
                    "Could not read model manifest {}: {error}",
                    manifest_path.display()
                ));
            }
        },
        Ok(metadata) => {
            return preload_error(format!(
                "Model manifest {} is too large ({} bytes; maximum {MAX_MANIFEST_BYTES})",
                manifest_path.display(),
                metadata.len()
            ));
        }
        Err(error) => {
            return preload_error(format!(
                "Could not inspect model manifest {}: {error}",
                manifest_path.display()
            ));
        }
    };
    let manifest: ModelManifest = match serde_json::from_slice(&manifest_bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            return preload_error(format!(
                "Could not parse model manifest {}: {error}",
                manifest_path.display()
            ));
        }
    };
    if manifest.version != MANIFEST_VERSION {
        return preload_error(format!(
            "Unsupported model manifest version {} in {}; expected {MANIFEST_VERSION}",
            manifest.version,
            manifest_path.display()
        ));
    }
    if manifest.models.len() > MAX_MODEL_COUNT {
        return preload_error(format!(
            "Model manifest {} contains {} entries; maximum is {MAX_MODEL_COUNT}",
            manifest_path.display(),
            manifest.models.len()
        ));
    }

    let mut result = ModelPreloadResult::default();
    let mut ids = HashSet::new();
    for entry in manifest.models {
        if entry.id.trim().is_empty() {
            result
                .errors
                .push("Model manifest contains an empty id".into());
            continue;
        }
        if !ids.insert(entry.id.clone()) {
            result
                .errors
                .push(format!("Duplicate model id '{}'", entry.id));
            continue;
        }
        if !safe_relative_model_path(&entry.file) {
            result.errors.push(format!(
                "Model '{}' uses an unsafe path: {}",
                entry.id,
                entry.file.display()
            ));
            continue;
        }
        let path = root.join(&entry.file);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                result.errors.push(format!(
                    "Could not preload model '{}' from {}: {error}",
                    entry.id,
                    path.display()
                ));
                continue;
            }
        };
        if let Some(expected) = entry.expected_bytes
            && u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected
        {
            result.errors.push(format!(
                "Model '{}' size mismatch: expected {expected} bytes, found {}",
                entry.id,
                bytes.len()
            ));
            continue;
        }
        result.models.total_bytes = result.models.total_bytes.saturating_add(bytes.len());
        result
            .models
            .entries
            .insert(entry.id, Arc::from(bytes.into_boxed_slice()));
    }
    result
}

fn preload_error(message: String) -> ModelPreloadResult {
    ModelPreloadResult {
        errors: vec![message],
        ..Default::default()
    }
}

fn safe_relative_model_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_models(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "maelstrom-models-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn absent_manifest_is_a_ready_empty_registry() {
        let root = temp_models("absent");
        let loaded = preload_models(Some(&root));
        assert_eq!(loaded.models.len(), 0);
        assert!(loaded.errors.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_models_are_retained_by_stable_id() {
        let root = temp_models("valid");
        fs::write(root.join("speech.bin"), [1, 2, 3, 4]).unwrap();
        fs::write(root.join("vision.bin"), [5, 6, 7]).unwrap();
        fs::write(
            root.join(MANIFEST_FILE),
            br#"{"version":1,"models":[{"id":"speech","file":"speech.bin","expected_bytes":4},{"id":"vision","file":"vision.bin","expected_bytes":3}]}"#,
        )
        .unwrap();

        let loaded = preload_models(Some(&root));
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.models.len(), 2);
        assert_eq!(loaded.models.total_bytes(), 7);
        assert_eq!(
            loaded.models.get("speech").as_deref(),
            Some([1, 2, 3, 4].as_slice())
        );
        assert_eq!(
            loaded.models.get("vision").as_deref(),
            Some([5, 6, 7].as_slice())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_duplicate_missing_and_size_mismatch_entries_are_rejected_independently() {
        let root = temp_models("invalid");
        fs::write(root.join("good.bin"), [9, 8]).unwrap();
        fs::write(
            root.join(MANIFEST_FILE),
            br#"{"version":1,"models":[{"id":"good","file":"good.bin","expected_bytes":2},{"id":"good","file":"good.bin"},{"id":"escape","file":"../outside.bin"},{"id":"missing","file":"missing.bin"},{"id":"wrong-size","file":"good.bin","expected_bytes":99}]}"#,
        )
        .unwrap();

        let loaded = preload_models(Some(&root));
        assert_eq!(loaded.models.len(), 1);
        assert_eq!(
            loaded.models.get("good").as_deref(),
            Some([9, 8].as_slice())
        );
        assert_eq!(loaded.errors.len(), 4);
        assert!(
            loaded
                .errors
                .iter()
                .any(|error| error.contains("Duplicate"))
        );
        assert!(
            loaded
                .errors
                .iter()
                .any(|error| error.contains("unsafe path"))
        );
        assert!(
            loaded
                .errors
                .iter()
                .any(|error| error.contains("Could not preload"))
        );
        assert!(
            loaded
                .errors
                .iter()
                .any(|error| error.contains("size mismatch"))
        );
        fs::remove_dir_all(root).unwrap();
    }
}
