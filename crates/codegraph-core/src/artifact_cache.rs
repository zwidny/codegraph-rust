// ABOUTME: Versioned, content-addressed cache for rebuildable indexing artifacts.
// ABOUTME: Uses atomic replacement and bounded compressed entries; corrupt data is a cache miss.

use serde::{Serialize, de::DeserializeOwned};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ArtifactCache {
    root: PathBuf,
    max_entry_bytes: usize,
}

fn canonicalize(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.sort_keys();
            for value in object.values_mut() {
                canonicalize(value);
            }
        }
        serde_json::Value::Array(array) => {
            for value in array {
                canonicalize(value);
            }
        }
        _ => {}
    }
}

pub fn fingerprint(value: &impl Serialize) -> std::result::Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(value)?;
    canonicalize(&mut value);
    Ok(blake3::hash(&serde_json::to_vec(&value)?)
        .to_hex()
        .to_string())
}

/// Hash of `(text, fixed...)` tuples with the fixed tail encoded once.
///
/// `fingerprint(&(text, a, b, ...))` serialises the whole tuple to a JSON
/// array, recursively sorts every object key and hashes the encoded bytes.
/// When a large batch varies only `text` while `a, b, ...` stay constant, that
/// per-item serialisation and canonicalisation dominates. This type performs
/// the fixed work once and then, for every text, feeds exactly the same byte
/// stream to a fresh hasher:
///
/// ```text
/// [ "escaped text" , <canonical fixed tail> ]
/// ```
///
/// The resulting hash strings are byte-identical to `fingerprint`'s, so cache
/// keys stay valid.
pub struct FingerprintTail {
    tail: Vec<u8>,
}

impl FingerprintTail {
    /// Pre-encode the fixed tuple elements. `fixed` must be a tuple whose
    /// `serde_json` encoding is a non-empty JSON array.
    pub fn new<F: Serialize + ?Sized>(fixed: &F) -> std::result::Result<Self, serde_json::Error> {
        let mut value = serde_json::to_value(fixed)?;
        canonicalize(&mut value);
        match &value {
            serde_json::Value::Array(items) if !items.is_empty() => {}
            _ => {
                return Err(<serde_json::Error as serde::ser::Error>::custom(
                    "fingerprint tail must serialise to a non-empty JSON array",
                ));
            }
        }
        let encoded = serde_json::to_vec(&value)?;
        // `encoded` is `[<tail>]`; keep `<tail>]` so the caller can prepend the
        // separating comma and reuse the closing bracket.
        let tail = encoded
            .split_first()
            .map(|(_, rest)| rest.to_vec())
            .ok_or_else(|| {
                <serde_json::Error as serde::ser::Error>::custom("empty fingerprint encoding")
            })?;
        Ok(Self { tail })
    }

    /// Hash `(text, fixed...)` with the pre-encoded tail.
    pub fn hash(&self, text: &str) -> std::result::Result<String, serde_json::Error> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"[");
        hasher.update(&serde_json::to_vec(text)?);
        hasher.update(b",");
        hasher.update(&self.tail);
        Ok(hasher.finalize().to_hex().to_string())
    }
}

