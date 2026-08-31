//! Optional derived video proxies.  This crate never changes source media or exports.

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

/// A proxy profile change must invalidate all previously generated artifacts.
pub const PROXY_PROFILE_VERSION: u32 = 1;
pub const MAX_CACHE_ITEMS: usize = 64;
pub const MAX_CACHE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const OUTPUT_PREFIX: &str = "maelstrom-proxy-v";

#[derive(Clone, Debug)]
pub struct ProxyRequest {
    pub input: PathBuf,
    pub cache_root: PathBuf,
    pub ffmpeg: PathBuf,
    /// Regenerate this derived artifact instead of reusing the matching cache entry.
    pub replace_existing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub canonical_path: PathBuf,
    pub bytes: u64,
    pub modified_unix_nanos: i128,
}

impl SourceFingerprint {
    pub fn capture(path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("could not read source {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("source is not a file: {}", path.display()));
        }
        let modified = metadata.modified().map_err(|error| {
            format!(
                "could not read source modification time {}: {error}",
                path.display()
            )
        })?;
        Ok(Self {
            canonical_path: fs::canonicalize(path).map_err(|error| {
                format!("could not canonicalize source {}: {error}", path.display())
            })?,
            bytes: metadata.len(),
            modified_unix_nanos: unix_nanos(modified),
        })
    }

