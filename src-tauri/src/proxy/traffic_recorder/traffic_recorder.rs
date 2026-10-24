//! Opt-in, device-local HTTP body recording. Bodies never enter diagnostic logs.

use crate::error::AppError;
use bytes::Bytes;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct TrafficRecordingConfig {
    pub enabled: bool,
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CaptureStage {
    ClientRequest,
    UpstreamRequest,
    UpstreamResponse,
    ClientResponse,
}

#[derive(Clone)]
pub(crate) struct CaptureMetadata {
    pub request_id: String,
    pub started_at: DateTime<Local>,
    pub attempt: Option<u32>,
    pub stage: CaptureStage,
    pub app_type: Option<String>,
    pub provider_id: Option<String>,
    pub method: Option<String>,
    pub url: Option<String>,
    pub status: Option<u16>,
    pub headers: http::HeaderMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum CaptureOutcome {
    Complete,
    Interrupted { reason: String },
    ReadFailed { reason: String },
    RecordingFailed { reason: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrafficExport {
    pub path: PathBuf,
    pub request_count: u64,
    pub incomplete_count: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Part {
    stage: CaptureStage,
    attempt: Option<u32>,
    file: String,
    method: Option<String>,
    url: Option<String>,
    status: Option<u16>,
    provider_id: Option<String>,
    headers: BTreeMap<String, Vec<String>>,
    bytes: u64,
    sha256: String,
    outcome: CaptureOutcome,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    request_id: String,
    started_at: DateTime<Local>,
    app_type: Option<String>,
    parts: BTreeMap<String, Part>,
    #[serde(default)]
    unobserved: Vec<String>,
}

struct ActiveCapture {
    file: File,
    request_dir: PathBuf,
    key: String,
    part: Part,
    hash: Sha256,
}

struct State {
    config: TrafficRecordingConfig,
    closed: bool,
    active: HashMap<Uuid, ActiveCapture>,
}

pub(crate) struct TrafficRecorder {
    state: Arc<Mutex<State>>,
    enabled: AtomicBool,
}

pub(crate) struct TrafficCapture {
    state: Arc<Mutex<State>>,
    id: Option<Uuid>,
}

impl TrafficRecorder {
    pub(crate) async fn open(config: TrafficRecordingConfig) -> Result<Self, AppError> {
        let config = tokio::task::spawn_blocking(move || resolve_config(config))
            .await
            .map_err(join_error)??;
        Ok(Self {
            enabled: AtomicBool::new(config.enabled),
            state: Arc::new(Mutex::new(State {
                config,
                closed: false,
                active: HashMap::new(),
            })),
        })
    }

    pub(crate) async fn reconfigure(&self, config: TrafficRecordingConfig) -> Result<(), AppError> {
        let enabled = config.enabled;
        let state = self.state.clone();
        let result = tokio::task::spawn_blocking(move || {
            let config = resolve_config(config)?;
            let mut state = state.lock()?;
            if state.closed {
                return Err(AppError::Message("Traffic recorder is closed".into()));
            }
            state.config = config;
            Ok(())
        })
        .await
        .map_err(join_error)?;
        if result.is_ok() {
            self.enabled.store(enabled, Ordering::Release);
        }
        result
    }

    pub(crate) async fn begin(
        &self,
        meta: CaptureMetadata,
    ) -> Result<Option<TrafficCapture>, AppError> {
        if !self.enabled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let state = self.state.clone();
        let owner = state.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock()?;
            if state.closed || !state.config.enabled {
                return Ok(None);
            }
            if Uuid::parse_str(&meta.request_id).is_err()
                || meta.request_id != Uuid::parse_str(&meta.request_id).unwrap().to_string()
            {
                return Err(AppError::InvalidInput(
                    "Invalid recording request ID".into(),
                ));
            }
            let upstream = matches!(
                meta.stage,
                CaptureStage::UpstreamRequest | CaptureStage::UpstreamResponse
            );
            if upstream != meta.attempt.is_some() || meta.attempt == Some(0) {
                return Err(AppError::InvalidInput("Invalid recording attempt".into()));
            }
            let root = state.config.directory.as_ref().expect("resolved directory");
            let day = root.join(meta.started_at.format("%Y-%m-%d").to_string());
            private_dir(&day)?;
            let request_dir = day.join(&meta.request_id);
            private_dir(&request_dir)?;
            let mut manifest = load_manifest(&request_dir)?.unwrap_or_else(|| Manifest {
                schema_version: 1,
                request_id: meta.request_id.clone(),
                started_at: meta.started_at,
                app_type: meta.app_type.clone(),
                parts: BTreeMap::new(),
                unobserved: vec![],
            });
            let direction = if matches!(
                meta.stage,
                CaptureStage::ClientRequest | CaptureStage::UpstreamRequest
            ) {
                "request"
            } else {
                "response"
            };
            let group = meta
                .attempt
                .map(|n| format!("attempts/{n:06}"))
                .unwrap_or_else(|| "client".into());
            let key = format!("{group}/{direction}");
            if manifest.parts.contains_key(&key) {
                return Err(AppError::Conflict(
                    "Recording boundary already exists".into(),
                ));
            }
            let is_sse = meta
                .headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                });
            let encoded = meta
                .headers
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| !v.is_empty() && !v.eq_ignore_ascii_case("identity"));
            let extension = if is_sse && !encoded { "sse" } else { "body" };
            let relative = format!("{key}.{extension}");
            let path = request_dir.join(&relative);
            if upstream {
                private_dir(&request_dir.join("attempts"))?;
            }
            private_dir(path.parent().expect("body parent"))?;
            let file = private_file(&path)?;
            let part = Part {
                stage: meta.stage,
                attempt: meta.attempt,
                file: relative,
                method: meta.method,
                url: meta.url.map(|v| redact_url(&v, &meta.headers)),
                status: meta.status,
                provider_id: meta.provider_id,
                headers: redact_headers(&meta.headers),
                bytes: 0,
                sha256: format!("{:x}", Sha256::digest([])),
                outcome: CaptureOutcome::Interrupted {
                    reason: "Recording in progress".into(),
                },
            };
            if manifest.app_type.is_none() {
                manifest.app_type = meta.app_type;
            }
            manifest.parts.insert(key.clone(), part.clone());
            save_manifest(&request_dir, &manifest)?;
            let id = Uuid::new_v4();
            state.active.insert(
                id,
                ActiveCapture {
                    file,
                    request_dir,
                    key,
                    part,
                    hash: Sha256::new(),
                },
            );
            Ok(Some(TrafficCapture {
                state: owner,
                id: Some(id),
            }))
        })
        .await
        .map_err(join_error)?
    }

    pub(crate) async fn export(&self, target: PathBuf) -> Result<TrafficExport, AppError> {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let snapshots = {
                let state = state.lock()?;
                snapshot(&state, &target)?
            };
            export_snapshot(target, snapshots)
        })
        .await
        .map_err(join_error)?
    }

    pub(crate) async fn shutdown(&self) -> Result<(), AppError> {
        self.enabled.store(false, Ordering::Release);
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock()?;
            state.closed = true;
            let ids: Vec<_> = state.active.keys().copied().collect();
            let mut error = None;
            for id in ids {
                if let Err(e) = finish_capture(
                    &mut state,
                    id,
                    CaptureOutcome::Interrupted {
                        reason: "Application shutdown".into(),
                    },
                ) {
                    error = Some(e);
                }
            }
            error.map_or(Ok(()), Err)
        })
        .await
        .map_err(join_error)?
    }
}

