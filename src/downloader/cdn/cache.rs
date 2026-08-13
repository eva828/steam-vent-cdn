use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CdnCache {
    root: PathBuf,
}

impl CdnCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn get_chunk(&self, chunk_id: &str) -> Option<Vec<u8>> {
        self.read(&self.root.join("chunks").join(chunk_id)).await
    }

    pub async fn put_chunk(&self, chunk_id: &str, data: &[u8]) {
        self.write(&self.root.join("chunks").join(chunk_id), data)
            .await;
    }

    pub async fn get_manifest(&self, depot_id: u32, manifest_id: u64) -> Option<Vec<u8>> {
        let name = format!("{depot_id}-{manifest_id}");
        self.read(&self.root.join("manifests").join(name)).await
    }

    pub async fn put_manifest(&self, depot_id: u32, manifest_id: u64, data: &[u8]) {
        let name = format!("{depot_id}-{manifest_id}");
        self.write(&self.root.join("manifests").join(name), data)
            .await;
    }

    async fn read(&self, path: &Path) -> Option<Vec<u8>> {
        match tokio::fs::read(path).await {
            Ok(data) => Some(data),
            Err(e) => {
                tracing::debug!("Cache read failed for {}: {}", path.display(), e);
                None
            }
        }
    }

    async fn write(&self, path: &Path, data: &[u8]) {
        let Some(parent) = path.parent() else {
            return;
        };
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::debug!("Failed to create cache dir {}: {}", parent.display(), e);
            return;
        }
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let tmp = parent.join(format!("{file_name}.{}.tmp", std::process::id()));
        if let Err(e) = tokio::fs::write(&tmp, data).await {
            tracing::debug!("Failed to write cache temp {}: {}", tmp.display(), e);
            let _ = tokio::fs::remove_file(&tmp).await;
            return;
        }
        if let Err(e) = tokio::fs::rename(&tmp, path).await {
            tracing::debug!("Failed to finalize cache {}: {}", tmp.display(), e);
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("steam-vent-cdn-cache-{tag}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn chunk_roundtrip() {
        let cache = CdnCache::new(temp_dir("chunk-roundtrip"));
        assert_eq!(cache.get_chunk("abc123").await, None);

        let data = vec![1u8, 2, 3, 4];
        cache.put_chunk("abc123", &data).await;
        assert_eq!(cache.get_chunk("abc123").await, Some(data));
    }

    #[tokio::test]
    async fn manifest_roundtrip() {
        let cache = CdnCache::new(temp_dir("manifest-roundtrip"));
        assert_eq!(cache.get_manifest(1234, 5678).await, None);

        let data = b"manifest".to_vec();
        cache.put_manifest(1234, 5678, &data).await;
        assert_eq!(cache.get_manifest(1234, 5678).await, Some(data));
    }

    #[test]
    fn root_is_preserved() {
        let root = PathBuf::from("/tmp/cache");
        let cache = CdnCache::new(root.clone());
        assert_eq!(cache.root(), root);
    }
}
