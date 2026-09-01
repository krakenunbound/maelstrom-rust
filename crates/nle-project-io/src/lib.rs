//! Portable, versioned project documents and crash-safe writes.
//!
//! This crate owns filesystem I/O so the UI and timeline remain storage-free.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use nle_ui_core::{EditorProjectSnapshot, EditorState, Language, MediaId};
use serde::{Deserialize, Serialize};

pub const PROJECT_DOCUMENT_VERSION: u32 = 8;
pub const PROJECT_TIMEBASE: u32 = 1_000_000;

#[derive(Deserialize)]
struct ProjectDocumentHeader {
    version: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSettings {
    pub fps: [u32; 2],
    pub size: [u32; 2],
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            fps: [30, 1],
            size: [1920, 1080],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaReference {
    pub id: MediaId,
    pub absolute: PathBuf,
    pub relative: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDocument {
    pub version: u32,
    #[serde(default)]
    pub project_name: String,
    #[serde(default = "default_timebase")]
    pub timebase: u32,
    #[serde(default = "default_fps")]
    pub fps: [u32; 2],
    #[serde(default = "default_size")]
    pub size: [u32; 2],
    #[serde(default)]
    pub media: Vec<MediaReference>,
    pub snapshot: EditorProjectSnapshot,
}

fn default_timebase() -> u32 {
    PROJECT_TIMEBASE
}

fn default_fps() -> [u32; 2] {
    ProjectSettings::default().fps
}

fn default_size() -> [u32; 2] {
    ProjectSettings::default().size
}

#[derive(Debug)]
pub enum ProjectIoError {
    Io(io::Error),
    Json(serde_json::Error),
    UnsupportedVersion(u32),
    InvalidTimebase(u32),
    InvalidFrameRate([u32; 2]),
    InvalidSize([u32; 2]),
    InvalidMediaReferences,
    InvalidSnapshot(String),
}

impl std::fmt::Display for ProjectIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "invalid project document: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported project document version {version}")
            }
            Self::InvalidTimebase(value) => write!(f, "invalid project timebase {value}"),
            Self::InvalidFrameRate(value) => write!(f, "invalid project frame rate {value:?}"),
            Self::InvalidSize(value) => write!(f, "invalid project size {value:?}"),
            Self::InvalidMediaReferences => write!(f, "project media references do not match"),
            Self::InvalidSnapshot(error) => write!(f, "invalid editor snapshot: {error}"),
        }
    }
}

impl std::error::Error for ProjectIoError {}

impl From<io::Error> for ProjectIoError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ProjectIoError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub fn document_for_path(
    project_path: &Path,
    project_name: impl Into<String>,
    snapshot: EditorProjectSnapshot,
    settings: ProjectSettings,
) -> ProjectDocument {
    let project_dir = project_path.parent().unwrap_or_else(|| Path::new("."));
    let media = snapshot
        .media
        .iter()
        .map(|item| {
            let absolute = absolute_path(&item.path);
            let relative = if item.path.is_relative() {
                Some(item.path.clone())
            } else {
                absolute
                    .strip_prefix(project_dir)
                    .ok()
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(Path::to_path_buf)
            };
            MediaReference {
                id: item.id,
                absolute,
                relative,
            }
        })
        .collect();
    ProjectDocument {
        version: PROJECT_DOCUMENT_VERSION,
        project_name: project_name.into(),
        timebase: PROJECT_TIMEBASE,
        fps: settings.fps,
        size: settings.size,
        media,
        snapshot,
    }
}

pub fn write_document(path: &Path, document: &ProjectDocument) -> Result<(), ProjectIoError> {
    validate_document(document)?;
    let bytes = serde_json::to_vec_pretty(document)?;
    atomic_write(path, &bytes)?;
    Ok(())
}