    pub fn matches(&self, path: &Path) -> bool {
        Self::capture(path).is_ok_and(|current| current == *self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyArtifact {
    pub path: PathBuf,
    pub source: SourceFingerprint,
    pub output_bytes: u64,
    pub profile_version: u32,
}

/// Rediscovers a completed current-profile cache entry without generating, deleting, or pruning.
/// Performs filesystem I/O: call only on a worker, never on the UI or monitor-submit thread.
/// A miss (including cancellation, source changes or invalid files) leaves originals authoritative.
pub fn find_cached_proxy(
    input: &Path,
    cache_root: &Path,
    cancel: &AtomicBool,
) -> Option<ProxyArtifact> {
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    let source = SourceFingerprint::capture(input).ok()?;
    let path = artifact_path(cache_root, &source);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CACHE_BYTES {
        return None;
    }
    if cancel.load(Ordering::Acquire) || !source.matches(input) {
        return None;
    }
    Some(ProxyArtifact {
        path,
        source,
        output_bytes: metadata.len(),
        profile_version: PROXY_PROFILE_VERSION,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProxyEvent {
    Progress(f32),
    Completed(ProxyArtifact),
    Cancelled,
    Failed(String),
}

pub struct ProxyJob {
    cancel: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    events: mpsc::Receiver<ProxyEvent>,
    join: Option<thread::JoinHandle<()>>,
}

impl ProxyJob {
    /// Starts ownership of a background job without inspecting source/tool files on the caller.
    /// Only worker-spawn failures are returned here; validation failures arrive as `Failed` events.
    pub fn start(
        request: ProxyRequest,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let (tx, events) = mpsc::channel();
        let worker_cancel = Arc::clone(&cancel);
        let worker_child = Arc::clone(&child);
        let notify = Arc::new(notify);
        let join = thread::Builder::new()
            .name("maelstrom-proxy".into())
            .spawn(move || run_job(request, worker_cancel, worker_child, tx, notify))
            .map_err(|error| format!("could not start proxy worker: {error}"))?;
        Ok(Self {
            cancel,
            child,
            events,
            join: Some(join),
        })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Ok(mut child) = self.child.lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.kill();
        }
    }

    pub fn try_recv(&self) -> Result<ProxyEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }
}

impl Drop for ProxyJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyDeleteEvent {
    Completed,
    Failed(String),
}

/// Owns asynchronous proxy deletion so UI event handling never performs disk I/O.
pub struct ProxyDeleteJob {
    cancel: Arc<AtomicBool>,
    events: mpsc::Receiver<ProxyDeleteEvent>,
    join: Option<thread::JoinHandle<()>>,
}

impl ProxyDeleteJob {
    pub fn start(path: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, events) = mpsc::channel();
        let notify = Arc::new(notify);
        let join = thread::Builder::new()
            .name("maelstrom-proxy-delete".into())
            .spawn(move || {
                let event = if worker_cancel.load(Ordering::Acquire) || !path.exists() {
                    ProxyDeleteEvent::Completed
                } else {
                    match fs::remove_file(&path) {
                        Ok(()) => ProxyDeleteEvent::Completed,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            ProxyDeleteEvent::Completed
                        }
                        Err(error) => ProxyDeleteEvent::Failed(format!(
                            "could not delete proxy {}: {error}",
                            path.display()
                        )),
                    }
                };
                let _ = tx.send(event);
                notify();
            })
            .map_err(|error| format!("could not start proxy delete worker: {error}"))?;
        Ok(Self {
            cancel,
            events,
            join: Some(join),
        })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn try_recv(&self) -> Result<ProxyDeleteEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }
}

impl Drop for ProxyDeleteJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn validate_request(request: &ProxyRequest) -> Result<(), String> {
    let source = fs::metadata(&request.input)
        .map_err(|error| format!("source is missing: {} ({error})", request.input.display()))?;
    if !source.is_file() {
        return Err(format!("source is not a file: {}", request.input.display()));
    }
    let ffmpeg = fs::metadata(&request.ffmpeg)
        .map_err(|error| format!("ffmpeg is missing: {} ({error})", request.ffmpeg.display()))?;
    if !ffmpeg.is_file() {
        return Err(format!(
            "ffmpeg is not a file: {}",
            request.ffmpeg.display()
        ));
    }
    Ok(())
}

fn run_job(
    request: ProxyRequest,
    cancel: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    events: mpsc::Sender<ProxyEvent>,
    notify: Arc<dyn Fn() + Send + Sync>,
) {
    let result = (|| {
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        // Metadata queries and canonicalization can stall on offline/network storage. Keep the
        // complete validation/fingerprint phase on the owned worker, before any cache mutation.
        validate_request(&request)?;
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        let source = SourceFingerprint::capture(&request.input)?;
        generate_proxy(&request, &source, &cancel, &child, &events, &notify)
    })();
    let event = match result {
        Ok(artifact) if cancel.load(Ordering::Acquire) => ProxyEvent::Cancelled,
        Ok(artifact) => ProxyEvent::Completed(artifact),
        Err(error) if cancel.load(Ordering::Acquire) || error == "cancelled" => {
            ProxyEvent::Cancelled
        }
        Err(error) => ProxyEvent::Failed(error),
    };
    send_event(&events, event, &notify);
}

fn generate_proxy(
    request: &ProxyRequest,
    source: &SourceFingerprint,
    cancel: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
    events: &mpsc::Sender<ProxyEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<ProxyArtifact, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    fs::create_dir_all(&request.cache_root).map_err(|error| {
        format!(
            "could not create proxy cache {}: {error}",
            request.cache_root.display()
        )
    })?;
    prune_cache(&request.cache_root, None)?;
    let final_path = artifact_path(&request.cache_root, source);
    if final_path.exists() {
        if !request.replace_existing && final_path.is_file() {
            return artifact_for(final_path, source.clone());
        }
        fs::remove_file(&final_path).map_err(|error| {
            format!(
                "could not replace existing proxy {}: {error}",
                final_path.display()
            )
        })?;
    }

    let temp_path = temporary_path(&final_path);
    let _ = fs::remove_file(&temp_path);
    let duration_us = probe_duration_us(&request.ffmpeg, &request.input);
    let result = run_ffmpeg(
        request,
        &temp_path,
        duration_us,
        cancel,
        child_slot,
        events,
        notify,
    );
    if result.is_err() || cancel.load(Ordering::Acquire) {
        let _ = fs::remove_file(&temp_path);
        return result.map(|_| unreachable!());
    }
    if !temp_path.is_file() {
        return Err("ffmpeg completed without writing a proxy".into());
    }
    if cancel.load(Ordering::Acquire) {
        let _ = fs::remove_file(&temp_path);
        return Err("cancelled".into());
    }
    match SourceFingerprint::capture(&request.input) {
        Ok(current) if current == *source => {}
        Ok(_) => {
            let _ = fs::remove_file(&temp_path);
            return Err("source changed while generating proxy; artifact was not published".into());
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            return Err(format!(
                "could not validate source before publishing proxy; artifact was not published: {error}"
            ));
        }
    }
    fs::rename(&temp_path, &final_path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!("could not publish proxy {}: {error}", final_path.display())
    })?;
    if cancel.load(Ordering::Acquire) {
        let _ = fs::remove_file(&final_path);
        return Err("cancelled".into());
    }
    prune_cache(&request.cache_root, None)?;
    if !final_path.exists() {
        return Err("proxy cache cap removed the newly generated artifact".into());
    }
    send_event(events, ProxyEvent::Progress(1.0), notify);
    artifact_for(final_path, source.clone())
}

fn run_ffmpeg(
    request: &ProxyRequest,
    output: &Path,
    duration_us: Option<u64>,
    cancel: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
    events: &mpsc::Sender<ProxyEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let mut command = hidden_command(&request.ffmpeg);
    command.args(ffmpeg_arguments(&request.input, output));
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut spawned = command
        .spawn()
        .map_err(|error| format!("could not start ffmpeg: {error}"))?;
    let stdout = spawned
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg progress pipe was unavailable".to_owned())?;
    {
        let mut slot = child_slot
            .lock()
            .map_err(|_| "proxy child lock poisoned".to_owned())?;
        *slot = Some(spawned);
    }
    send_event(events, ProxyEvent::Progress(0.0), notify);
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let read = match reader.read_line(&mut line) {
            Ok(read) => read,
            Err(error) => {
                kill_and_wait_active_child(child_slot);
                return Err(format!("could not read ffmpeg progress: {error}"));
            }
        };
        if read == 0 {
            break;
        }
        if let Some(value) = line.trim().strip_prefix("out_time_us=")
            && let (Ok(out_time), Some(duration)) = (value.parse::<u64>(), duration_us)
        {
            let progress = (out_time as f64 / duration.max(1) as f64) as f32;
            send_event(
                events,
                ProxyEvent::Progress(progress.clamp(0.0, 0.99)),
                notify,
            );
        }
        if cancel.load(Ordering::Acquire) {
            kill_and_wait_active_child(child_slot);
            return Err("cancelled".into());
        }
    }
    let mut child = child_slot
        .lock()
        .map_err(|_| "proxy child lock poisoned".to_owned())?
        .take()
        .ok_or_else(|| "proxy child disappeared".to_owned())?;
    let mut stderr = String::new();
    if let Some(mut handle) = child.stderr.take() {
        let _ = handle.read_to_string(&mut stderr);
    }
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for ffmpeg: {error}"))?;
    if cancel.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    if !status.success() {
        return Err(format!("ffmpeg proxy generation failed: {}", stderr.trim()));
    }
    Ok(())
}

