//! Opt-in windowed measurement. No physical input/scanout claims; normal sessions allocate none.
use super::*;
use std::io::Read;

pub(super) const WARMUP_SAMPLES: usize = 8;
pub(super) const MEASURED_SAMPLES: usize = 40;
const TOTAL_SAMPLES: usize = WARMUP_SAMPLES + MEASURED_SAMPLES;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Configuration {
    pub schema_version: u32,
    pub run_id: String,
    pub source_paths: Vec<PathBuf>,
    pub report_path: PathBuf,
    pub adapter_class: String,
}

impl Configuration {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 || !matches!(self.source_paths.len(), 1 | 4) {
            return Err("schema 1 requires one or four sources".into());
        }
        if self.run_id.is_empty()
            || self.run_id.len() > 80
            || !self
                .run_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err("invalid run identity".into());
        }
        if !matches!(self.adapter_class.as_str(), "IntegratedGpu" | "DiscreteGpu") {
            return Err("an explicit integrated/discrete adapter class is required".into());
        }
        if !self.report_path.is_absolute()
            || self.report_path.extension().is_none_or(|ext| ext != "json")
        {
            return Err("report must be an absolute JSON path".into());
        }
        let mut unique = HashSet::new();
        for path in &self.source_paths {
            if !path.is_absolute() || !path.is_file() {
                return Err("all sources must be existing absolute file paths".into());
            }
            let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
            if !unique.insert(canonical.to_string_lossy().to_lowercase()) {
                return Err("source files must be independent paths".into());
            }
        }
        if self.report_path.exists() {
            return Err("report already exists; use a fresh run directory".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LayerTarget {
    pub slot: usize,
    pub media_id: u32,
    pub clip_id: u32,
    pub generation: u64,
    pub request_id: u64,
    pub requested_source_tick: i64,
    pub output_size: [u32; 2],
}

#[derive(Clone, Debug, Serialize)]
struct AcceptedLayer {
    slot: usize,
    media_id: u32,
    clip_id: u32,
    generation: u64,
    request_id: u64,
    source_tick: i64,
    output_size: [u32; 2],
    backend: Option<String>,
    upload_serial: u64,
    input_to_upload_ms: f64,
}

#[derive(Clone, Debug, Serialize)]
struct Sample {
    index: usize,
    warmup: bool,
    playhead_tick: i64,
    expected_playhead_tick: i64,
    sequence_generation: u64,
    input_to_ui_cpu_ms: f64,
    full_cpu_frame_ms: f64,
    input_to_surface_submission_ms: f64,
    matching_layers_to_surface_ms: f64,
    paint_serial: u64,
    paint_serial_before_input: u64,
    targets: Vec<LayerTarget>,
    layers: Vec<AcceptedLayer>,
}

struct Pending {
    started: Instant,
    sample: Sample,
    first_surface_recorded: bool,
    accepted: [Option<AcceptedLayer>; MONITOR_LAYER_COUNT],
    last_observed: [Option<AcceptedLayer>; MONITOR_LAYER_COUNT],
}

/// Bounded opt-in evidence retained only in failed Phase 1 reports.
#[derive(Serialize)]
struct PendingFailureDiagnostics {
    pending_sample: Sample,
    last_observed_layers: [Option<AcceptedLayer>; MONITOR_LAYER_COUNT],
    accepted_layers: [Option<AcceptedLayer>; MONITOR_LAYER_COUNT],
    presentation: PresentationDiagnostics,
}

/// One backend input batch, captured before probe injection. Never retain keys, text,
/// clipboard contents, device identifiers, or screenshot payloads.
#[derive(Debug, Serialize)]
struct BackendInputObservation {
    elapsed_ms: f64,
    pending_sample_index: Option<usize>,
    focused: bool,
    event_count: usize,
    pointer_moves: usize,
    pointer_buttons: usize,
    relative_mouse_moves: usize,
    focus_events: usize,
    other_events: usize,
    last_pointer: Option<[f32; 2]>,
    playhead_before: i64,
    playhead_after: Option<i64>,
    scrubbing_before: bool,
    scrubbing_after: Option<bool>,
    injected_motion: bool,
}

impl BackendInputObservation {
    fn capture(
        input: &egui::RawInput,
        editor: &EditorState,
        elapsed_ms: f64,
        pending_sample_index: Option<usize>,
    ) -> Self {
        let mut observation = Self {
            elapsed_ms,
            pending_sample_index,
            focused: input.focused,
            event_count: input.events.len(),
            pointer_moves: 0,
            pointer_buttons: 0,
            relative_mouse_moves: 0,
            focus_events: 0,
            other_events: 0,
            last_pointer: None,
            playhead_before: editor.playhead.0,
            playhead_after: None,
            scrubbing_before: editor.is_scrubbing(),
            scrubbing_after: None,
            injected_motion: false,
        };
        for event in &input.events {
            match event {
                egui::Event::PointerMoved(point) => {
                    observation.pointer_moves += 1;
                    observation.last_pointer = Some([point.x, point.y]);
                }
                egui::Event::PointerButton { pos, .. } => {
                    observation.pointer_buttons += 1;
                    observation.last_pointer = Some([pos.x, pos.y]);
                }
                egui::Event::MouseMoved(_) => observation.relative_mouse_moves += 1,
                egui::Event::WindowFocused(_) => observation.focus_events += 1,
                _ => observation.other_events += 1,
            }
        }
        observation
    }
}

/// End-of-run snapshot of one monitor decoder's accumulated worker CPU spans.
/// Totals can overlap between layer workers and therefore are not wall-clock latency.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct DecoderLayerStageTimings {
    layer: usize,
    cache_lookup: DecoderStageTiming,
    demux_packet: DecoderStageTiming,
    decoder_calls: DecoderStageTiming,
    hardware_transfer: DecoderStageTiming,
    scaler: DecoderStageTiming,
    rgba_copy_letterbox: DecoderStageTiming,
    worker_request: DecoderStageTiming,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
struct DecoderStageTiming {
    samples: u64,
    total_nanos: u64,
    max_nanos: u64,
}

impl From<nle_decode::MonitorStageTiming> for DecoderStageTiming {
    fn from(timing: nle_decode::MonitorStageTiming) -> Self {
        Self {
            samples: timing.samples,
            total_nanos: timing.total_nanos,
            max_nanos: timing.max_nanos,
        }
    }
}

impl DecoderLayerStageTimings {
    fn from_snapshot(layer: usize, timings: nle_decode::MonitorDecoderStageTimings) -> Self {
        Self {
            layer,
            cache_lookup: timings.cache_lookup.into(),
            demux_packet: timings.demux_packet.into(),
            decoder_calls: timings.decoder_calls.into(),
            hardware_transfer: timings.hardware_transfer.into(),
            scaler: timings.scaler.into(),
            rgba_copy_letterbox: timings.rgba_copy_letterbox.into(),
            worker_request: timings.worker_request.into(),
        }
    }
}

#[derive(Serialize)]
struct PresentationDiagnostics {
    upload_serials: [u64; MONITOR_LAYER_COUNT],
    painted_upload_serials: [Option<u64>; MONITOR_LAYER_COUNT],
    paint_serial: u64,
}

impl From<nle_render::ViewerPresentationEvidence> for PresentationDiagnostics {
    fn from(evidence: nle_render::ViewerPresentationEvidence) -> Self {
        Self {
            upload_serials: evidence.upload_serials,
            painted_upload_serials: evidence.painted_upload_serials,
            paint_serial: evidence.paint_serial,
        }
    }
}

#[derive(Debug, Serialize)]
struct Distribution {
    samples: usize,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

fn distribution(values: impl Iterator<Item = f64>) -> Distribution {
    let mut values: Vec<_> = values.collect();
    values.sort_by(f64::total_cmp);
    let percentile = |percent: usize| {
        values
            .get(
                values
                    .len()
                    .saturating_mul(percent)
                    .div_ceil(100)
                    .saturating_sub(1),
            )
            .copied()
            .unwrap_or(0.0)
    };
    Distribution {
        samples: values.len(),
        p50_ms: percentile(50),
        p95_ms: percentile(95),
        max_ms: percentile(100),
    }
}

fn frame_matches(target: &LayerTarget, frame: &AcceptedLayer) -> bool {
    target.slot == frame.slot
        && target.media_id == frame.media_id
        && target.clip_id == frame.clip_id
        && target.generation == frame.generation
        && target.request_id == frame.request_id
        && target.output_size == [1920, 1080]
        && frame.output_size == target.output_size
        && nle_decode::source_tick_reaches_target(frame.source_tick, target.requested_source_tick)
        && frame
            .source_tick
            .saturating_sub(target.requested_source_tick)
            <= 33_334
        && frame.upload_serial != 0
}

pub(super) struct Probe {
    pub config: Configuration,
    started: Instant,
    armed: bool,
    pressed: bool,
    released: bool,
    pointer: egui::Pos2,
    pending: Option<Pending>,
    samples: Vec<Sample>,
    failure: Option<String>,
    report_sent: bool,
    last_paint_serial: u64,
    last_presentation: nle_render::ViewerPresentationEvidence,
    last_backend_input: Option<BackendInputObservation>,
    backend_input_this_frame: bool,
    timeline_generation: Option<u64>,
    tx: Option<mpsc::SyncSender<serde_json::Value>>,
    writer: Option<thread::JoinHandle<()>>,
    writer_done: Arc<AtomicBool>,
}

impl Probe {
    pub fn from_environment() -> Option<Self> {
        let path = std::env::var_os("MAELSTROM_PHASE1_UI_CONFIG")?;
        let load = || -> Result<Self, String> {
            let mut bytes = Vec::new();
            fs::File::open(path)
                .map_err(|e| e.to_string())?
                .take(65_537)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 65_536 {
                return Err("UI probe configuration is too large".into());
            }
            let config: Configuration =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            config.validate()?;
            if std::env::var("MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS")
                .ok()
                .as_deref()
                != Some(config.adapter_class.as_str())
            {
                return Err("probe and renderer adapter selections disagree".into());
            }
            Self::new(config)
        };
        Some(load().expect("invalid explicit Phase 1 UI probe configuration"))
    }

    fn new(config: Configuration) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<serde_json::Value>(1);
        let path = config.report_path.clone();
        let temporary = path.with_extension(format!("{}.tmp", config.run_id));
        let writer_done = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&writer_done);
        let writer = thread::Builder::new()
            .name("maelstrom-phase1-ui-report".into())
            .spawn(move || {
                if let Ok(report) = rx.recv() {
                    let write = || -> Result<(), Box<dyn std::error::Error>> {
                        let bytes = serde_json::to_vec_pretty(&report)?;
                        fs::write(&temporary, bytes)?;
                        fs::rename(&temporary, &path)?;
                        Ok(())
                    };
                    if let Err(error) = write() {
                        tracing::error!("Phase 1 UI report failed: {error}");
                        let _ = fs::remove_file(&temporary);
                    }
                }
                done.store(true, Ordering::Release);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            config,
            started: Instant::now(),
            armed: false,
            pressed: false,
            released: false,
            pointer: egui::Pos2::ZERO,
            pending: None,
            samples: Vec::with_capacity(TOTAL_SAMPLES),
            failure: None,
            report_sent: false,
            last_paint_serial: 0,
            last_presentation: Default::default(),
            last_backend_input: None,
            backend_input_this_frame: false,
            timeline_generation: None,
            tx: Some(tx),
            writer: Some(writer),
            writer_done,
        })
    }

    pub fn should_exit(&self) -> bool {
        self.writer_done.load(Ordering::Acquire)
    }

    // Called before context.run_ui. The injected events enter the ordinary egui ruler handler.
    pub fn inject_input(&mut self, input: &mut egui::RawInput, editor: &EditorState) -> bool {
        self.backend_input_this_frame = false;
        if self.report_sent || self.failure.is_some() || !self.armed {
            return false;
        }
        if self.pressed && !self.released && !input.events.is_empty() {
            self.last_backend_input = Some(BackendInputObservation::capture(
                input,
                editor,
                self.started.elapsed().as_secs_f64() * 1000.0,
                self.pending.as_ref().map(|pending| pending.sample.index),
            ));
            self.backend_input_this_frame = true;
        }
        let Some(geometry) = editor.timeline_scrub_geometry() else {
            return false;
        };
        if !self.pressed {
            self.pointer = geometry.handle_center;
            input.events.push(egui::Event::PointerMoved(self.pointer));
            input.events.push(egui::Event::PointerButton {
                pos: self.pointer,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            });
            self.pressed = true;
            return false;
        }
        if self.samples.len() == TOTAL_SAMPLES && !self.released {
            input.events.push(egui::Event::PointerButton {
                pos: self.pointer,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            });
            self.released = true;
            return false;
        }
        if self.pending.is_some() || self.released {
            return false;
        }
        if !editor.is_scrubbing() {
            self.failure = Some("ruler press did not acquire the real scrub gesture".into());
            return false;
        }
        // 48 distinct target fractions; alternate large forward/backward seeks within five seconds.
        let index = self.samples.len();
        let fraction = 0.08 + 0.84 * (1 + (index * 37 % 149)) as f32 / 150.0;
        self.pointer = egui::pos2(
            geometry.content.left() + geometry.content.width() * fraction,
            geometry.handle_center.y,
        );
        input.events.push(egui::Event::PointerMoved(self.pointer));
        self.pending = Some(Pending {
            started: Instant::now(),
            sample: Sample {
                index,
                warmup: index < WARMUP_SAMPLES,
                playhead_tick: 0,
                expected_playhead_tick: geometry.view_start.0
                    + (((self.pointer.x - geometry.content.left()) / geometry.content.width()
                        * geometry.view_span.0 as f32)
                        .round() as i64),
                sequence_generation: 0,
                input_to_ui_cpu_ms: 0.0,
                full_cpu_frame_ms: 0.0,
                input_to_surface_submission_ms: 0.0,
                matching_layers_to_surface_ms: 0.0,
                paint_serial: 0,
                paint_serial_before_input: self.last_paint_serial,
                targets: Vec::with_capacity(self.config.source_paths.len()),
                layers: Vec::with_capacity(self.config.source_paths.len()),
            },
            first_surface_recorded: false,
            accepted: std::array::from_fn(|_| None),
            last_observed: std::array::from_fn(|_| None),
        });
        true
    }

    pub fn after_ui(&mut self, editor: &EditorState, measured_input: bool) {
        if measured_input {
            self.ui_complete(editor);
        }
        if self.backend_input_this_frame
            && let Some(observation) = &mut self.last_backend_input
        {
            observation.playhead_after = Some(editor.playhead.0);
            observation.scrubbing_after = Some(editor.is_scrubbing());
            observation.injected_motion = measured_input;
        }
    }

    fn check_editor_state(&mut self, editor: &EditorState) {
        if self.failure.is_some() {
            return;
        }
        if self
            .timeline_generation
            .is_some_and(|generation| generation != editor.timeline.generation())
        {
            self.failure = Some("timeline changed during the fixed qualification workload".into());
        } else if let Some(pending) = &self.pending
            && pending.first_surface_recorded
            && (!editor.is_scrubbing() || editor.playhead.0 != pending.sample.playhead_tick)
        {
            self.failure =
                Some("ruler target changed while waiting for its matching layers".into());
        }
    }

    pub fn ui_complete(&mut self, editor: &EditorState) {
        if let Some(pending) = &mut self.pending {
            pending.sample.input_to_ui_cpu_ms = pending.started.elapsed().as_secs_f64() * 1000.0;
            pending.sample.playhead_tick = editor.playhead.0;
            pending.sample.sequence_generation = editor.timeline.generation();
            if !editor.is_scrubbing() || editor.playhead.0 != pending.sample.expected_playhead_tick
            {
                self.failure =
                    Some("injected ruler motion did not produce its expected playhead".into());
            }
        }
    }

    pub fn targets(&mut self, targets: Vec<LayerTarget>) {
        if let Some(pending) = &mut self.pending
            && pending.sample.targets.is_empty()
        {
            if targets.len() != self.config.source_paths.len()
                || targets.iter().any(|t| t.output_size != [1920, 1080])
            {
                self.failure =
                    Some("active source count or Full-1080p request dimensions differ".into());
            }
            pending.sample.targets = targets;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn decoded(
        &mut self,
        slot: usize,
        media_id: u32,
        clip_id: u32,
        generation: u64,
        request_id: u64,
        source_tick: i64,
        output_size: [u32; 2],
        backend: Option<nle_decode::DecodeBackend>,
        upload_serial: u64,
    ) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        let frame = AcceptedLayer {
            slot,
            media_id,
            clip_id,
            generation,
            request_id,
            source_tick,
            output_size,
            backend: backend.map(|b| b.display_name().to_owned()),
            upload_serial,
            input_to_upload_ms: pending.started.elapsed().as_secs_f64() * 1000.0,
        };
        if slot < MONITOR_LAYER_COUNT {
            pending.last_observed[slot] = Some(frame.clone());
        }
        if pending
            .sample
            .targets
            .iter()
            .any(|target| frame_matches(target, &frame))
        {
            pending.accepted[slot] = Some(frame);
        }
    }

    pub fn presented(
        &mut self,
        frame_cpu: Duration,
        evidence: nle_render::ViewerPresentationEvidence,
    ) {
        self.last_paint_serial = evidence.paint_serial;
        self.last_presentation = evidence;
        if self.report_sent || self.failure.is_some() {
            return;
        }
        if self.started.elapsed() > Duration::from_secs(150) {
            self.failure = Some("windowed probe exceeded 150 seconds".into());
            return;
        }
        let Some(pending) = &mut self.pending else {
            return;
        };
        if !pending.first_surface_recorded {
            pending.sample.full_cpu_frame_ms = frame_cpu.as_secs_f64() * 1000.0;
            pending.sample.input_to_surface_submission_ms =
                pending.started.elapsed().as_secs_f64() * 1000.0;
            pending.first_surface_recorded = true;
        }
        if pending.started.elapsed() > Duration::from_secs(5) {
            self.failure = Some(format!(
                "sample {} timed out waiting for exact native layer identities",
                pending.sample.index
            ));
            return;
        }
        if pending.sample.targets.len() != self.config.source_paths.len()
            || evidence.paint_serial <= pending.sample.paint_serial_before_input
        {
            return;
        }
        let all_painted = pending.sample.targets.iter().all(|target| {
            pending.accepted[target.slot]
                .as_ref()
                .is_some_and(|accepted| {
                    frame_matches(target, accepted)
                        && evidence.painted_upload_serials[target.slot]
                            == Some(accepted.upload_serial)
                })
        });
        if all_painted {
            let mut completed = self.pending.take().expect("checked pending sample");
            completed.sample.matching_layers_to_surface_ms =
                completed.started.elapsed().as_secs_f64() * 1000.0;
            completed.sample.paint_serial = evidence.paint_serial;
            completed
                .sample
                .layers
                .extend(completed.accepted.into_iter().flatten());
            self.samples.push(completed.sample);
        }
    }

    fn finish(&mut self, environment: serde_json::Value) {
        if self.report_sent {
            return;
        }
        let input = distribution(
            self.samples
                .iter()
                .filter(|s| !s.warmup)
                .map(|s| s.input_to_ui_cpu_ms),
        );
        let cpu = distribution(
            self.samples
                .iter()
                .filter(|s| !s.warmup)
                .map(|s| s.full_cpu_frame_ms),
        );
        let ready = distribution(
            self.samples
                .iter()
                .filter(|s| !s.warmup)
                .map(|s| s.matching_layers_to_surface_ms),
        );
        let complete =
            self.failure.is_none() && self.samples.len() == TOTAL_SAMPLES && self.released;
        let budgets_passed = complete && input.p95_ms <= 1.0 && cpu.p95_ms < 8.0;
        let pending_failure_diagnostics = self.failure.as_ref().and_then(|_| {
            self.pending
                .as_ref()
                .map(|pending| PendingFailureDiagnostics {
                    pending_sample: pending.sample.clone(),
                    last_observed_layers: pending.last_observed.clone(),
                    accepted_layers: pending.accepted.clone(),
                    presentation: self.last_presentation.into(),
                })
        });
        let mut report = serde_json::json!({ "schema_version":1, "run_id":self.config.run_id,
            "process_id":std::process::id(), "status":if complete {"completed"} else {"failed"},
            "failure":self.failure, "configuration":self.config, "warmup_samples":WARMUP_SAMPLES,
            "measured_samples":MEASURED_SAMPLES, "measurement_scope":"synthetic egui ruler input to UI CPU completion and matching native layers to surface submission; not physical input, GPU completion, or scanout",
            "cpu_budgets_passed":budgets_passed, "input_to_ui_cpu":input, "full_cpu_frame":cpu,
            "matching_layers_to_surface":ready, "environment":environment, "samples":self.samples });
        if self.failure.is_some() {
            report["last_backend_input"] = serde_json::to_value(&self.last_backend_input)
                .expect("backend input observation serializes");
            report["backend_input_scope"] = serde_json::json!(
                "Last nonempty egui-winit input batch during the controlled gesture, before probe injection; aggregate counts and pointer position only, not physical-device attribution."
            );
        }
        if let Some(diagnostics) = pending_failure_diagnostics {
            report["pending_failure_diagnostics"] =
                serde_json::to_value(diagnostics).expect("failure diagnostics serialize");
        }
        if let Some(tx) = self.tx.take() {
            let _ = tx.try_send(report);
        }
        self.report_sent = true;
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl App {
    pub(super) fn start_phase1_ui(&mut self) {
        let Some(probe) = &self.phase1_ui_probe else {
            return;
        };
        let paths = probe.config.source_paths.clone();
        self.show_editor_screen(
            "Four-source UI qualification".into(),
            Language::English,
            None,
            ProjectSettings::default(),
            true,
        );
        self.editor.set_project_canvas_size(1920, 1080);
        self.editor.set_preview_quality(PreviewQuality::Full);
        self.editor.set_paused_preview_quality(PreviewQuality::Full);
        self.editor.timeline_view_start = nle_timeline::Tick(0);
        self.editor.timeline_view_span = nle_timeline::Tick(5_000_000);
        self.editor.set_playhead(nle_timeline::Tick(500_000));
        self.add_media_paths(paths.clone());
        let mut tracks: Vec<_> = self
            .editor
            .timeline
            .tracks
            .iter()
            .filter(|t| t.kind == nle_timeline::TrackKind::Video)
            .map(|t| t.id)
            .collect();
        while tracks.len() < paths.len() {
            tracks.push(
                self.editor
                    .timeline
                    .add_track(nle_timeline::TrackKind::Video),
            );
        }
        for (index, (track, path)) in tracks.into_iter().zip(paths.iter()).enumerate() {
            let media_id = self.editor.media[index].id;
            let clip = self
                .editor
                .timeline
                .insert_clip(
                    track,
                    nle_timeline::MediaId(media_id),
                    nle_timeline::Tick(0),
                    nle_timeline::Tick(5_000_000),
                    nle_timeline::Tick(0),
                )
                .expect("qualification fixture clip");
            if paths.len() == 4 {
                self.editor
                    .timeline
                    .set_clip_transform(
                        clip,
                        nle_timeline::ClipTransform {
                            scale_x: 0.5,
                            scale_y: 0.5,
                            pos_x: if index % 2 == 0 { -0.5 } else { 0.5 },
                            pos_y: if index < 2 { -0.5 } else { 0.5 },
                            ..Default::default()
                        },
                    )
                    .expect("qualification quadrant transform");
            }
            self.request_media_analysis(media_id, path.clone());
        }
        if let Some(window) = &self.window {
            window.set_maximized(false);
            let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(1920, 1080));
            window.set_window_level(WindowLevel::AlwaysOnTop);
            window.request_redraw();
        }
    }

    pub(super) fn capture_phase1_ui_targets(&mut self) {
        if !self.phase1_ui_probe.as_ref().is_some_and(|p| {
            p.pending
                .as_ref()
                .is_some_and(|pending| pending.sample.targets.is_empty())
        }) {
            return;
        }
        let targets = self
            .editor
            .playback_targets()
            .enumerate()
            .filter_map(|(slot, source)| {
                self.monitor_last_requests[slot].map(|request| LayerTarget {
                    slot,
                    media_id: source.media_id,
                    clip_id: source.clip_id.0,
                    generation: self.monitor_generations[slot],
                    request_id: self.monitor_latest_request_ids[slot],
                    requested_source_tick: request.source_tick,
                    output_size: [request.width, request.height],
                })
            })
            .collect();
        self.phase1_ui_probe
            .as_mut()
            .expect("checked probe")
            .targets(targets);
    }

    pub(super) fn advance_phase1_ui(&mut self, frame_cpu: Duration) {
        let Some(mut probe) = self.phase1_ui_probe.take() else {
            return;
        };
        if !probe.armed
            && self.media_analysis_pending.is_empty()
            && self.media_analysis_in_flight.is_empty()
            && self.hardware_profile.is_some()
            && self.startup_resources_ready
        {
            let sources: Vec<_> = self.editor.playback_targets().collect();
            let valid = sources.len() == probe.config.source_paths.len()
                && sources
                    .iter()
                    .all(|source| source.source_size == Some((1920, 1080)));
            if valid {
                probe.armed = true;
                probe.timeline_generation = Some(self.editor.timeline.generation());
            } else {
                probe.failure = Some("source analysis did not verify all Full-1080p inputs".into());
            }
        }
        probe.check_editor_state(&self.editor);
        let evidence = self
            .hub_renderer
            .as_ref()
            .map(|r| r.viewer_presentation_evidence())
            .unwrap_or_default();
        probe.presented(frame_cpu, evidence);
        if probe.failure.is_some() || (probe.samples.len() == TOTAL_SAMPLES && probe.released) {
            let renderer = self.renderer_report.as_ref();
            let resources = self.monitor_session_pool.diagnostics();
            let cache = self.monitor_frame_cache_pool.diagnostics();
            // This is intentionally captured only while finishing the opt-in probe. The
            // counters accumulate from decoder creation, and independent workers can overlap.
            let decoder_stage_timings: Vec<_> = (0..MONITOR_LAYER_COUNT)
                .map(|layer| {
                    DecoderLayerStageTimings::from_snapshot(
                        layer,
                        self.monitor_decoders[layer].stage_timings(),
                    )
                })
                .collect();
            let gpu = self.hub_renderer.as_ref().map(|renderer| {
                GpuStageTimingsReport::from_snapshots(
                    renderer.viewer_compositor_gpu_timing(),
                    renderer.gpu_submission_completion_timing(),
                )
            });
            let mut environment = serde_json::json!({
                "renderer_name":renderer.map(|r| &r.name), "renderer_device_type":renderer.map(|r| &r.device_type),
                "renderer_backend":renderer.map(|r| &r.backend), "driver":renderer.map(|r| &r.driver), "driver_info":renderer.map(|r| &r.driver_info),
                "cpu_identity":self.machine_profile.cpu_identity, "logical_cpu_count":self.machine_profile.logical_cpu_count,
                "total_physical_memory_bytes":self.machine_profile.total_physical_memory_bytes,
                "surface_size":self.surface_config.as_ref().map(|c| [c.width,c.height]),
                "display_refresh_millihertz":self.window.as_ref().and_then(|w| w.current_monitor()).and_then(|m| m.refresh_rate_millihertz()),
                "decoder_backends":self.observed_decoder_backends, "encoder_backend":null,
                "preview_quality":"Full", "requested_output_size":[1920,1080],
                "cache_bytes":cache.current_bytes, "cache_peak_bytes":cache.peak_bytes, "cache_cap_bytes":self.monitor_cache_cap_bytes,
                "active_sessions":resources.active_sticky_sessions, "peak_sessions":resources.peak_sticky_sessions, "session_cap":resources.session_cap,
                "decoder_stage_timings_scope":"Accumulated CPU call-boundary durations since the start of each monitor decoder lifetime; per-layer worker time can overlap and is not wall-clock latency.",
                "decoder_stage_timings":decoder_stage_timings,
                "viewer_upload_timing_scope":"Rolling CPU/API submission timing; excludes GPU completion and scanout.",
                "viewer_upload_timing":self.viewer_upload_timings.snapshot(),
                "gpu_stage_timings":gpu,
                "runtime_diagnostics":RuntimeDiagnosticsReport::from(self.runtime_diagnostics())
            });
            if probe.failure.is_some() {
                let monitor_requests: Vec<_> = (0..MONITOR_LAYER_COUNT)
                    .map(|slot| {
                        let key = self.monitor_last_requests[slot];
                        serde_json::json!({
                            "slot":slot,
                            "latest_request_id":self.monitor_latest_request_ids[slot],
                            "generation":self.monitor_generations[slot],
                            "key":key.map(|key| serde_json::json!({
                                "source_tick":key.source_tick,
                                "width":key.width,
                                "height":key.height
                            })),
                            "in_flight":self.monitor_requests_in_flight[slot],
                            "deferred":self.monitor_request_deferred[slot]
                        })
                    })
                    .collect();
                environment["failure_current_monitor"] = serde_json::json!({
                    "playhead_tick":self.editor.playhead.0,
                    "requests":monitor_requests
                });
            }
            probe.finish(environment);
        }
        self.phase1_ui_probe = Some(probe);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TemporaryDirectory(PathBuf);
    impl TemporaryDirectory {
        fn new() -> Self {
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "maelstrom-phase1-ui-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self, count: usize) -> Configuration {
            let source_paths = (0..count)
                .map(|index| {
                    let path = self.0.join(format!("source-{index}.mp4"));
                    fs::write(&path, b"configuration fixture only").unwrap();
                    path
                })
                .collect();
            Configuration {
                schema_version: 1,
                run_id: "headless-regression".into(),
                source_paths,
                report_path: self.0.join("report.json"),
                adapter_class: "DiscreteGpu".into(),
            }
        }
    }
    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn input_frame(context: &egui::Context, editor: &mut EditorState, probe: &mut Probe) -> bool {
        input_frame_with_events(context, editor, probe, Vec::new())
    }

    fn input_frame_with_events(
        context: &egui::Context,
        editor: &mut EditorState,
        probe: &mut Probe,
        events: Vec<egui::Event>,
    ) -> bool {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1920.0, 1080.0),
            )),
            events,
            ..Default::default()
        };
        let measured = probe.inject_input(&mut input, editor);
        let _ = context.run_ui(input, |ui| nle_ui_core::show_editor(ui, editor));
        probe.after_ui(editor, measured);
        probe.check_editor_state(editor);
        measured
    }

    #[test]
    fn phase1_ui_backend_motion_during_wait_preserves_failed_sample_evidence() {
        let directory = TemporaryDirectory::new();
        let config = directory.config(1);
        let mut probe = Probe::new(config.clone()).unwrap();
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Backend input regression");
        editor.add_media_paths(config.source_paths.clone());
        let track = editor.timeline.add_track(nle_timeline::TrackKind::Video);
        editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(5_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        editor.timeline_view_span = nle_timeline::Tick(5_000_000);
        editor.set_playhead(nle_timeline::Tick(500_000));
        input_frame(&context, &mut editor, &mut probe);
        probe.armed = true;
        assert!(!input_frame(&context, &mut editor, &mut probe));
        assert!(input_frame(&context, &mut editor, &mut probe));
        let target_tick = editor.playhead.0;
        probe.targets(vec![LayerTarget {
            slot: 0,
            media_id: 1,
            clip_id: 1,
            generation: 3,
            request_id: 1,
            requested_source_tick: target_tick,
            output_size: [1920, 1080],
        }]);
        probe.decoded(0, 1, 1, 3, 1, target_tick, [1920, 1080], None, 1);
        probe.presented(Duration::from_micros(500), Default::default());
        assert!(probe.failure.is_none());
        let geometry = editor.timeline_scrub_geometry().unwrap();
        let foreign_pointer = egui::pos2(
            geometry.content.left() + geometry.content.width() * 0.15,
            geometry.handle_center.y,
        );
        assert!(!input_frame_with_events(
            &context,
            &mut editor,
            &mut probe,
            vec![egui::Event::PointerMoved(foreign_pointer)]
        ));
        assert_ne!(editor.playhead.0, target_tick);
        assert!(
            probe
                .failure
                .as_ref()
                .unwrap()
                .contains("ruler target changed")
        );
        // Even an eligible paint arriving on the failure frame must not consume the failed
        // sample or erase the evidence needed to explain the unscripted movement.
        probe.presented(
            Duration::from_micros(500),
            nle_render::ViewerPresentationEvidence {
                upload_serials: [1, 0, 0, 0],
                painted_upload_serials: [Some(1), None, None, None],
                paint_serial: 1,
            },
        );
        assert!(probe.samples.is_empty());
        assert!(probe.pending.is_some());
        probe.finish(serde_json::json!({}));
        drop(probe);
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(config.report_path).unwrap()).unwrap();
        assert_eq!(report["status"], "failed");
        assert_eq!(report["cpu_budgets_passed"], false);
        assert_eq!(
            report["pending_failure_diagnostics"]["pending_sample"]["playhead_tick"],
            target_tick
        );
        let observation = &report["last_backend_input"];
        assert_eq!(observation["pointer_moves"], 1);
        assert_eq!(observation["playhead_before"], target_tick);
        assert_eq!(observation["playhead_after"], editor.playhead.0);
        assert_eq!(observation["injected_motion"], false);
        assert_eq!(observation["pending_sample_index"], 0);
    }

    #[test]
    fn phase1_ui_backend_input_summary_is_bounded_and_omits_private_payloads() {
        let editor = EditorState::new(Language::English, "Input summary privacy");
        let input = egui::RawInput {
            events: vec![egui::Event::Paste("private clipboard sentinel".into()); 10_000],
            ..Default::default()
        };
        let observation = BackendInputObservation::capture(&input, &editor, 0.0, Some(0));
        let encoded = serde_json::to_string(&observation).unwrap();
        assert!(encoded.len() < 512);
        assert!(!encoded.contains("private clipboard sentinel"));
        assert_eq!(observation.event_count, 10_000);
        assert_eq!(observation.other_events, 10_000);
    }

    #[test]
    fn phase1_ui_configuration_rejects_reused_reports_and_aliased_sources() {
        let directory = TemporaryDirectory::new();
        let mut config = directory.config(4);
        assert!(config.validate().is_ok());
        config.source_paths[3] = config.source_paths[0].clone();
        assert!(config.validate().is_err());
        config.source_paths.truncate(1);
        fs::write(&config.report_path, b"historical report").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn phase1_ui_real_ruler_input_waits_for_each_exact_painted_upload() {
        let directory = TemporaryDirectory::new();
        let config = directory.config(4);
        let mut probe = Probe::new(config.clone()).unwrap();
        let context = egui::Context::default();
        let mut editor = EditorState::new(Language::English, "Probe gesture regression");
        editor.add_media_paths(config.source_paths.clone());
        let track = editor.timeline.add_track(nle_timeline::TrackKind::Video);
        editor
            .timeline
            .insert_clip(
                track,
                nle_timeline::MediaId(1),
                nle_timeline::Tick(0),
                nle_timeline::Tick(5_000_000),
                nle_timeline::Tick(0),
            )
            .unwrap();
        editor.timeline_view_span = nle_timeline::Tick(5_000_000);
        editor.set_playhead(nle_timeline::Tick(500_000));
        input_frame(&context, &mut editor, &mut probe);
        probe.armed = true;
        assert!(!input_frame(&context, &mut editor, &mut probe));
        assert!(editor.is_scrubbing());
        for index in 0..TOTAL_SAMPLES {
            assert!(input_frame(&context, &mut editor, &mut probe));
            assert!(probe.failure.is_none(), "{:?}", probe.failure);
            let requested_tick = editor.playhead.0;
            probe.targets(
                (0..4)
                    .map(|slot| LayerTarget {
                        slot,
                        media_id: slot as u32 + 1,
                        clip_id: slot as u32 + 1,
                        generation: 3,
                        request_id: index as u64 + 1,
                        requested_source_tick: requested_tick,
                        output_size: [1920, 1080],
                    })
                    .collect(),
            );
            for slot in 0..4 {
                probe.decoded(
                    slot,
                    slot as u32 + 1,
                    slot as u32 + 1,
                    3,
                    index as u64 + 1,
                    requested_tick,
                    [1920, 1080],
                    None,
                    (index * 4 + slot + 1) as u64,
                );
            }
            let mut evidence = nle_render::ViewerPresentationEvidence {
                paint_serial: index as u64 + 1,
                painted_upload_serials: std::array::from_fn(|slot| {
                    Some((index * 4 + slot + 1) as u64)
                }),
                ..Default::default()
            };
            // A previous/omitted fourth layer must not complete a multi-source sample.
            evidence.painted_upload_serials[3] = Some(0);
            probe.presented(Duration::from_micros(500), evidence);
            assert_eq!(probe.samples.len(), index);
            evidence.painted_upload_serials[3] = Some((index * 4 + 4) as u64);
            probe.presented(Duration::from_micros(500), evidence);
            assert_eq!(probe.samples.len(), index + 1);
        }
        assert!(!input_frame(&context, &mut editor, &mut probe));
        assert!(!editor.is_scrubbing());
        probe.finish(serde_json::json!({"test_only":true}));
        drop(probe); // Joins the bounded report writer before inspecting output.
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(config.report_path).unwrap()).unwrap();
        assert_eq!(report["status"], "completed");
        assert_eq!(report["samples"].as_array().unwrap().len(), TOTAL_SAMPLES);
        assert_eq!(report["input_to_ui_cpu"]["samples"], MEASURED_SAMPLES);
        assert!(
            report["samples"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["playhead_tick"] == s["expected_playhead_tick"])
        );
    }

    #[test]
    fn phase1_ui_timeout_preserves_failure_and_joins_writer() {
        let directory = TemporaryDirectory::new();
        let config = directory.config(1);
        let mut probe = Probe::new(config.clone()).unwrap();
        probe.started = Instant::now() - Duration::from_secs(151);
        probe.presented(Duration::ZERO, Default::default());
        assert!(probe.failure.is_some());
        probe.finish(serde_json::json!({}));
        drop(probe);
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(config.report_path).unwrap()).unwrap();
        assert_eq!(report["status"], "failed");
        assert_eq!(report["cpu_budgets_passed"], false);
        assert!(report["failure"].as_str().unwrap().contains("150 seconds"));
    }

    #[test]
    fn phase1_ui_failed_report_preserves_pending_mismatch_without_completing_sample() {
        let directory = TemporaryDirectory::new();
        let config = directory.config(1);
        let mut probe = Probe::new(config.clone()).unwrap();
        let target = LayerTarget {
            slot: 0,
            media_id: 1,
            clip_id: 2,
            generation: 3,
            request_id: 4,
            requested_source_tick: 5_000,
            output_size: [1920, 1080],
        };
        probe.pending = Some(Pending {
            started: Instant::now() - Duration::from_secs(6),
            sample: Sample {
                index: 0,
                warmup: true,
                playhead_tick: 5_000,
                expected_playhead_tick: 5_000,
                sequence_generation: 3,
                input_to_ui_cpu_ms: 1.0,
                full_cpu_frame_ms: 2.0,
                input_to_surface_submission_ms: 3.0,
                matching_layers_to_surface_ms: 0.0,
                paint_serial: 0,
                paint_serial_before_input: 0,
                targets: vec![target],
                layers: Vec::new(),
            },
            first_surface_recorded: true,
            accepted: std::array::from_fn(|_| None),
            last_observed: std::array::from_fn(|_| None),
        });
        probe.decoded(0, 99, 2, 3, 4, 5_000, [1920, 1080], None, 42);
        probe.presented(
            Duration::from_micros(500),
            nle_render::ViewerPresentationEvidence {
                upload_serials: [42, 0, 0, 0],
                painted_upload_serials: [Some(42), None, None, None],
                paint_serial: 19,
            },
        );
        assert!(
            probe
                .failure
                .as_ref()
                .is_some_and(|f| f.contains("sample 0 timed out"))
        );
        assert!(probe.samples.is_empty());
        probe.finish(serde_json::json!({}));
        drop(probe);

        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(config.report_path).unwrap()).unwrap();
        assert_eq!(report["status"], "failed");
        assert!(report["samples"].as_array().unwrap().is_empty());
        assert_eq!(report["input_to_ui_cpu"]["samples"], 0);
        let diagnostics = &report["pending_failure_diagnostics"];
        assert_eq!(diagnostics["pending_sample"]["targets"][0]["media_id"], 1);
        assert_eq!(diagnostics["last_observed_layers"][0]["media_id"], 99);
        assert!(diagnostics["accepted_layers"][0].is_null());
        assert_eq!(diagnostics["presentation"]["paint_serial"], 19);
        assert_eq!(diagnostics["presentation"]["painted_upload_serials"][0], 42);
    }

    #[test]
    fn phase1_ui_layer_evidence_requires_exact_identity_and_full_output() {
        let target = LayerTarget {
            slot: 0,
            media_id: 1,
            clip_id: 1,
            generation: 7,
            request_id: 9,
            requested_source_tick: 1_000_000,
            output_size: [1920, 1080],
        };
        let mut frame = AcceptedLayer {
            slot: 0,
            media_id: 1,
            clip_id: 1,
            generation: 7,
            request_id: 9,
            source_tick: 1_000_000,
            output_size: [1920, 1080],
            backend: None,
            upload_serial: 12,
            input_to_upload_ms: 1.0,
        };
        assert!(frame_matches(&target, &frame));
        frame.generation = 6;
        assert!(!frame_matches(&target, &frame));
        frame.generation = 7;
        frame.request_id = 8;
        assert!(!frame_matches(&target, &frame));
        frame.request_id = 9;
        frame.media_id = 2;
        assert!(!frame_matches(&target, &frame));
        frame.media_id = 1;
        frame.clip_id = 2;
        assert!(!frame_matches(&target, &frame));
        frame.clip_id = 1;
        frame.output_size = [640, 360];
        assert!(!frame_matches(&target, &frame));
        frame.output_size = [1920, 1080];
        // The decoder rounds FFmpeg PTS while source-frame requests round upward.
        frame.source_tick = 999_999;
        assert!(frame_matches(&target, &frame));
        frame.source_tick = 999_998;
        assert!(!frame_matches(&target, &frame));
        frame.source_tick = 1_033_335;
        assert!(!frame_matches(&target, &frame));
        frame.source_tick = 1_033_334;
        assert!(frame_matches(&target, &frame));
        frame.upload_serial = 0;
        assert!(!frame_matches(&target, &frame));
    }

    #[test]
    fn phase1_ui_percentiles_use_nearest_rank() {
        let d = distribution((1..=40).map(f64::from));
        assert_eq!(
            (d.samples, d.p50_ms, d.p95_ms, d.max_ms),
            (40, 20.0, 38.0, 40.0)
        );
        let ticks: HashSet<_> = (0..TOTAL_SAMPLES)
            .map(|index| 1 + index * 37 % 149)
            .collect();
        assert_eq!(ticks.len(), TOTAL_SAMPLES);
    }

    #[test]
    fn phase1_ui_decoder_stage_snapshot_serializes_raw_accumulated_counters() {
        let snapshot = nle_decode::MonitorDecoderStageTimings {
            cache_lookup: nle_decode::MonitorStageTiming {
                samples: 1,
                total_nanos: 2,
                max_nanos: 3,
            },
            demux_packet: nle_decode::MonitorStageTiming {
                samples: 4,
                total_nanos: 5,
                max_nanos: 6,
            },
            decoder_calls: nle_decode::MonitorStageTiming {
                samples: 7,
                total_nanos: 8,
                max_nanos: 9,
            },
            hardware_transfer: nle_decode::MonitorStageTiming {
                samples: 10,
                total_nanos: 11,
                max_nanos: 12,
            },
            scaler: nle_decode::MonitorStageTiming {
                samples: 13,
                total_nanos: 14,
                max_nanos: 15,
            },
            rgba_copy_letterbox: nle_decode::MonitorStageTiming {
                samples: 16,
                total_nanos: 17,
                max_nanos: 18,
            },
            worker_request: nle_decode::MonitorStageTiming {
                samples: 19,
                total_nanos: 20,
                max_nanos: 21,
            },
        };

        let value = serde_json::to_value(DecoderLayerStageTimings::from_snapshot(2, snapshot))
            .expect("stage snapshot serializes");
        assert_eq!(value["layer"], 2);
        for (stage, counters) in [
            ("cache_lookup", [1, 2, 3]),
            ("demux_packet", [4, 5, 6]),
            ("decoder_calls", [7, 8, 9]),
            ("hardware_transfer", [10, 11, 12]),
            ("scaler", [13, 14, 15]),
            ("rgba_copy_letterbox", [16, 17, 18]),
            ("worker_request", [19, 20, 21]),
        ] {
            assert_eq!(value[stage]["samples"], counters[0]);
            assert_eq!(value[stage]["total_nanos"], counters[1]);
            assert_eq!(value[stage]["max_nanos"], counters[2]);
        }
    }
}