impl TrafficCapture {
    pub(crate) async fn write(&mut self, bytes: Bytes) -> Result<(), AppError> {
        let state = self.state.clone();
        let id = self.id.expect("capture is owned");
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock()?;
            let capture = state
                .active
                .get_mut(&id)
                .ok_or_else(|| AppError::Message("Traffic capture is closed".into()))?;
            if let Err(e) = capture.file.write_all(&bytes) {
                let reason = e.to_string();
                let _ = finish_capture(&mut state, id, CaptureOutcome::RecordingFailed { reason });
                return Err(AppError::Message(
                    "Could not write traffic recording".into(),
                ));
            }
            capture.hash.update(&bytes);
            capture.part.bytes += bytes.len() as u64;
            Ok(())
        })
        .await
        .map_err(join_error)?
    }

    pub(crate) async fn finish(mut self, outcome: CaptureOutcome) -> Result<(), AppError> {
        let state = self.state.clone();
        let id = self.id.take().expect("capture is owned");
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock()?;
            finish_capture(&mut state, id, outcome)
        })
        .await
        .map_err(join_error)?
    }
}

impl Drop for TrafficCapture {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return };
        let state = self.state.clone();
        let finish = move || {
            if let Ok(mut state) = state.lock() {
                if let Err(e) = finish_capture(
                    &mut state,
                    id,
                    CaptureOutcome::Interrupted {
                        reason: "Body was dropped before end of stream".into(),
                    },
                ) {
                    log::warn!("Traffic recording could not finalize: {e}");
                }
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(finish);
        } else {
            finish();
        }
    }
}

