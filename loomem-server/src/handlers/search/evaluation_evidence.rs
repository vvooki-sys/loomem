//! Private, create-only receipts. Compiled only into evaluation builds.
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use chrono::SecondsFormat;
use loomem_core::HybridSearchResult;
use serde::Serialize;

use super::operation::PreparedOperation;

pub(super) struct Capture {
    primary: Vec<HybridSearchResult>,
    date: Vec<HybridSearchResult>,
    interval: Vec<u8>,
}

impl Capture {
    pub(super) fn new(
        primary: &[HybridSearchResult],
        prepared: &PreparedOperation,
    ) -> Option<Self> {
        std::env::var_os("LOOMEM_EVALUATION_EVIDENCE_DIR")?;
        Some(Self {
            primary: primary.to_vec(),
            date: Vec::new(),
            interval: interval_json(prepared).unwrap_or_else(|| b"null".to_vec()),
        })
    }

    pub(super) fn date(&mut self, date: &[HybridSearchResult]) {
        self.date = date.to_vec();
    }

    pub(super) fn write(self, output: &[HybridSearchResult]) -> std::io::Result<()> {
        let Some((directory, key)) = settings()? else {
            return Ok(());
        };
        let primary = ids(&self.primary);
        let date = ids(&self.date);
        let output_ids = ids(output);
        let primary_set: HashSet<_> = primary.iter().copied().collect();
        let output_set: HashSet<_> = output_ids.iter().copied().collect();
        let admitted: Vec<_> = output_ids
            .iter()
            .copied()
            .filter(|id| !primary_set.contains(id))
            .collect();
        let removed: Vec<_> = primary
            .iter()
            .copied()
            .filter(|id| !output_set.contains(id))
            .collect();
        let vectors = [
            ("primary_ordered_ids", primary.as_slice()),
            ("primary_prefix20_ids", prefix(&primary)),
            ("date_ordered_ids", date.as_slice()),
            ("date_prefix20_ids", prefix(&date)),
            ("pre_prefix_ids", prefix(&primary)),
            ("post_prefix_ids", prefix(&output_ids)),
            ("output_ordered_ids", output_ids.as_slice()),
            ("admitted_ids", admitted.as_slice()),
            ("removed_ids", removed.as_slice()),
        ];
        let mut proofs = Vec::new();
        for (name, vector) in vectors {
            proofs.push((
                format!("{name}_hmac_sha256"),
                vector_digest(&key, name, vector)?,
            ));
        }
        let domain = if self.interval == b"null" {
            "loomem:e2:private:disabled-interval:v1"
        } else {
            "loomem:e2:private:resolved-interval:v1"
        };
        proofs.push((
            "resolved_interval_hmac_sha256".into(),
            json_digest(&key, domain, &self.interval)?,
        ));
        let receipt = Receipt {
            schema_version: 1,
            proofs: Proofs(proofs),
            interval_applicable: self.interval != b"null",
            primary_prefix_records_preserved: same_records(
                &self.primary[..self.primary.len().min(20)],
                &output[..output.len().min(20)],
            ),
            surviving_primary_records_preserved: surviving_records_equal(&self.primary, output),
            complete_primary_preserved: same_records(&self.primary, output),
        };
        write_new(
            directory.join(format!("{}.json", uuid::Uuid::new_v4())),
            &receipt,
        )
    }
}

#[derive(Serialize)]
struct Receipt {
    schema_version: u8,
    proofs: Proofs,
    interval_applicable: bool,
    primary_prefix_records_preserved: bool,
    surviving_primary_records_preserved: bool,
    complete_primary_preserved: bool,
}
struct Proofs(Vec<(String, String)>);
impl Serialize for Proofs {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, digest) in &self.0 {
            map.serialize_entry(name, digest)?;
        }
        map.end()
    }
}

fn ids(records: &[HybridSearchResult]) -> Vec<&str> {
    records.iter().map(|record| record.id.as_str()).collect()
}
fn prefix<'a>(values: &'a [&'a str]) -> &'a [&'a str] {
    &values[..values.len().min(20)]
}
fn same_record(left: &HybridSearchResult, right: &HybridSearchResult) -> bool {
    let identity = |item: &HybridSearchResult| (item.level, item.timestamp);
    let scores = |item: &HybridSearchResult| {
        (
            item.score.to_bits(),
            item.bm25_score.to_bits(),
            item.vector_score.to_bits(),
            item.time_decay_factor.to_bits(),
        )
    };
    (
        &left.id,
        &left.content,
        &left.user_id,
        &left.app_id,
        identity(left),
        scores(left),
    ) == (
        &right.id,
        &right.content,
        &right.user_id,
        &right.app_id,
        identity(right),
        scores(right),
    )
}
fn same_records(left: &[HybridSearchResult], right: &[HybridSearchResult]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| same_record(a, b))
}
fn surviving_records_equal(primary: &[HybridSearchResult], output: &[HybridSearchResult]) -> bool {
    let surviving: Vec<_> = output
        .iter()
        .filter(|item| primary.iter().any(|original| original.id == item.id))
        .collect();
    let expected: Vec<_> = primary
        .iter()
        .filter(|item| output.iter().any(|result| result.id == item.id))
        .collect();
    expected.len() == surviving.len()
        && expected
            .iter()
            .zip(surviving)
            .all(|(a, b)| same_record(a, b))
}