fn ffmpeg_arguments(input: &Path, output: &Path) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-copyts".into(),
        "-start_at_zero".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-an".into(),
        "-vf".into(),
        "scale=w='min(1280,iw)':h='min(720,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2".into(),
        "-c:v".into(),
        "mpeg4".into(),
        "-q:v".into(),
        "5".into(),
        "-g".into(),
        "1".into(),
        "-bf".into(),
        "0".into(),
        "-fps_mode".into(),
        "passthrough".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-y".into(),
        "-f".into(),
        "mp4".into(),
        output.to_string_lossy().into_owned(),
    ]
}

fn send_event(
    events: &mpsc::Sender<ProxyEvent>,
    event: ProxyEvent,
    notify: &Arc<dyn Fn() + Send + Sync>,
) {
    let _ = events.send(event);
    notify();
}

fn kill_and_wait_active_child(child_slot: &Mutex<Option<Child>>) {
    if let Ok(mut slot) = child_slot.lock()
        && let Some(mut child) = slot.take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn artifact_for(path: PathBuf, source: SourceFingerprint) -> Result<ProxyArtifact, String> {
    let bytes = fs::metadata(&path)
        .map_err(|error| {
            format!(
                "could not inspect generated proxy {}: {error}",
                path.display()
            )
        })?
        .len();
    Ok(ProxyArtifact {
        path,
        source,
        output_bytes: bytes,
        profile_version: PROXY_PROFILE_VERSION,
    })
}

fn artifact_path(cache_root: &Path, source: &SourceFingerprint) -> PathBuf {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    stable_hash(&mut hash, &PROXY_PROFILE_VERSION.to_le_bytes());
    stable_hash(
        &mut hash,
        source.canonical_path.to_string_lossy().as_bytes(),
    );
    stable_hash(&mut hash, &source.bytes.to_le_bytes());
    stable_hash(&mut hash, &source.modified_unix_nanos.to_le_bytes());
    cache_root.join(format!(
        "{OUTPUT_PREFIX}{PROXY_PROFILE_VERSION}-{hash:016x}.mp4"
    ))
}

fn stable_hash(state: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *state ^= u64::from(*byte);
        *state = state.wrapping_mul(0x100_0000_01b3);
    }
}

fn temporary_path(final_path: &Path) -> PathBuf {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    final_path.with_extension(format!("mp4.part-{}-{id}", std::process::id()))
}