fn join_error(e: tokio::task::JoinError) -> AppError {
    AppError::Message(format!("Traffic recording task failed: {e}"))
}

fn resolve_config(mut config: TrafficRecordingConfig) -> Result<TrafficRecordingConfig, AppError> {
    let path = config
        .directory
        .take()
        .unwrap_or_else(|| crate::config::get_app_config_dir().join("raw-traffic"));
    if !path.is_absolute() {
        return Err(AppError::InvalidInput(
            "Recording directory must be absolute".into(),
        ));
    }
    if config.enabled {
        private_dir(&path)?;
        let probe = tempfile::NamedTempFile::new_in(&path).map_err(|e| AppError::io(&path, e))?;
        drop(probe);
        config.directory = Some(fs::canonicalize(&path).map_err(|e| AppError::io(&path, e))?);
    } else {
        config.directory = Some(path);
    }
    Ok(config)
}

fn private_dir(path: &Path) -> Result<(), AppError> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(AppError::InvalidInput(
                "Recording directory is not a regular directory".into(),
            ));
        }
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| AppError::io(path, e))
}

fn private_file(path: &Path) -> Result<File, AppError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|e| AppError::io(path, e))
}

fn load_manifest(directory: &Path) -> Result<Option<Manifest>, AppError> {
    let path = directory.join("manifest.json");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::io(&path, e)),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err(AppError::InvalidInput("Unsafe recording manifest".into()))
        }
        Ok(_) => {
            let data = fs::read(&path).map_err(|e| AppError::io(&path, e))?;
            serde_json::from_slice(&data)
                .map(Some)
                .map_err(|e| AppError::json(&path, e))
        }
    }
}

fn save_manifest(directory: &Path, manifest: &Manifest) -> Result<(), AppError> {
    let mut temp =
        tempfile::NamedTempFile::new_in(directory).map_err(|e| AppError::io(directory, e))?;
    serde_json::to_writer_pretty(&mut temp, manifest)
        .map_err(|source| AppError::JsonSerialize { source })?;
    temp.as_file()
        .sync_all()
        .map_err(|e| AppError::io(directory, e))?;
    let path = directory.join("manifest.json");
    temp.persist(&path)
        .map_err(|e| AppError::io(&path, e.error))?;
    Ok(())
}

fn finish_capture(state: &mut State, id: Uuid, outcome: CaptureOutcome) -> Result<(), AppError> {
    let Some(mut capture) = state.active.remove(&id) else {
        return Ok(());
    };
    let sync = capture.file.sync_all();
    capture.part.outcome = match &sync {
        Ok(()) => outcome,
        Err(e) => CaptureOutcome::RecordingFailed {
            reason: e.to_string(),
        },
    };
    capture.part.bytes = capture
        .file
        .metadata()
        .map_err(|e| AppError::io(&capture.request_dir, e))?
        .len();
    capture.part.sha256 = if matches!(capture.part.outcome, CaptureOutcome::RecordingFailed { .. })
    {
        hash_file(
            &capture.request_dir.join(&capture.part.file),
            capture.part.bytes,
        )?
    } else {
        format!("{:x}", capture.hash.finalize())
    };
    let mut manifest = load_manifest(&capture.request_dir)?
        .ok_or_else(|| AppError::Message("Missing recording manifest".into()))?;
    manifest.parts.insert(capture.key, capture.part);
    save_manifest(&capture.request_dir, &manifest)?;
    sync.map_err(|e| AppError::io(&capture.request_dir, e))
}

fn sensitive_header(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) || name.ends_with("-key")
        || name.ends_with("-token")
}

fn redact_headers(headers: &http::HeaderMap) -> BTreeMap<String, Vec<String>> {
    headers
        .keys()
        .map(|name| {
            let values = if sensitive_header(name.as_str()) {
                vec!["[redacted]".into()]
            } else {
                headers
                    .get_all(name)
                    .iter()
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                    .collect()
            };
            (name.to_string(), values)
        })
        .collect()
}