impl ArtifactCache {
    pub fn new(root: impl AsRef<Path>, namespace: &str) -> Self {
        // Namespace never controls a filesystem path: hashing keeps arbitrary callers scoped.
        Self {
            root: root
                .as_ref()
                .join(fingerprint(&namespace).expect("string fingerprint")),
            max_entry_bytes: 64 * 1024 * 1024,
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root
            .join(format!("{}.zst", blake3::hash(key.as_bytes()).to_hex()))
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let result = (|| -> anyhow::Result<T> {
            let file = std::fs::File::open(self.path(key))?;
            if file.metadata()?.len() > self.max_entry_bytes as u64 {
                anyhow::bail!("Oversized cache entry");
            }
            let decoder = zstd::stream::read::Decoder::new(file)?;
            let mut bytes = Vec::new();
            decoder
                .take(self.max_entry_bytes as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > self.max_entry_bytes {
                anyhow::bail!("Oversized decompressed entry");
            }
            Ok(serde_json::from_slice(&bytes)?)
        })();
        result.ok()
    }

    pub fn put(&self, key: &str, value: &impl Serialize) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > self.max_entry_bytes {
            anyhow::bail!("Artifact exceeds cache entry budget");
        }
        let compressed = zstd::stream::encode_all(bytes.as_slice(), 1)?;
        std::fs::create_dir_all(&self.root)?;
        let path = self.path(key);
        let temporary = self.root.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&compressed)?;
            // Rebuildable artifacts need atomic visibility, not an fsync per source file.
            drop(file);
            std::fs::rename(&temporary, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        Ok(result?)
    }

    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Bound all derived namespaces together; pending jobs are durable work, not artifacts.
    pub fn prune_tree(root: &Path, maximum_bytes: u64, protected: &[&str]) -> std::io::Result<()> {
        let excluded: Vec<_> = protected
            .iter()
            .map(|name| Self::new(root, name).root)
            .collect();
        let namespaces = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        let mut total = 0u64;
        for namespace in namespaces {
            let namespace = namespace?;
            if !namespace.file_type()?.is_dir() || excluded.contains(&namespace.path()) {
                continue;
            }
            for entry in std::fs::read_dir(namespace.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file()
                    || entry.path().extension().and_then(|value| value.to_str()) != Some("zst")
                {
                    continue;
                }
                let metadata = entry.metadata()?;
                total = total.saturating_add(metadata.len());
                files.push((metadata.modified()?, entry.path(), metadata.len()));
            }
        }
        files.sort();
        for (_, path, size) in files {
            if total <= maximum_bytes {
                break;
            }
            match std::fs::remove_file(path) {
                Ok(()) => total = total.saturating_sub(size),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Evict oldest artifacts at a run boundary, avoiding directory scans per write.
    pub fn prune(&self, maximum_bytes: u64) -> std::io::Result<()> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in entries {
            let entry = entry?;
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("zst") {
                continue;
            }
            let metadata = entry.metadata()?;
            total = total.saturating_add(metadata.len());
            files.push((metadata.modified()?, entry.path(), metadata.len()));
        }
        files.sort();
        for (_, path, size) in files {
            if total <= maximum_bytes {
                break;
            }
            match std::fs::remove_file(path) {
                Ok(()) => total = total.saturating_sub(size),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_artifacts_are_scoped_and_corruption_is_rebuildable() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path(), "parser-v1");
        cache.put("key", &vec!["source", "data"]).unwrap();
        assert_eq!(
            cache.get::<Vec<String>>("key").unwrap(),
            vec!["source", "data"]
        );
        assert!(
            ArtifactCache::new(directory.path(), "parser-v2")
                .get::<Vec<String>>("key")
                .is_none()
        );
        std::fs::write(cache.path("key"), b"broken").unwrap();
        assert!(cache.get::<Vec<String>>("key").is_none());
        cache.put("key", &vec!["repaired"]).unwrap();
        cache.prune(0).unwrap();
        assert!(cache.get::<Vec<String>>("key").is_none());
    }

    #[test]
    fn aggregate_eviction_preserves_pending_work() {
        let root = tempfile::tempdir().unwrap();
        let artifact = ArtifactCache::new(root.path(), "derived");
        let job = ArtifactCache::new(root.path(), "pending-inference-v1");
        artifact.put("a", &"bytes").unwrap();
        job.put("job", &"resume").unwrap();
        ArtifactCache::prune_tree(root.path(), 0, &["pending-inference-v1"]).unwrap();
        assert!(artifact.get::<String>("a").is_none());
        assert_eq!(job.get::<String>("job").unwrap(), "resume");
    }
    #[test]
    fn fingerprints_include_task_identity_and_ignore_map_insertion_order() {
        let fingerprint = fingerprint(&("parser-v1", "model", "task", 384)).unwrap();
        assert_eq!(fingerprint.len(), 64);
        let a = std::collections::HashMap::from([("a", 1), ("b", 2)]);
        let b = std::collections::HashMap::from([("b", 2), ("a", 1)]);
        assert_eq!(
            super::fingerprint(&a).unwrap(),
            super::fingerprint(&b).unwrap()
        );
        assert_ne!(
            fingerprint,
            super::fingerprint(&("parser-v1", "model", "query", 384)).unwrap()
        );
    }

    #[test]
    fn fingerprint_tail_matches_full_fingerprint_byte_for_byte() {
        let config = serde_json::json!({
            "zeta": 1,
            "alpha": {
                "nested": ["x", {"b": 2, "a": 1, "inner": {"z": true, "a": null}}],
            },
            "vector_size": 384,
            "unicode": "配置 ✓",
        });
        let policy: std::collections::BTreeMap<String, String> = [
            (
                "CODEGRAPH_CHUNK_SPLITTER".to_string(),
                "semantic".to_string(),
            ),
            ("CODEGRAPH_ONNX_EP".to_string(), "cpu".to_string()),
        ]
        .into_iter()
        .collect();
        let model = "text-embedding-3-small".to_string();
        let vector_dim = 384usize;
        let epoch = 12u64;

        let tail = FingerprintTail::new(&(&config, &policy, &model, vector_dim, epoch)).unwrap();
        let texts = [
            "plain ascii chunk text",
            "quotes \" and backslash \\ and newline \n and tab \t",
            "unicode 你好 🚀 and escaped \\u{1F600}",
            "",
            "line\r\nbreak \u{0} control",
        ];
        for text in texts {
            assert_eq!(
                tail.hash(text).unwrap(),
                fingerprint(&(text, &config, &policy, &model, vector_dim, epoch)).unwrap(),
                "text: {text:?}"
            );
        }
    }

    #[test]
    fn fingerprint_tail_canonicalises_nested_object_keys() {
        fn fixed(reverse: bool) -> serde_json::Value {
            let mut inner = serde_json::Map::new();
            let mut outer = serde_json::Map::new();
            let entries = [("b", 2), ("a", 1), ("c", 3)];
            let mut ordered: Vec<_> = entries.to_vec();
            if reverse {
                ordered.reverse();
            }
            for (key, value) in ordered {
                inner.insert(key.to_string(), serde_json::json!(value));
            }
            outer.insert("nested".to_string(), serde_json::Value::Object(inner));
            outer.insert("model".to_string(), serde_json::json!("m"));
            serde_json::Value::Object(outer)
        }

        let a = FingerprintTail::new(&(&fixed(true), 8usize, 0u64)).unwrap();
        let b = FingerprintTail::new(&(&fixed(false), 8usize, 0u64)).unwrap();
        assert_eq!(a.hash("chunk").unwrap(), b.hash("chunk").unwrap());
    }
}