fn probe_duration_us(ffmpeg: &Path, input: &Path) -> Option<u64> {
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    if !ffprobe.is_file() {
        return None;
    }
    let output = hidden_command(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            input.to_string_lossy().as_ref(),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|seconds| *seconds > 0.0)
        .map(|seconds| (seconds * 1_000_000.0) as u64)
}

fn prune_cache(cache_root: &Path, keep: Option<&Path>) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(cache_root).map_err(|error| {
        format!(
            "could not enumerate proxy cache {}: {error}",
            cache_root.display()
        )
    })? {
        let entry =
            entry.map_err(|error| format!("could not inspect proxy cache entry: {error}"))?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() && is_proxy_artifact(&path) => metadata,
            _ => continue,
        };
        entries.push((
            metadata.modified().unwrap_or(UNIX_EPOCH),
            path,
            metadata.len(),
        ));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut total = entries.iter().map(|entry| entry.2).sum::<u64>();
    let mut count = entries.len();
    for (_, path, bytes) in entries {
        if count <= MAX_CACHE_ITEMS && total <= MAX_CACHE_BYTES {
            break;
        }
        if keep.is_some_and(|protected| protected == path) {
            continue;
        }
        fs::remove_file(&path)
            .map_err(|error| format!("could not prune proxy {}: {error}", path.display()))?;
        count -= 1;
        total = total.saturating_sub(bytes);
    }
    Ok(())
}

fn is_proxy_artifact(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(OUTPUT_PREFIX))
}

fn unix_nanos(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    }
}

