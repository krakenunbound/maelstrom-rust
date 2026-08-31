//! Bounded, read-only proxy validation. Submission never inspects files or waits on a job.

use crate::{MAX_CACHE_BYTES, MAX_CACHE_ITEMS, PROXY_PROFILE_VERSION, ProxyArtifact};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
};

pub const VALIDATION_CAPACITY: usize = MAX_CACHE_ITEMS;

#[derive(Clone, Debug)]
pub struct ProxyValidationRequest {
    pub token: u64,
    pub original: PathBuf,
    pub artifact: ProxyArtifact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProxyValidationResult {
    pub token: u64,
    pub usable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyValidationSubmitError {
    Busy,
    Closed,
}

struct Submission {
    epoch: u64,
    request: ProxyValidationRequest,
}

/// One worker per owning app, with separately bounded request and result queues. Project reset
/// invalidates work without joining the worker; Drop is reserved for final owner shutdown.
pub struct ProxyValidationWorker {
    requests: Option<SyncSender<Submission>>,
    results: Option<Receiver<ProxyValidationResult>>,
    epoch: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ProxyValidationWorker {
    pub fn start(notify: impl Fn() + Send + 'static) -> Result<Self, String> {
        Self::start_with_validator(notify, validate)
    }

    fn start_with_validator(
        notify: impl Fn() + Send + 'static,
        mut validate: impl FnMut(&ProxyValidationRequest) -> bool + Send + 'static,
    ) -> Result<Self, String> {
        let (requests, incoming) = mpsc::sync_channel::<Submission>(VALIDATION_CAPACITY);
        let (outgoing, results) = mpsc::sync_channel(VALIDATION_CAPACITY);
        let epoch = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_epoch = Arc::clone(&epoch);
        let worker_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("maelstrom-proxy-validation".into())
            .spawn(move || {
                while let Ok(submission) = incoming.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if submission.epoch != worker_epoch.load(Ordering::Acquire) {
                        continue;
                    }
                    let usable = validate(&submission.request);
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if submission.epoch != worker_epoch.load(Ordering::Acquire) {
                        continue;
                    }
                    if outgoing
                        .send(ProxyValidationResult {
                            token: submission.request.token,
                            usable,
                        })
                        .is_err()
                    {
                        break;
                    }
                    notify();
                }
            })
            .map_err(|error| format!("could not start proxy validation worker: {error}"))?;
        Ok(Self {
            requests: Some(requests),
            results: Some(results),
            epoch,
            stop,
            join: Some(join),
        })
    }

    pub fn try_submit(
        &self,
        request: ProxyValidationRequest,
    ) -> Result<(), ProxyValidationSubmitError> {
        let submission = Submission {
            epoch: self.epoch.load(Ordering::Acquire),
            request,
        };
        match self
            .requests
            .as_ref()
            .expect("live worker sender")
            .try_send(submission)
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(ProxyValidationSubmitError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(ProxyValidationSubmitError::Closed),
        }
    }

    pub fn try_recv(&self) -> Result<ProxyValidationResult, TryRecvError> {
        self.results
            .as_ref()
            .expect("live worker receiver")
            .try_recv()
    }

    pub fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for ProxyValidationWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.requests.take();
        // Unblock a worker publishing into a full result queue before joining it.
        self.results.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn validate(request: &ProxyValidationRequest) -> bool {
    let artifact = &request.artifact;
    artifact.profile_version == PROXY_PROFILE_VERSION
        && artifact.output_bytes > 0
        && artifact.output_bytes <= MAX_CACHE_BYTES
        && fs::symlink_metadata(&artifact.path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == artifact.output_bytes)
        && artifact.source.matches(&request.original)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceFingerprint;
    use std::{
        sync::Mutex,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    fn request(token: u64) -> ProxyValidationRequest {
        ProxyValidationRequest {
            token,
            original: PathBuf::from("original.mp4"),
            artifact: ProxyArtifact {
                path: PathBuf::from("proxy.mp4"),
                source: SourceFingerprint {
                    canonical_path: PathBuf::from("original.mp4"),
                    bytes: 1,
                    modified_unix_nanos: 1,
                },
                output_bytes: 1,
                profile_version: PROXY_PROFILE_VERSION,
            },
        }
    }

    fn receive(worker: &ProxyValidationWorker) -> ProxyValidationResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match worker.try_recv() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => {}
                Err(error) => panic!("validation worker closed: {error}"),
            }
            assert!(Instant::now() < deadline, "validation timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn validation_queue_is_bounded_while_worker_is_blocked() {
        let (started, signal) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let mut first = true;
        let worker = ProxyValidationWorker::start_with_validator(
            || {},
            move |_| {
                if first {
                    first = false;
                    started.send(thread::current().id()).unwrap();
                    gate.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                true
            },
        )
        .unwrap();
        worker.try_submit(request(1)).unwrap();
        assert_ne!(
            signal.recv_timeout(Duration::from_secs(1)).unwrap(),
            thread::current().id()
        );
        for token in 0..VALIDATION_CAPACITY {
            worker.try_submit(request(token as u64 + 2)).unwrap();
        }
        assert_eq!(
            worker.try_submit(request(1000)),
            Err(ProxyValidationSubmitError::Busy)
        );
        release.send(()).unwrap();
        drop(worker);
    }

    #[test]
    fn validation_reset_discards_inflight_and_queued_old_requests() {
        let (started, signal) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let checked = Arc::new(Mutex::new(Vec::new()));
        let worker_checked = Arc::clone(&checked);
        let worker = ProxyValidationWorker::start_with_validator(
            || {},
            move |request| {
                worker_checked.lock().unwrap().push(request.token);
                if request.token == 1 {
                    started.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                true
            },
        )
        .unwrap();
        worker.try_submit(request(1)).unwrap();
        signal.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.try_submit(request(2)).unwrap();
        worker.invalidate();
        release.send(()).unwrap();
        worker.try_submit(request(3)).unwrap();
        assert_eq!(
            receive(&worker),
            ProxyValidationResult {
                token: 3,
                usable: true
            }
        );
        drop(worker);
        assert_eq!(*checked.lock().unwrap(), vec![1, 3]);
    }

    #[test]
    fn validation_shutdown_unblocks_a_full_result_queue() {
        let (entered, signal) = mpsc::channel();
        let (notify, notified) = mpsc::channel();
        let worker = ProxyValidationWorker::start_with_validator(
            move || {
                let _ = notify.send(());
            },
            move |request| {
                if request.token == VALIDATION_CAPACITY as u64 + 1 {
                    entered.send(()).unwrap();
                }
                true
            },
        )
        .unwrap();
        worker.try_submit(request(1)).unwrap();
        notified.recv_timeout(Duration::from_secs(2)).unwrap();
        for token in 2..=VALIDATION_CAPACITY as u64 + 1 {
            worker.try_submit(request(token)).unwrap();
        }
        signal.recv_timeout(Duration::from_secs(2)).unwrap();
        // The final send cannot complete until the receiver is drained or dropped.
        drop(worker);
        assert_eq!(notified.try_iter().count(), VALIDATION_CAPACITY - 1);
    }

    #[test]
    fn validation_rejects_changed_source_proxy_size_profile_and_missing_output() {
        let root = std::env::temp_dir().join(format!(
            "nle-proxy-validation-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let original = root.join("original.mp4");
        let proxy = root.join("proxy.mp4");
        fs::write(&original, b"source").unwrap();
        fs::write(&proxy, b"proxy").unwrap();
        let mut request = ProxyValidationRequest {
            token: 1,
            original: original.clone(),
            artifact: ProxyArtifact {
                path: proxy.clone(),
                source: SourceFingerprint::capture(&original).unwrap(),
                output_bytes: 5,
                profile_version: PROXY_PROFILE_VERSION,
            },
        };
        assert!(validate(&request));
        request.artifact.profile_version += 1;
        assert!(!validate(&request));
        request.artifact.profile_version = PROXY_PROFILE_VERSION;
        fs::write(&proxy, b"changed proxy size").unwrap();
        assert!(!validate(&request));
        fs::write(&proxy, b"proxy").unwrap();
        fs::write(&original, b"new source contents").unwrap();
        assert!(!validate(&request));
        request.artifact.source = SourceFingerprint::capture(&original).unwrap();
        assert!(validate(&request));
        fs::remove_file(&proxy).unwrap();
        assert!(!validate(&request));
        fs::remove_dir_all(root).unwrap();
    }
}