#[derive(Serialize)]
struct Interval {
    reference_time: String,
    resolved_half_open_start: i64,
    resolved_half_open_end: i64,
    tantivy_inclusive_start: i64,
    tantivy_inclusive_end: i64,
}
fn interval_json(prepared: &PreparedOperation) -> Option<Vec<u8>> {
    let interval = prepared.plan.half_open_interval()?;
    let (start, end) = prepared.e2_lane_spec()?.inclusive_range?;
    serde_json::to_vec(&Interval {
        reference_time: prepared
            .evidence_reference_time()?
            .to_rfc3339_opts(SecondsFormat::Micros, true),
        resolved_half_open_start: interval.start,
        resolved_half_open_end: interval.end,
        tantivy_inclusive_start: start,
        tantivy_inclusive_end: end,
    })
    .ok()
}
fn vector_digest(key: &[u8; 32], name: &str, ids: &[&str]) -> std::io::Result<String> {
    let mut bytes = format!("loomem:e2:private:{name}:v1\0").into_bytes();
    bytes.extend_from_slice(
        &u64::try_from(ids.len())
            .map_err(std::io::Error::other)?
            .to_be_bytes(),
    );
    for id in ids {
        bytes.extend_from_slice(
            &u32::try_from(id.len())
                .map_err(std::io::Error::other)?
                .to_be_bytes(),
        );
        bytes.extend_from_slice(id.as_bytes());
    }
    loomem_core::evaluation_evidence::authenticate(key, &bytes).map_err(std::io::Error::other)
}
fn json_digest(key: &[u8; 32], domain: &str, json: &[u8]) -> std::io::Result<String> {
    let mut bytes = domain.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(
        &u32::try_from(json.len())
            .map_err(std::io::Error::other)?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(json);
    loomem_core::evaluation_evidence::authenticate(key, &bytes).map_err(std::io::Error::other)
}
fn settings() -> std::io::Result<Option<(PathBuf, [u8; 32])>> {
    let Some(directory) = std::env::var_os("LOOMEM_EVALUATION_EVIDENCE_DIR") else {
        return Ok(None);
    };
    validate_directory(std::path::Path::new(&directory))?;
    let encoded = std::env::var("LOOMEM_EVALUATION_HMAC_KEY").map_err(std::io::Error::other)?;
    if encoded.len() != 64 || !encoded.is_ascii() {
        return Err(std::io::Error::other("invalid evaluation key"));
    }
    let mut key = [0; 32];
    for (index, target) in key.iter_mut().enumerate() {
        *target = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16)
            .map_err(|_| std::io::Error::other("invalid evaluation key"))?;
    }
    Ok(Some((PathBuf::from(directory), key)))
}
fn validate_directory(directory: &std::path::Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("invalid private evidence directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::other("evidence directory must be private"));
        }
    }
    Ok(())
}

fn write_new(path: PathBuf, value: &impl Serialize) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vector_framing_matches_python() {
        assert_eq!(
            vector_digest(&[0; 32], "primary_ordered_ids", &["a", "β"]).unwrap(),
            "26c6bc5f72814d94bba619050578f53a5c7919c204e7418f3271144ec01a7129"
        );
    }
    fn record() -> HybridSearchResult {
        HybridSearchResult {
            id: "a".into(),
            content: "record".into(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 1.0,
            bm25_score: 1.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }
    #[test]
    fn record_equality_detects_score_and_content_changes() {
        let original = record();
        let mut changed = original.clone();
        assert!(same_record(&original, &changed));
        changed.score = f64::from_bits(original.score.to_bits() + 1);
        assert!(!same_record(&original, &changed));
        changed = original.clone();
        changed.content.push('x');
        assert!(!same_record(&original, &changed));
        changed = original.clone();
        changed.vector_score = -0.0;
        assert!(!same_record(&original, &changed));
    }
    #[test]
    fn interval_proof_has_a_separate_disabled_domain() {
        let key = [0; 32];
        assert_ne!(
            json_digest(&key, "loomem:e2:private:resolved-interval:v1", b"null").unwrap(),
            json_digest(&key, "loomem:e2:private:disabled-interval:v1", b"null").unwrap()
        );
    }
    #[test]
    fn sink_never_replaces_existing_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("receipt.json");
        write_new(file.clone(), &serde_json::json!({"test":true})).unwrap();
        assert_eq!(
            write_new(file, &serde_json::json!({})).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }
}