fn redact_url(value: &str, headers: &http::HeaderMap) -> String {
    let mut value = if let Ok(mut url) = url::Url::parse(value) {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        if url.query().is_some() {
            url.set_query(Some("redacted"));
        }
        url.set_fragment(None);
        url.to_string()
    } else {
        value.split(['?', '#']).next().unwrap_or("").to_string()
    };
    for (name, secret) in headers {
        if sensitive_header(name.as_str()) {
            if let Ok(secret) = secret.to_str() {
                let secret = secret.strip_prefix("Bearer ").unwrap_or(secret);
                if !secret.is_empty() {
                    value = value.replace(secret, "[redacted]");
                }
            }
        }
    }
    value
}

fn checked_body_path(directory: &Path, relative: &str) -> Result<PathBuf, AppError> {
    let path = Path::new(relative);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
        || relative.is_empty()
        || relative.contains('\\')
    {
        return Err(AppError::InvalidInput("Unsafe recording body path".into()));
    }
    let mut current = directory.to_path_buf();
    for component in path.components() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|e| AppError::io(&current, e))?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::InvalidInput(
                "Recording export refuses symbolic links".into(),
            ));
        }
    }
    Ok(current)
}

struct Snapshot {
    prefix: String,
    directory: PathBuf,
    manifest: Manifest,
}