fn hidden_command(program: &Path) -> Command {
    let is_batch = program
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        });
    let mut command = if is_batch {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(program);
        command
    } else {
        Command::new(program)
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
        time::Duration,
    };

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nle-proxy-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_source(root: &Path) -> PathBuf {
        let source = root.join("source.mov");
        fs::write(&source, b"source-media").unwrap();
        source
    }

    fn fake_ffmpeg(root: &Path, body: &str) -> PathBuf {
        let path = root.join("fake-ffmpeg.cmd");
        fs::write(&path, format!("@echo off\r\n{body}\r\n")).unwrap();
        path
    }

    fn wait_event(job: &ProxyJob) -> ProxyEvent {
        wait_event_with_attempts(job, 100)
    }

    fn wait_event_with_attempts(job: &ProxyJob, attempts: usize) -> ProxyEvent {
        for _ in 0..attempts {
            match job.try_recv() {
                Ok(
                    event @ (ProxyEvent::Completed(_)
                    | ProxyEvent::Cancelled
                    | ProxyEvent::Failed(_)),
                ) => return event,
                Ok(_) | Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("event channel closed: {error}"),
            }
        }
        panic!("proxy job did not finish")
    }

    fn wait_delete_event(job: &ProxyDeleteJob) -> ProxyDeleteEvent {
        for _ in 0..100 {
            match job.try_recv() {
                Ok(event) => return event,
                Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("delete event channel closed: {error}"),
            }
        }
        panic!("proxy delete job did not finish")
    }

    #[test]
    fn fingerprint_invalidates_when_source_changes() {
        let root = fixture("fingerprint");
        let source = write_source(&root);
        let fingerprint = SourceFingerprint::capture(&source).unwrap();
        fs::write(&source, b"changed-source-media").unwrap();
        assert!(!fingerprint.matches(&source));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_key_is_deterministic_for_one_source_profile() {
        let root = fixture("key");
        let source = write_source(&root);
        let fingerprint = SourceFingerprint::capture(&source).unwrap();
        assert_eq!(
            artifact_path(&root, &fingerprint),
            artifact_path(&root, &fingerprint)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cached_proxy_discovery_is_read_only_and_rejects_stale_or_cancelled_sources() {
        let root = fixture("rediscover");
        let source = write_source(&root);
        let cache = root.join("cache");
        let cancel = AtomicBool::new(false);
        assert!(find_cached_proxy(&source, &cache, &cancel).is_none());
        assert!(!cache.exists(), "discovery must not create a cache");
        fs::create_dir(&cache).unwrap();
        let fingerprint = SourceFingerprint::capture(&source).unwrap();
        let path = artifact_path(&cache, &fingerprint);
        fs::write(&path, b"completed-cache-entry").unwrap();
        let found = find_cached_proxy(&source, &cache, &cancel).unwrap();
        assert_eq!(found, artifact_for(path.clone(), fingerprint).unwrap());
        cancel.store(true, Ordering::Release);
        assert!(find_cached_proxy(&source, &cache, &cancel).is_none());
        cancel.store(false, Ordering::Release);
        fs::write(&source, b"replacement source with different size").unwrap();
        assert!(find_cached_proxy(&source, &cache, &cancel).is_none());
        assert_eq!(fs::read(&path).unwrap(), b"completed-cache-entry");
        fs::remove_file(&source).unwrap();
        assert!(find_cached_proxy(&source, &cache, &cancel).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_proxy_discovery_ignores_partial_empty_directory_and_old_profile_entries() {
        let root = fixture("rediscover-incomplete");
        let source = write_source(&root);
        let cancel = AtomicBool::new(false);
        let fingerprint = SourceFingerprint::capture(&source).unwrap();
        let path = artifact_path(&root, &fingerprint);
        let partial = temporary_path(&path);
        fs::write(&partial, b"not yet published").unwrap();
        fs::write(root.join("maelstrom-proxy-v0-0000000000000000.mp4"), b"old").unwrap();
        assert!(find_cached_proxy(&source, &root, &cancel).is_none());
        fs::write(&path, b"").unwrap();
        assert!(find_cached_proxy(&source, &root, &cancel).is_none());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(find_cached_proxy(&source, &root, &cancel).is_none());
        assert!(
            partial.exists(),
            "discovery must not clean up another worker's output"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ffmpeg_explicitly_selects_mp4_for_temporary_output() {
        let arguments = ffmpeg_arguments(Path::new("source.mov"), Path::new("proxy.mp4.part-1"));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-copyts", "-start_at_zero"])
        );
        assert_eq!(
            arguments[arguments.len() - 3..],
            ["-f", "mp4", "proxy.mp4.part-1"]
        );
    }

    #[test]
    fn cache_pruning_enforces_item_cap() {
        let root = fixture("prune");
        for index in 0..=MAX_CACHE_ITEMS {
            fs::write(
                root.join(format!("{OUTPUT_PREFIX}1-{index:016x}.mp4")),
                b"x",
            )
            .unwrap();
        }
        prune_cache(&root, None).unwrap();
        let count = fs::read_dir(&root).unwrap().count();
        assert_eq!(count, MAX_CACHE_ITEMS);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_pruning_enforces_byte_cap_with_sparse_files() {
        let root = fixture("prune-bytes");
        let old = root.join(format!("{OUTPUT_PREFIX}1-0000000000000000.mp4"));
        fs::File::create(&old)
            .unwrap()
            .set_len(MAX_CACHE_BYTES)
            .unwrap();
        thread::sleep(Duration::from_millis(30));
        let newest = root.join(format!("{OUTPUT_PREFIX}1-ffffffffffffffff.mp4"));
        fs::write(&newest, b"x").unwrap();
        prune_cache(&root, None).unwrap();
        assert!(!old.exists(), "oldest entry must be evicted at byte cap");
        assert!(newest.exists(), "newest entry within cap must be retained");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_ffmpeg_never_publishes_a_proxy() {
        let root = fixture("failed");
        let source = write_source(&root);
        let ffmpeg = fake_ffmpeg(&root, "exit /b 7");
        let job = ProxyJob::start(
            ProxyRequest {
                input: source,
                cache_root: root.join("cache"),
                ffmpeg,
                replace_existing: false,
            },
            || {},
        )
        .unwrap();
        assert!(matches!(wait_event(&job), ProxyEvent::Failed(_)));
        assert!(
            !job.events
                .try_iter()
                .any(|event| matches!(event, ProxyEvent::Completed(_)))
        );
        let cache = root.join("cache");
        assert!(!cache.exists() || fs::read_dir(cache).unwrap().next().is_none());
        drop(job);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn proxy_start_reports_invalid_paths_as_worker_failures() {
        let root = fixture("async-validation");
        let source = write_source(&root);
        let tool = fake_ffmpeg(&root, "exit /b 7");
        let caller = thread::current().id();
        for (input, ffmpeg, expected) in [
            (root.join("missing.mov"), tool.clone(), "source is missing"),
            (root.clone(), tool.clone(), "source is not a file"),
            (
                source.clone(),
                root.join("missing-tool.exe"),
                "ffmpeg is missing",
            ),
            (source.clone(), root.clone(), "ffmpeg is not a file"),
        ] {
            let (tx, notifications) = mpsc::channel();
            let started = ProxyJob::start(
                ProxyRequest {
                    input,
                    cache_root: root.join("cache"),
                    ffmpeg,
                    replace_existing: false,
                },
                move || {
                    let _ = tx.send(thread::current().id());
                },
            );
            let job = match started {
                Ok(job) => job,
                Err(error) => {
                    fs::remove_dir_all(&root).unwrap();
                    panic!("filesystem validation ran synchronously: {error}");
                }
            };
            assert!(
                matches!(wait_event(&job), ProxyEvent::Failed(error) if error.contains(expected))
            );
            assert_ne!(
                notifications.recv_timeout(Duration::from_secs(1)).unwrap(),
                caller
            );
            drop(job);
            assert!(
                !root.join("cache").exists(),
                "failed validation must not mutate cache"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_cancelled_proxy_request_emits_only_cancelled() {
        let root = fixture("pre-cancelled-validation");
        let child = Arc::new(Mutex::new(None));
        let (tx, events) = mpsc::channel();
        run_job(
            ProxyRequest {
                input: root.join("missing.mov"),
                cache_root: root.join("cache"),
                ffmpeg: root.join("missing-tool.exe"),
                replace_existing: false,
            },
            Arc::new(AtomicBool::new(true)),
            Arc::clone(&child),
            tx,
            Arc::new(|| {}),
        );
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![ProxyEvent::Cancelled]
        );
        assert!(child.lock().unwrap().is_none());
        assert!(!root.join("cache").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replace_existing_controls_cached_artifact_reuse() {
        let root = fixture("replace");
        let source = write_source(&root);
        let cache = root.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let fingerprint = SourceFingerprint::capture(&source).unwrap();
        let existing = artifact_path(&cache, &fingerprint);
        fs::write(&existing, b"stale").unwrap();

        let reuse_job = ProxyJob::start(
            ProxyRequest {
                input: source.clone(),
                cache_root: cache.clone(),
                ffmpeg: fake_ffmpeg(&root, "exit /b 7"),
                replace_existing: false,
            },
            || {},
        )
        .unwrap();
        let ProxyEvent::Completed(reused) = wait_event(&reuse_job) else {
            panic!("matching existing proxy should be reused");
        };
        assert_eq!(fs::read(&reused.path).unwrap(), b"stale");
        drop(reuse_job);

        let regenerate = fake_ffmpeg(
            &root,
            "set out=\r\n:args\r\nif \"%~1\"==\"\" goto write\r\nset out=%~1\r\nshift\r\ngoto args\r\n:write\r\n> \"%out%\" echo regenerated",
        );
        let replace_job = ProxyJob::start(
            ProxyRequest {
                input: source,
                cache_root: cache,
                ffmpeg: regenerate,
                replace_existing: true,
            },
            || {},
        )
        .unwrap();
        let ProxyEvent::Completed(replaced) = wait_event(&replace_job) else {
            panic!("replace_existing must regenerate the proxy");
        };
        assert_ne!(fs::read(&replaced.path).unwrap(), b"stale");
        drop(replace_job);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn changed_source_during_encode_is_never_published() {
        let root = fixture("source-changed");
        let source = write_source(&root);
        let cache = root.join("cache");
        let initial_fingerprint = SourceFingerprint::capture(&source).unwrap();
        let ffmpeg = fake_ffmpeg(
            &root,
            "ping 127.0.0.1 -n 2 > nul\r\nset out=\r\n:args\r\nif \"%~1\"==\"\" goto write\r\nset out=%~1\r\nshift\r\ngoto args\r\n:write\r\n> \"%out%\" echo generated",
        );
        let job = ProxyJob::start(
            ProxyRequest {
                input: source.clone(),
                cache_root: cache.clone(),
                ffmpeg,
                replace_existing: true,
            },
            || {},
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while job.child.lock().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "encoding did not start"
            );
            thread::sleep(Duration::from_millis(1));
        }
        fs::write(&source, b"source media changed while encoding").unwrap();
        assert!(matches!(
            wait_event(&job),
            ProxyEvent::Failed(error) if error.contains("source changed while generating proxy")
        ));
        assert!(!artifact_path(&cache, &initial_fingerprint).exists());
        assert!(
            !cache.exists()
                || fs::read_dir(&cache).unwrap().all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".part-"))
        );
        drop(job);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn proxy_delete_job_removes_existing_file() {
        let root = fixture("delete-success");
        let proxy = root.join("proxy.mp4");
        fs::write(&proxy, b"proxy").unwrap();
        let job = ProxyDeleteJob::start(proxy.clone(), || {}).unwrap();
        assert_eq!(wait_delete_event(&job), ProxyDeleteEvent::Completed);
        assert!(!proxy.exists());
        drop(job);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn proxy_delete_job_treats_missing_file_as_completed() {
        let root = fixture("delete-missing");
        let job = ProxyDeleteJob::start(root.join("missing.mp4"), || {}).unwrap();
        assert_eq!(wait_delete_event(&job), ProxyDeleteEvent::Completed);
        drop(job);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn proxy_delete_job_reports_delete_failure() {
        let root = fixture("delete-failure");
        let directory = root.join("proxy.mp4");
        fs::create_dir(&directory).unwrap();
        let job = ProxyDeleteJob::start(directory, || {}).unwrap();
        assert!(matches!(
            wait_delete_event(&job),
            ProxyDeleteEvent::Failed(_)
        ));
        drop(job);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_kills_job_and_cleans_temporary_output() {
        let root = fixture("cancel");
        let source = write_source(&root);
        let ffmpeg = fake_ffmpeg(
            &root,
            ":loop\r\necho out_time_us=1\r\necho progress=continue\r\ngoto loop",
        );
        let cache = root.join("cache");
        let wakes = Arc::new(AtomicUsize::new(0));
        let notify_wakes = Arc::clone(&wakes);
        let job = ProxyJob::start(
            ProxyRequest {
                input: source,
                cache_root: cache.clone(),
                ffmpeg,
                replace_existing: false,
            },
            move || {
                notify_wakes.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match job.try_recv() {
                Ok(ProxyEvent::Progress(_)) => break,
                Err(mpsc::TryRecvError::Empty) => {}
                other => panic!("encoding did not reach progress: {other:?}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "encoding progress timed out"
            );
            thread::sleep(Duration::from_millis(1));
        }
        job.cancel();
        assert!(matches!(wait_event(&job), ProxyEvent::Cancelled));
        drop(job); // The terminal event may be received just before its notifier finishes.
        assert!(
            wakes.load(Ordering::Relaxed) >= 2,
            "progress and terminal event must wake UI"
        );
        assert!(fs::read_dir(&cache).unwrap().next().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires MAELSTROM_TEST_FFMPEG and a local bundled FFmpeg runtime"]
    fn real_media_proxy_is_video_only_bounded_and_timestamp_normalized() {
        let Ok(ffmpeg) = std::env::var("MAELSTROM_TEST_FFMPEG") else {
            eprintln!("skipping: MAELSTROM_TEST_FFMPEG is not set");
            return;
        };
        let ffmpeg = PathBuf::from(ffmpeg);
        assert!(ffmpeg.is_file(), "MAELSTROM_TEST_FFMPEG must name a file");
        let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        assert!(ffprobe.is_file(), "ffprobe must be adjacent to test ffmpeg");

        let root = fixture("real-media");
        let source = root.join("source-1080p.mp4");
        let status = hidden_command(&ffmpeg)
            .args([
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1920x1080:rate=30000/1001",
                "-t",
                "1.2",
                "-an",
                "-c:v",
                "mpeg4",
                "-q:v",
                "4",
                "-g",
                "12",
                "-bf",
                "0",
                "-y",
                source.to_string_lossy().as_ref(),
            ])
            .status()
            .expect("could not create real-media proxy fixture");
        assert!(status.success(), "could not create dynamic 1080p source");

        let cache = root.join("cache");
        let job = ProxyJob::start(
            ProxyRequest {
                input: source.clone(),
                cache_root: cache.clone(),
                ffmpeg,
                replace_existing: false,
            },
            || {},
        )
        .expect("could not start real-media proxy job");
        let ProxyEvent::Completed(artifact) = wait_event_with_attempts(&job, 1_000) else {
            panic!("real-media proxy job did not complete successfully");
        };
        assert!(artifact.path.is_file());
        assert!(artifact.source.matches(&source));
        assert!(
            fs::read_dir(&cache).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".part-")),
            "proxy temporary files must be removed"
        );

        let probe = hidden_command(&ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,codec_name,width,height,has_b_frames,start_time:format=start_time",
                "-of",
                "default=noprint_wrappers=1:nokey=0",
                artifact.path.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("could not probe generated proxy");
        assert!(probe.status.success(), "ffprobe failed for generated proxy");
        let probe = String::from_utf8_lossy(&probe.stdout);
        assert_eq!(
            probe
                .lines()
                .filter(|line| *line == "codec_type=video")
                .count(),
            1,
            "proxy must contain only one video stream: {probe}"
        );
        assert!(probe.lines().any(|line| line == "codec_name=mpeg4"));
        let width = probe_value(&probe, "width").expect("proxy width missing");
        let height = probe_value(&probe, "height").expect("proxy height missing");
        assert!(
            width <= 1280 && height <= 720,
            "proxy dimensions exceed 720p: {width}x{height}"
        );
        assert_eq!(probe_value(&probe, "has_b_frames"), Some(0));
        let start_times = probe
            .lines()
            .filter_map(|line| line.strip_prefix("start_time="))
            .filter_map(|value| value.parse::<f64>().ok())
            .collect::<Vec<_>>();
        assert!(
            start_times.iter().any(|value| value.abs() <= 0.05),
            "proxy timestamp origin was not normalized: {probe}"
        );

        let frames = hidden_command(&ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=key_frame",
                "-of",
                "csv=p=0",
                artifact.path.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("could not inspect proxy keyframes");
        assert!(frames.status.success());
        let frame_flags = String::from_utf8_lossy(&frames.stdout);
        assert!(
            !frame_flags.trim().is_empty() && frame_flags.lines().all(|flag| flag.trim() == "1"),
            "proxy must use intra-only frames: {frame_flags}"
        );
        drop(job);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires MAELSTROM_TEST_FFMPEG and a local bundled FFmpeg runtime"]
    fn real_media_proxy_preserves_irregular_pts_intervals() {
        let Ok(ffmpeg) = std::env::var("MAELSTROM_TEST_FFMPEG") else {
            eprintln!("skipping: MAELSTROM_TEST_FFMPEG is not set");
            return;
        };
        let ffmpeg = PathBuf::from(ffmpeg);
        assert!(ffmpeg.is_file(), "MAELSTROM_TEST_FFMPEG must name a file");
        let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        assert!(ffprobe.is_file(), "ffprobe must be adjacent to test ffmpeg");

        let root = fixture("real-vfr");
        let source = root.join("source-irregular-pts.mp4");
        let status = hidden_command(&ffmpeg)
            .args([
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1920x1080:rate=30",
                "-frames:v",
                "18",
                "-vf",
                "setpts=if(eq(N\\,0)\\,0\\,PREV_OUTPTS+if(eq(mod(N\\,3)\\,0)\\,3\\,1))",
                "-fps_mode",
                "vfr",
                "-an",
                "-c:v",
                "mpeg4",
                "-q:v",
                "4",
                "-g",
                "12",
                "-bf",
                "0",
                "-y",
                source.to_string_lossy().as_ref(),
            ])
            .status()
            .expect("could not create irregular-PTS fixture");
        assert!(status.success(), "could not create irregular-PTS source");
        let source_intervals = frame_pts_intervals_us(&ffprobe, &source);
        assert!(
            source_intervals.windows(2).any(|pair| pair[0] != pair[1]),
            "fixture did not retain irregular PTS intervals: {source_intervals:?}"
        );

        let job = ProxyJob::start(
            ProxyRequest {
                input: source,
                cache_root: root.join("cache"),
                ffmpeg,
                replace_existing: false,
            },
            || {},
        )
        .expect("could not start irregular-PTS proxy job");
        let ProxyEvent::Completed(artifact) = wait_event_with_attempts(&job, 1_000) else {
            panic!("irregular-PTS proxy job did not complete successfully");
        };
        let proxy_intervals = frame_pts_intervals_us(&ffprobe, &artifact.path);
        assert_eq!(source_intervals.len(), proxy_intervals.len());
        for (index, (source, proxy)) in source_intervals.iter().zip(&proxy_intervals).enumerate() {
            assert!(
                (source - proxy).abs() <= 1_000,
                "PTS interval {index} drifted beyond 1ms: source={source}us proxy={proxy}us"
            );
        }
        drop(job);
        let _ = fs::remove_dir_all(root);
    }

    fn frame_pts_intervals_us(ffprobe: &Path, media: &Path) -> Vec<i64> {
        let output = hidden_command(ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=best_effort_timestamp_time",
                "-of",
                "csv=p=0",
                media.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("could not read frame timestamps");
        assert!(
            output.status.success(),
            "ffprobe frame timestamp read failed"
        );
        let timestamps = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|value| {
                (value
                    .trim()
                    .parse::<f64>()
                    .expect("frame timestamp must be numeric")
                    * 1_000_000.0)
                    .round() as i64
            })
            .collect::<Vec<_>>();
        assert!(timestamps.len() > 1, "fixture must contain multiple frames");
        timestamps
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .collect()
    }

    fn probe_value(text: &str, key: &str) -> Option<u32> {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .and_then(|value| value.parse().ok())
    }
}
