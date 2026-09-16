//! The record of a synthetic upload run. `download` replays it, so it holds
//! everything needed to fetch each object and regenerate its expected bytes.
//!
//! It is rewritten after every file completes, so a run interrupted partway
//! through still leaves a manifest covering the files that made it.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sia_storage::Hash256;

mod nanos {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_nanos)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub created_at: DateTime<Utc>,
    /// File `index` holds the bytes of `Data::new(seed, index, file_size)`.
    pub seed: u64,
    pub file_size: u64,
    pub concurrency: usize,
    pub files: Vec<Record>,
}

/// One uploaded file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub index: u32,
    pub object_id: Hash256,
    pub size: u64,
    pub encoded_size: u64,
    #[serde(with = "nanos")]
    pub upload: Duration,
    #[serde(with = "nanos")]
    pub pin: Duration,
}

impl Record {
    /// From the first byte offered to the SDK until the object is durable.
    pub fn elapsed(&self) -> Duration {
        self.upload + self.pin
    }
}

impl Manifest {
    pub fn new(seed: u64, file_size: u64, concurrency: usize) -> Self {
        Self {
            created_at: Utc::now(),
            seed,
            file_size,
            concurrency,
            files: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("invalid manifest {}", path.display()))
    }

    /// Writes the manifest to a sibling temporary file and renames it over
    /// `path`, so an interrupted write never truncates the previous manifest.
    pub fn save(&self, path: &Path) -> Result<()> {
        let temp = path.with_extension("json.tmp");
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&temp, text).with_context(|| format!("failed to write {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("failed to write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(index: u32) -> Record {
        Record {
            index,
            object_id: Hash256::new([index as u8; 32]),
            size: 1 << 20,
            encoded_size: 3 << 20,
            upload: Duration::from_millis(1500),
            pin: Duration::from_millis(250),
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");

        let mut m = Manifest::new(42, 1 << 20, 4);
        m.files.push(record(0));
        m.save(&path).unwrap();
        m.files.push(record(1));
        m.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.seed, 42);
        assert_eq!(loaded.concurrency, 4);
        assert_eq!(loaded.files.len(), 2);
        assert_eq!(loaded.files[1].object_id, record(1).object_id);
        assert_eq!(loaded.files[0].elapsed(), Duration::from_millis(1750));
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temporary file was left behind"
        );
    }

    #[test]
    fn json_shape() {
        let mut m = Manifest::new(1, 1 << 20, 1);
        m.files.push(record(0));
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(v["fileSize"], 1 << 20);
        assert_eq!(v["files"][0]["upload"], 1_500_000_000u64);
        assert_eq!(v["files"][0]["objectId"], hex::encode([0u8; 32]));
    }
}