fn snapshot(state: &State, target: &Path) -> Result<Vec<Snapshot>, AppError> {
    let root = state.config.directory.as_ref().expect("resolved directory");
    if !target.is_absolute() || target.exists() {
        return Err(AppError::InvalidInput(
            "Export requires a new absolute file path".into(),
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| AppError::InvalidInput("Invalid export path".into()))?;
    let parent = fs::canonicalize(parent).map_err(|e| AppError::io(parent, e))?;
    let normalized_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
    if parent.starts_with(&normalized_root) {
        return Err(AppError::InvalidInput(
            "Export must be outside the recording directory".into(),
        ));
    }
    if !root.exists() {
        return Ok(vec![]);
    }
    if fs::symlink_metadata(root)
        .map_err(|e| AppError::io(root, e))?
        .file_type()
        .is_symlink()
    {
        return Err(AppError::InvalidInput(
            "Recording export refuses symbolic links".into(),
        ));
    }

    let mut manifests = Vec::new();
    find_manifests(root, &mut manifests)?;
    manifests.sort();
    let mut result = vec![];
    for directory in manifests {
        let mut manifest = load_manifest(&directory)?
            .ok_or_else(|| AppError::Message("Missing recording manifest".into()))?;
        if manifest.schema_version != 1 {
            return Err(AppError::InvalidInput(
                "Unsupported recording schema".into(),
            ));
        }
        for part in manifest.parts.values_mut() {
            let path = checked_body_path(&directory, &part.file)?;
            let bytes = fs::metadata(&path)
                .map_err(|e| AppError::io(&path, e))?
                .len();
            let sha = hash_file(&path, bytes)?;
            if part.outcome == CaptureOutcome::Complete
                && (part.bytes != bytes || part.sha256 != sha)
            {
                return Err(AppError::Conflict(
                    "Recorded body changed after capture".into(),
                ));
            }
            part.bytes = bytes;
            part.sha256 = sha;
        }
        manifest.unobserved.clear();
        for key in ["client/request", "client/response"] {
            if !manifest.parts.contains_key(key) {
                manifest.unobserved.push(key.into());
            }
        }
        let attempts: Vec<_> = manifest.parts.values().filter_map(|p| p.attempt).collect();
        for attempt in attempts {
            let key = format!("attempts/{attempt:06}/response");
            if !manifest.parts.contains_key(&key) && !manifest.unobserved.contains(&key) {
                manifest.unobserved.push(key);
            }
        }
        let prefix = directory
            .strip_prefix(root)
            .map_err(|e| AppError::Message(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let expected_prefix = format!(
            "{}/{}",
            manifest.started_at.format("%Y-%m-%d"),
            manifest.request_id
        );
        if prefix != expected_prefix
            || Uuid::parse_str(&manifest.request_id)
                .map(|id| id.to_string())
                .ok()
                .as_deref()
                != Some(manifest.request_id.as_str())
        {
            return Err(AppError::InvalidInput("Unsafe recording directory".into()));
        }
        result.push(Snapshot {
            prefix,
            directory,
            manifest,
        });
    }
    Ok(result)
}

fn find_manifests(directory: &Path, result: &mut Vec<PathBuf>) -> Result<(), AppError> {
    for entry in fs::read_dir(directory).map_err(|e| AppError::io(directory, e))? {
        let entry = entry.map_err(|e| AppError::io(directory, e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| AppError::io(entry.path(), e))?;
        if file_type.is_symlink() {
            return Err(AppError::InvalidInput(
                "Recording export refuses symbolic links".into(),
            ));
        }
        if file_type.is_dir() {
            find_manifests(&entry.path(), result)?;
        } else if entry.file_name() == "manifest.json" {
            result.push(directory.to_path_buf());
        }
    }
    Ok(())
}

fn hash_file(path: &Path, length: u64) -> Result<String, AppError> {
    let mut file = File::open(path)
        .map_err(|e| AppError::io(path, e))?
        .take(length);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| AppError::io(path, e))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn export_snapshot(target: PathBuf, snapshots: Vec<Snapshot>) -> Result<TrafficExport, AppError> {
    let parent = target.parent().expect("validated export parent");
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| AppError::io(parent, e))?;
    let mut zip = zip::ZipWriter::new(temp.as_file_mut());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut index = String::new();
    let mut incomplete_count = 0;
    for snapshot in &snapshots {
        let manifest = &snapshot.manifest;
        let complete = manifest.unobserved.is_empty()
            && manifest
                .parts
                .values()
                .all(|p| p.outcome == CaptureOutcome::Complete);
        if !complete {
            incomplete_count += 1;
        }
        index.push_str(&serde_json::json!({"schemaVersion":1,"requestId":manifest.request_id,"startedAt":manifest.started_at,"appType":manifest.app_type,"directory":snapshot.prefix,"complete":complete}).to_string());
        index.push('\n');
        for part in manifest.parts.values() {
            let path = checked_body_path(&snapshot.directory, &part.file)?;
            zip.start_file(format!("{}/{}", snapshot.prefix, part.file), options)
                .map_err(zip_error)?;
            let mut file = File::open(&path)
                .map_err(|e| AppError::io(&path, e))?
                .take(part.bytes);
            let mut buffer = [0u8; 65536];
            let mut hash = Sha256::new();
            let mut copied = 0;
            loop {
                let n = file.read(&mut buffer).map_err(|e| AppError::io(&path, e))?;
                if n == 0 {
                    break;
                }
                zip.write_all(&buffer[..n])
                    .map_err(|e| AppError::io(&target, e))?;
                hash.update(&buffer[..n]);
                copied += n as u64;
            }
            if copied != part.bytes || format!("{:x}", hash.finalize()) != part.sha256 {
                return Err(AppError::Conflict("Recording changed during export".into()));
            }
        }
        zip.start_file(format!("{}/manifest.json", snapshot.prefix), options)
            .map_err(zip_error)?;
        serde_json::to_writer_pretty(&mut zip, manifest)
            .map_err(|source| AppError::JsonSerialize { source })?;
    }
    zip.start_file("index.jsonl", options).map_err(zip_error)?;
    zip.write_all(index.as_bytes())
        .map_err(|e| AppError::io(&target, e))?;
    zip.finish().map_err(zip_error)?;
    temp.as_file()
        .sync_all()
        .map_err(|e| AppError::io(&target, e))?;
    temp.persist_noclobber(&target)
        .map_err(|e| AppError::io(&target, e.error))?;
    Ok(TrafficExport {
        path: target,
        request_count: snapshots.len() as u64,
        incomplete_count,
    })
}

fn zip_error(e: zip::result::ZipError) -> AppError {
    AppError::Message(format!("Traffic recording ZIP failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn config(path: &Path, enabled: bool) -> TrafficRecordingConfig {
        TrafficRecordingConfig {
            enabled,
            directory: Some(path.into()),
        }
    }
    fn metadata(stage: CaptureStage, attempt: Option<u32>) -> CaptureMetadata {
        CaptureMetadata {
            request_id: Uuid::new_v4().to_string(),
            started_at: Local::now(),
            stage,
            attempt,
            app_type: Some("codex".into()),
            provider_id: Some("fixture".into()),
            method: Some("POST".into()),
            url: Some("https://user:password@example.com/v1?key=secret#token".into()),
            status: Some(200),
            headers: http::HeaderMap::new(),
        }
    }
    fn directory(root: &Path, meta: &CaptureMetadata) -> PathBuf {
        root.join(meta.started_at.format("%Y-%m-%d").to_string())
            .join(&meta.request_id)
    }
    async fn record(recorder: &TrafficRecorder, meta: CaptureMetadata, bytes: &[u8]) {
        let mut capture = recorder.begin(meta).await.unwrap().unwrap();
        capture.write(Bytes::copy_from_slice(bytes)).await.unwrap();
        capture.finish(CaptureOutcome::Complete).await.unwrap();
    }
    fn zip_bytes(zip: &mut zip::ZipArchive<File>, name: &str) -> Vec<u8> {
        let mut bytes = vec![];
        zip.by_name(name).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[tokio::test]
    async fn disabled_is_no_io_and_invalid_configuration_is_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("not-created");
        let recorder = TrafficRecorder::open(config(&root, false)).await.unwrap();
        assert!(recorder
            .begin(metadata(CaptureStage::ClientRequest, None))
            .await
            .unwrap()
            .is_none());
        assert!(!root.exists());
        let export = recorder
            .export(temp.path().join("empty.zip"))
            .await
            .unwrap();
        assert_eq!(export.request_count, 0);
        assert!(recorder
            .reconfigure(config(Path::new("relative"), true))
            .await
            .is_err());
        assert!(!recorder.enabled.load(Ordering::Acquire));
        recorder.reconfigure(config(&root, true)).await.unwrap();
        let blocker = temp.path().join("file");
        fs::write(&blocker, b"x").unwrap();
        assert!(recorder.reconfigure(config(&blocker, true)).await.is_err());
        record(
            &recorder,
            metadata(CaptureStage::ClientRequest, None),
            b"still enabled",
        )
        .await;
        recorder.shutdown().await.unwrap();
        assert!(recorder.reconfigure(config(&root, true)).await.is_err());
        assert!(recorder
            .begin(metadata(CaptureStage::ClientRequest, None))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn four_boundaries_export_exact_bytes_and_redact_metadata_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let recorder = TrafficRecorder::open(config(&root, true)).await.unwrap();
        let mut meta = metadata(CaptureStage::ClientRequest, None);
        meta.headers
            .insert("authorization", "Bearer secret".parse().unwrap());
        meta.headers.insert("x-api-key", "secret".parse().unwrap());
        meta.headers
            .insert("cookie", "session=secret".parse().unwrap());
        meta.headers.append("x-custom", "one".parse().unwrap());
        meta.headers.append("x-custom", "two".parse().unwrap());
        let request = b" { \"model\": \"fixture\", \"prompt\": \"secret\" } \n";
        record(&recorder, meta.clone(), request).await;
        meta.stage = CaptureStage::UpstreamRequest;
        meta.attempt = Some(1);
        record(&recorder, meta.clone(), b"{\"changed\":true}").await;
        let sse = ": heartbeat\r\nevent: message\r\ndata: {\"text\":\"中文\"}\r\n\r\ndata: [DONE]\n\ntail".as_bytes();
        for stage in [CaptureStage::UpstreamResponse, CaptureStage::ClientResponse] {
            meta.stage = stage;
            meta.attempt = if stage == CaptureStage::UpstreamResponse {
                Some(1)
            } else {
                None
            };
            meta.headers.insert(
                "content-type",
                "text/event-stream; charset=utf-8".parse().unwrap(),
            );
            let mut cap = recorder.begin(meta.clone()).await.unwrap().unwrap();
            // Split in the middle of UTF-8 and CRLF, not only at event boundaries.
            for byte in sse {
                cap.write(Bytes::copy_from_slice(&[*byte])).await.unwrap();
            }
            cap.finish(CaptureOutcome::Complete).await.unwrap();
        }
        let dir = directory(&root, &meta);
        let manifest = load_manifest(&dir).unwrap().unwrap();
        let part = &manifest.parts["client/request"];
        assert_eq!(part.headers["authorization"], ["[redacted]"]);
        assert_eq!(part.headers["x-custom"], ["one", "two"]);
        assert!(!part.url.as_ref().unwrap().contains("secret"));
        assert!(!part.url.as_ref().unwrap().contains("password"));
        assert_eq!(fs::read(dir.join(&part.file)).unwrap(), request);
        let export = recorder
            .export(temp.path().join("export.zip"))
            .await
            .unwrap();
        assert_eq!((export.request_count, export.incomplete_count), (1, 0));
        let mut zip = zip::ZipArchive::new(File::open(&export.path).unwrap()).unwrap();
        let index: serde_json::Value =
            serde_json::from_slice(&zip_bytes(&mut zip, "index.jsonl")).unwrap();
        assert_eq!(index["complete"], true);
        let prefix = index["directory"].as_str().unwrap();
        assert_eq!(
            zip_bytes(&mut zip, &format!("{prefix}/client/request.body")),
            request
        );
        assert_eq!(
            zip_bytes(&mut zip, &format!("{prefix}/client/response.sse")),
            sse
        );
        assert_eq!(
            zip_bytes(&mut zip, &format!("{prefix}/attempts/000001/response.sse")),
            sse
        );
        for part in manifest.parts.values() {
            let bytes = zip_bytes(&mut zip, &format!("{prefix}/{}", part.file));
            assert_eq!(part.bytes, bytes.len() as u64);
            assert_eq!(part.sha256, format!("{:x}", Sha256::digest(&bytes)));
        }
        assert!(recorder.export(export.path).await.is_err());
        assert!(recorder.export(root.join("inside.zip")).await.is_err());
        assert!(recorder.export("relative.zip".into()).await.is_err());
        fs::write(dir.join("client/request.body"), b"tampered").unwrap();
        assert!(recorder
            .export(temp.path().join("tampered.zip"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn binary_large_bodies_and_retries_are_not_truncated() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let recorder = TrafficRecorder::open(config(&root, true)).await.unwrap();
        let mut meta = metadata(CaptureStage::UpstreamResponse, Some(1));
        meta.headers
            .insert("content-type", "text/event-stream".parse().unwrap());
        meta.headers
            .insert("content-encoding", "gzip".parse().unwrap());
        let gzip = b"\x1f\x8b\x08\x00\xff\x00binary";
        record(&recorder, meta.clone(), gzip).await;
        assert!(recorder.begin(meta.clone()).await.is_err());
        meta.attempt = Some(2);
        meta.status = Some(500);
        let mut cap = recorder.begin(meta.clone()).await.unwrap().unwrap();
        let chunk = Bytes::from(vec![0xa5; 1024 * 1024]);
        let mut expected = Sha256::new();
        for _ in 0..51 {
            cap.write(chunk.clone()).await.unwrap();
            expected.update(&chunk);
        }
        cap.finish(CaptureOutcome::Complete).await.unwrap();
        let dir = directory(&root, &meta);
        let manifest = load_manifest(&dir).unwrap().unwrap();
        assert_eq!(
            fs::read(dir.join("attempts/000001/response.body")).unwrap(),
            gzip
        );
        let part = &manifest.parts["attempts/000002/response"];
        assert_eq!(part.bytes, 51 * 1024 * 1024);
        assert_eq!(part.sha256, format!("{:x}", expected.finalize()));
        assert_eq!(part.status, Some(500));
        for (stage, attempt) in [
            (CaptureStage::ClientRequest, Some(1)),
            (CaptureStage::UpstreamRequest, None),
            (CaptureStage::UpstreamRequest, Some(0)),
        ] {
            let mut bad = meta.clone();
            bad.stage = stage;
            bad.attempt = attempt;
            assert!(recorder.begin(bad).await.is_err());
        }
        meta.request_id = "../escape".into();
        assert!(recorder.begin(meta).await.is_err());
    }

    #[tokio::test]
    async fn live_export_is_a_consistent_incomplete_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let recorder = TrafficRecorder::open(config(&root, true)).await.unwrap();
        let meta = metadata(CaptureStage::ClientResponse, None);
        let mut cap = recorder.begin(meta.clone()).await.unwrap().unwrap();
        cap.write(Bytes::from_static(b"prefix")).await.unwrap();
        let snapshots = snapshot(
            &recorder.state.lock().unwrap(),
            &temp.path().join("live.zip"),
        )
        .unwrap();
        cap.write(Bytes::from_static(b"suffix")).await.unwrap();
        let export = export_snapshot(temp.path().join("live.zip"), snapshots).unwrap();
        assert_eq!(export.incomplete_count, 1);
        let mut zip = zip::ZipArchive::new(File::open(export.path).unwrap()).unwrap();
        let prefix = directory(&root, &meta)
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(
            zip_bytes(&mut zip, &format!("{prefix}/client/response.body")),
            b"prefix"
        );
        cap.finish(CaptureOutcome::ReadFailed {
            reason: "upstream reset".into(),
        })
        .await
        .unwrap();
        assert!(matches!(
            load_manifest(&directory(&root, &meta))
                .unwrap()
                .unwrap()
                .parts["client/response"]
                .outcome,
            CaptureOutcome::ReadFailed { .. }
        ));
    }

    #[tokio::test]
    async fn lifecycle_keeps_active_files_and_marks_shutdown_or_drop_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("first");
        let second = temp.path().join("second");
        let recorder = TrafficRecorder::open(config(&root, true)).await.unwrap();
        let mut meta = metadata(CaptureStage::ClientRequest, None);
        meta.started_at -= Duration::days(1);
        let mut cap = recorder.begin(meta.clone()).await.unwrap().unwrap();
        recorder.reconfigure(config(&second, true)).await.unwrap();
        cap.write(Bytes::from_static(b"old root")).await.unwrap();
        cap.finish(CaptureOutcome::Complete).await.unwrap();
        assert_eq!(
            fs::read(directory(&root, &meta).join("client/request.body")).unwrap(),
            b"old root"
        );
        meta.started_at += Duration::days(1);
        let mut cap = recorder.begin(meta.clone()).await.unwrap().unwrap();
        cap.write(Bytes::from_static(b"partial")).await.unwrap();
        recorder.shutdown().await.unwrap();
        assert!(cap.write(Bytes::from_static(b"too late")).await.is_err());
        assert!(matches!(
            load_manifest(&directory(&second, &meta))
                .unwrap()
                .unwrap()
                .parts["client/request"]
                .outcome,
            CaptureOutcome::Interrupted { .. }
        ));
        drop(cap);
        let recorder = TrafficRecorder::open(config(&second, true)).await.unwrap();
        meta.request_id = Uuid::new_v4().to_string();
        let cap = recorder.begin(meta.clone()).await.unwrap().unwrap();
        drop(cap);
        // Synchronize all pending captures; shutdown and Drop are both idempotent.
        recorder.shutdown().await.unwrap();
        let export = recorder
            .export(temp.path().join("interrupted.zip"))
            .await
            .unwrap();
        assert_eq!((export.request_count, export.incomplete_count), (2, 2));
    }

    #[tokio::test]
    async fn write_failure_is_recorded_and_not_mistaken_for_complete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let recorder = TrafficRecorder::open(config(&root, true)).await.unwrap();
        let meta = metadata(CaptureStage::ClientRequest, None);
        let mut cap = recorder.begin(meta.clone()).await.unwrap().unwrap();
        cap.write(Bytes::from_static(b"prefix")).await.unwrap();
        {
            let mut state = recorder.state.lock().unwrap();
            let active = state.active.get_mut(&cap.id.unwrap()).unwrap();
            // A read-only descriptor deterministically fails write_all on every OS.
            active.file = File::open(directory(&root, &meta).join("client/request.body")).unwrap();
        }
        assert!(cap.write(Bytes::from_static(b"lost")).await.is_err());
        cap.finish(CaptureOutcome::Complete).await.unwrap();
        let manifest = load_manifest(&directory(&root, &meta)).unwrap().unwrap();
        let part = &manifest.parts["client/request"];
        assert!(matches!(
            part.outcome,
            CaptureOutcome::RecordingFailed { .. }
        ));
        assert_eq!(part.bytes, 6);
        assert_eq!(part.sha256, format!("{:x}", Sha256::digest(b"prefix")));
        let result = recorder
            .export(temp.path().join("failure.zip"))
            .await
            .unwrap();
        assert_eq!(result.incomplete_count, 1);
    }

    #[tokio::test]
    async fn export_rejects_unsafe_paths_and_schema() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("recordings");
        let recorder = TrafficRecorder::open(config(&root, true)).await.unwrap();
        let meta = metadata(CaptureStage::ClientRequest, None);
        record(&recorder, meta.clone(), b"body").await;
        let dir = directory(&root, &meta);
        let mut manifest = load_manifest(&dir).unwrap().unwrap();
        manifest.parts.get_mut("client/request").unwrap().file = "../../escape".into();
        save_manifest(&dir, &manifest).unwrap();
        assert!(recorder
            .export(temp.path().join("unsafe.zip"))
            .await
            .is_err());
        manifest.schema_version = 999;
        save_manifest(&dir, &manifest).unwrap();
        assert!(recorder
            .export(temp.path().join("schema.zip"))
            .await
            .is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            assert_eq!(
                fs::metadata(dir.join("client/request.body"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            symlink(temp.path(), root.join("link")).unwrap();
            assert!(recorder
                .export(temp.path().join("symlink.zip"))
                .await
                .is_err());
            assert!(TrafficRecorder::open(config(&root.join("link"), true))
                .await
                .is_err());
        }
        assert_eq!(
            redact_url("/messages?token=secret#fragment", &http::HeaderMap::new()),
            "/messages"
        );
    }
}