pub fn read_document(path: &Path) -> Result<Option<ProjectDocument>, ProjectIoError> {
    let mut found = false;
    let mut last_error = None;
    for candidate in [path.to_path_buf(), backup_path(path)] {
        match fs::read(&candidate) {
            Ok(bytes) => {
                found = true;
                match preflight_document_version(&bytes)
                    .and_then(|()| {
                        serde_json::from_slice::<ProjectDocument>(&bytes)
                            .map_err(ProjectIoError::from)
                    })
                    .and_then(migrate_document)
                    .and_then(|mut document| {
                        resolve_media_paths(path, &mut document)?;
                        validate_document(&document)?;
                        Ok(document)
                    }) {
                    Ok(document) => return Ok(Some(document)),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if found {
        Err(last_error.unwrap_or(ProjectIoError::InvalidMediaReferences))
    } else {
        Ok(None)
    }
}

/// Reject unsupported formats before parsing nested data whose schema may be newer than this
/// build. Earlier versions remain readable and are migrated after complete parsing.
fn preflight_document_version(bytes: &[u8]) -> Result<(), ProjectIoError> {
    let header: ProjectDocumentHeader = serde_json::from_slice(bytes)?;
    if !(1..=PROJECT_DOCUMENT_VERSION).contains(&header.version) {
        return Err(ProjectIoError::UnsupportedVersion(header.version));
    }
    Ok(())
}

/// Versions 1–3 share this build's original durable fields; version 4 adds title overlays,
/// version 5 adds video transitions, version 6 adds their kind, version 7 adds audio transitions,
/// and version 8 writes clip video effects as explicit schema-v1 graphs. Legacy effect arrays are
/// accepted and converted during deserialization, so updating the header is lossless.
fn migrate_document(mut document: ProjectDocument) -> Result<ProjectDocument, ProjectIoError> {
    match document.version {
        1..=7 => document.version = PROJECT_DOCUMENT_VERSION,
        PROJECT_DOCUMENT_VERSION => {}
        version => return Err(ProjectIoError::UnsupportedVersion(version)),
    }
    Ok(document)
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_owned();
    backup.push(".bak");
    PathBuf::from(backup)
}

fn validate_document(document: &ProjectDocument) -> Result<(), ProjectIoError> {
    if document.version != PROJECT_DOCUMENT_VERSION {
        return Err(ProjectIoError::UnsupportedVersion(document.version));
    }
    if document.timebase == 0 {
        return Err(ProjectIoError::InvalidTimebase(document.timebase));
    }
    if document.fps[0] == 0 || document.fps[1] == 0 {
        return Err(ProjectIoError::InvalidFrameRate(document.fps));
    }
    if document.size[0] == 0 || document.size[1] == 0 {
        return Err(ProjectIoError::InvalidSize(document.size));
    }
    if !document.media.is_empty()
        && (document.media.len() != document.snapshot.media.len()
            || document
                .media
                .iter()
                .zip(&document.snapshot.media)
                .any(|(reference, item)| reference.id != item.id))
    {
        return Err(ProjectIoError::InvalidMediaReferences);
    }
    EditorState::restore(
        Language::English,
        "Project validation",
        document.snapshot.clone(),
    )
    .map_err(|error| ProjectIoError::InvalidSnapshot(error.to_string()))?;
    Ok(())
}

fn resolve_media_paths(path: &Path, document: &mut ProjectDocument) -> Result<(), ProjectIoError> {
    if document.media.is_empty() {
        // Version-1 app-data documents written before portable references were introduced.
        return Ok(());
    }
    if document.media.len() != document.snapshot.media.len() {
        return Err(ProjectIoError::InvalidMediaReferences);
    }
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for (reference, item) in document.media.iter().zip(&mut document.snapshot.media) {
        if reference.id != item.id {
            return Err(ProjectIoError::InvalidMediaReferences);
        }
        let relative = reference
            .relative
            .as_ref()
            .map(|value| project_dir.join(value));
        item.path = relative
            .filter(|value| value.exists())
            .unwrap_or_else(|| reference.absolute.clone());
    }
    Ok(())
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}-{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        if path.exists() {
            fs::copy(path, backup_path(path))?;
        }
        replace_file(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
/// Atomically promotes a fully written sibling file over its destination. Callers retain the
/// destination unchanged until this final commit boundary.
pub fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers for the duration of the call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
/// Atomically promotes a fully written sibling file over its destination. Callers retain the
/// destination unchanged until this final commit boundary.
pub fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_timeline::{
        AudioTransitionKind, MediaId as TimelineMediaId, Tick, TrackKind, VideoTransitionKind,
    };
    use nle_ui_core::EditorState;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "maelstrom-project-io-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn current_documents_are_written_at_version_eight() {
        let root = test_root("version-eight");
        let path = root.join("VersionEight.nleproj");
        let editor = EditorState::new(Language::English, "Version eight");
        let document = document_for_path(
            &path,
            "Version eight",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        assert_eq!(document.version, PROJECT_DOCUMENT_VERSION);
        write_document(&path, &document).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["version"], PROJECT_DOCUMENT_VERSION);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_six_round_trips_native_dip_to_black() {
        let root = test_root("transition-v6");
        let path = root.join("Transition.nleproj");
        let mut editor = EditorState::new(Language::English, "Transition");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition_of_kind(
                track,
                left,
                right,
                Tick(1_000_000),
                0.35,
                VideoTransitionKind::DipToBlack,
            )
            .unwrap();
        let document = document_for_path(
            &path,
            "Transition",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        write_document(&path, &document).unwrap();
        let restored = read_document(&path).unwrap().unwrap();
        assert_eq!(restored.version, PROJECT_DOCUMENT_VERSION);
        assert_eq!(restored.snapshot.timeline.transitions.len(), 1);
        let transition = &restored.snapshot.timeline.transitions[0];
        assert_eq!((transition.left_clip, transition.right_clip), (left, right));
        assert_eq!(transition.duration, Tick(1_000_000));
        assert_eq!(transition.curve, 0.35);
        assert_eq!(transition.kind, VideoTransitionKind::DipToBlack);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_seven_round_trips_equal_power_audio_crossfade() {
        let root = test_root("audio-transition-v7");
        let path = root.join("AudioTransition.nleproj");
        let mut editor = EditorState::new(Language::English, "Audio transition");
        editor.add_media_paths([PathBuf::from("left.wav"), PathBuf::from("right.wav")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, TimelineMediaId(1), Tick(0), Tick(2_000_000), Tick(0))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(0),
            )
            .unwrap();
        editor
            .timeline
            .add_audio_transition(track, left, right, Tick(1_000_000))
            .unwrap();
        let document = document_for_path(
            &path,
            "Audio transition",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        write_document(&path, &document).unwrap();
        let restored = read_document(&path).unwrap().unwrap();
        assert_eq!(restored.version, PROJECT_DOCUMENT_VERSION);
        let transition = &restored.snapshot.timeline.audio_transitions[0];
        assert_eq!((transition.left_clip, transition.right_clip), (left, right));
        assert_eq!(transition.duration, Tick(1_000_000));
        assert_eq!(transition.kind, AudioTransitionKind::EqualPowerCrossfade);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_six_without_audio_transitions_defaults_to_an_empty_collection() {
        let root = test_root("audio-transition-v6-default");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("LegacyAudioTransition.nleproj");
        let editor = EditorState::new(Language::English, "Legacy audio transition");
        let mut legacy = document_for_path(
            &path,
            "Legacy audio transition",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        legacy.version = 6;
        let mut legacy_value = serde_json::to_value(&legacy).unwrap();
        legacy_value["snapshot"]["timeline"]
            .as_object_mut()
            .unwrap()
            .remove("audio_transitions");
        fs::write(&path, serde_json::to_vec_pretty(&legacy_value).unwrap()).unwrap();

        let loaded = read_document(&path).unwrap().unwrap();
        assert_eq!(loaded.version, PROJECT_DOCUMENT_VERSION);
        assert!(loaded.snapshot.timeline.audio_transitions.is_empty());
        assert_eq!(migrate_document(loaded.clone()).unwrap(), loaded);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_five_transition_without_kind_defaults_to_cross_dissolve() {
        let root = test_root("transition-v5-default-kind");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("Transition.nleproj");
        let mut editor = EditorState::new(Language::English, "Legacy transition");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                TimelineMediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, right, Tick(1_000_000), 0.35)
            .unwrap();
        let mut legacy = document_for_path(
            &path,
            "Legacy transition",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        legacy.version = 5;
        let mut legacy_value = serde_json::to_value(&legacy).unwrap();
        legacy_value["snapshot"]["timeline"]["transitions"][0]
            .as_object_mut()
            .unwrap()
            .remove("kind");
        fs::write(&path, serde_json::to_vec_pretty(&legacy_value).unwrap()).unwrap();

        let loaded = read_document(&path).unwrap().unwrap();
        assert_eq!(loaded.version, PROJECT_DOCUMENT_VERSION);
        assert_eq!(
            loaded.snapshot.timeline.transitions[0].kind,
            VideoTransitionKind::CrossDissolve
        );
        assert_eq!(migrate_document(loaded.clone()).unwrap(), loaded);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_one_through_seven_documents_load_and_migrate_idempotently_to_version_eight() {
        let root = test_root("legacy-version");
        fs::create_dir_all(&root).unwrap();
        let editor = EditorState::new(Language::English, "Legacy version");
        for version in 1..=7 {
            let path = root.join(format!("Version{version}.nleproj"));
            let mut legacy = document_for_path(
                &path,
                "Legacy version",
                editor.snapshot(),
                ProjectSettings::default(),
            );
            legacy.version = version;
            let mut legacy_value = serde_json::to_value(&legacy).unwrap();
            let timeline = legacy_value["snapshot"]["timeline"]
                .as_object_mut()
                .unwrap();
            timeline.remove("transitions");
            timeline.remove("audio_transitions");
            if version <= 3 {
                timeline.remove("titles");
            }
            fs::write(&path, serde_json::to_vec_pretty(&legacy_value).unwrap()).unwrap();

            let loaded = read_document(&path).unwrap().unwrap();
            assert_eq!(loaded.version, PROJECT_DOCUMENT_VERSION);
            assert!(loaded.snapshot.timeline.transitions.is_empty());
            if version <= 3 {
                assert!(loaded.snapshot.timeline.titles.is_empty());
            }
            assert_eq!(migrate_document(loaded.clone()).unwrap(), loaded);
            assert!(matches!(
                write_document(&path, &legacy),
                Err(ProjectIoError::UnsupportedVersion(rejected)) if rejected == version
            ));
        }
        let current = document_for_path(
            &root.join("Version8.nleproj"),
            "Current version",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        assert_eq!(migrate_document(current.clone()).unwrap(), current);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn future_document_version_is_rejected_before_unknown_interpolation_is_parsed() {
        let root = test_root("future-version");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("Future.nleproj");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": PROJECT_DOCUMENT_VERSION + 1,
                "snapshot": {
                    "timeline": {
                        "tracks": [{
                            "clips": [{
                                "video_effects": [{
                                    "type": "brightness_contrast",
                                    "brightness": {
                                        "keyframes": [{ "interpolation": "future_curve" }]
                                    }
                                }]
                            }]
                        }]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(matches!(
            read_document(&path),
            Err(ProjectIoError::UnsupportedVersion(version)) if version == PROJECT_DOCUMENT_VERSION + 1
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_seven_effect_arrays_migrate_to_ordered_schema_v1_graphs() {
        let root = test_root("effect-stack-round-trip");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("EffectStack.nleproj");
        let mut editor = EditorState::new(Language::English, "Effect stack");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut value = serde_json::to_value(document_for_path(
            &path,
            "Effect stack",
            editor.snapshot(),
            ProjectSettings::default(),
        ))
        .unwrap();
        value["snapshot"]["timeline"]["tracks"][0]["clips"][0]["video_effects"] = serde_json::json!([{
            "id": 1,
            "enabled": true,
            "type": "brightness_contrast",
            "brightness": {
                "value": 0.0,
                "keyframes": [
                    { "source_tick": 0, "value": 0.0, "interpolation": "Smooth" },
                    { "source_tick": 1000000, "value": 0.1, "interpolation": "EaseIn" },
                    { "source_tick": 2000000, "value": 0.2, "interpolation": "EaseOut" },
                    { "source_tick": 3000000, "value": 0.3, "interpolation": "Linear" }
                ]
            },
            "contrast": { "value": 1.0, "keyframes": [] }
        }, {
            "id": 2,
            "enabled": true,
            "type": "brightness_contrast",
            "brightness": { "value": 0.25, "keyframes": [] },
            "contrast": { "value": 1.5, "keyframes": [] }
        }]);
        value["version"] = serde_json::json!(7);
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let round_tripped = serde_json::to_value(read_document(&path).unwrap().unwrap()).unwrap();
        let keyframes = round_tripped["snapshot"]["timeline"]["tracks"][0]["clips"][0]
            ["video_effects"]["nodes"][0]["brightness"]["keyframes"]
            .as_array()
            .unwrap();
        assert_eq!(
            keyframes
                .iter()
                .map(|key| key["interpolation"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["Smooth", "EaseIn", "EaseOut", "Linear"]
        );
        let graph =
            &round_tripped["snapshot"]["timeline"]["tracks"][0]["clips"][0]["video_effects"];
        assert_eq!(graph["schema_version"], 1);
        assert_eq!(graph["connections"].as_array().unwrap().len(), 1);
        let effects = graph["nodes"].as_array().unwrap();
        assert_eq!(
            effects
                .iter()
                .map(|effect| effect["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_document_rejects_an_unsupported_nested_effect_graph_schema() {
        let root = test_root("future-effect-graph");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("FutureEffectGraph.nleproj");
        let mut editor = EditorState::new(Language::English, "Future effect graph");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut value = serde_json::to_value(document_for_path(
            &path,
            "Future effect graph",
            editor.snapshot(),
            ProjectSettings::default(),
        ))
        .unwrap();
        value["snapshot"]["timeline"]["tracks"][0]["clips"][0]["video_effects"]["schema_version"] =
            serde_json::json!(2);
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        assert!(matches!(
            read_document(&path),
            Err(ProjectIoError::InvalidSnapshot(error))
                if error.contains("UnsupportedSchemaVersion")
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moved_project_folder_resolves_relative_media_first() {
        let root = test_root("move");
        let original = root.join("original");
        let moved = root.join("moved");
        fs::create_dir_all(original.join("media")).unwrap();
        fs::write(original.join("media/clip.mp4"), b"media").unwrap();
        let mut editor = EditorState::new(Language::English, "Portable");
        editor.add_media_paths([original.join("media/clip.mp4")]);
        let project_path = original.join("Portable.nleproj");
        let document = document_for_path(
            &project_path,
            "Portable",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        assert_eq!(
            document.media[0].relative.as_deref(),
            Some(Path::new("media/clip.mp4"))
        );
        write_document(&project_path, &document).unwrap();

        fs::rename(&original, &moved).unwrap();
        let loaded = read_document(&moved.join("Portable.nleproj"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.snapshot.media[0].path, moved.join("media/clip.mp4"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reordered_portable_media_pairs_resolve_then_restore_in_canonical_id_order() {
        let root = test_root("reordered");
        let media_dir = root.join("media");
        fs::create_dir_all(&media_dir).unwrap();
        let first_path = media_dir.join("first.mp4");
        let second_path = media_dir.join("second.mp4");
        fs::write(&first_path, b"first").unwrap();
        fs::write(&second_path, b"second").unwrap();

        let mut editor = EditorState::new(Language::English, "Reordered portable");
        editor.add_media_paths([first_path.clone(), second_path.clone()]);
        let project_path = root.join("Reordered.nleproj");
        let mut document = document_for_path(
            &project_path,
            "Reordered portable",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        document.media.reverse();
        document.snapshot.media.reverse();
        write_document(&project_path, &document).unwrap();

        let loaded = read_document(&project_path).unwrap().unwrap();
        assert_eq!(loaded.snapshot.media[0].id, 2);
        assert_eq!(loaded.snapshot.media[0].path, second_path);
        let restored =
            EditorState::restore(Language::English, "Reordered portable", loaded.snapshot).unwrap();
        assert_eq!(
            restored
                .media
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(restored.media[0].path, first_path);
        assert_eq!(restored.media[1].path, second_path);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_primary_recovers_the_last_atomic_backup() {
        let root = test_root("backup");
        let path = root.join("Recovery.nleproj");
        let mut editor = EditorState::new(Language::English, "Recovery");
        let first = document_for_path(
            &path,
            "Recovery",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        write_document(&path, &first).unwrap();
        editor.add_media_paths([root.join("missing.mp4")]);
        let second = document_for_path(
            &path,
            "Recovery",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        write_document(&path, &second).unwrap();
        fs::write(&path, b"broken").unwrap();

        let recovered = read_document(&path).unwrap().unwrap();
        assert!(recovered.snapshot.media.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_project_document_without_clip_transform_restores_identity_composition() {
        let project_path = test_root("legacy-transform").join("Legacy.nleproj");
        let mut editor = EditorState::new(Language::English, "Legacy transform");
        editor.add_media_paths([PathBuf::from("legacy.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let document = document_for_path(
            &project_path,
            "Legacy transform",
            editor.snapshot(),
            ProjectSettings::default(),
        );
        let mut legacy = serde_json::to_value(document).unwrap();
        for track in legacy["snapshot"]["timeline"]["tracks"]
            .as_array_mut()
            .unwrap()
        {
            for clip in track["clips"].as_array_mut().unwrap() {
                clip.as_object_mut().unwrap().remove("transform");
            }
        }

        let document: ProjectDocument = serde_json::from_value(legacy).unwrap();
        let restored =
            EditorState::restore(Language::English, "Legacy transform", document.snapshot).unwrap();
        assert!(
            restored
                .timeline
                .tracks
                .iter()
                .flat_map(|track| &track.clips)
                .all(|clip| clip.transform == Default::default())
        );
    }
}
