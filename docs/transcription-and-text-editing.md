# Transcription, captions, and text-based editing

## Product contract

Maelstrom will provide a dockable **Text** workspace for local transcript generation, correction,
navigation, caption creation, and reviewable text-based edits. The panel follows the same native
detach/re-dock contract as the other editor sections, so it can live on a separate monitor without
creating a second timeline, decoder, undo stack, or render authority.

The first implementation is offline-first and optional. Missing models or an unsupported device
must leave the rest of the editor fully functional and show a specific setup or capability reason.
Maelstrom must never download weights, accept third-party terms, send media to a service, or lower
playback quality without an explicit user action.

## Reference implementation already proven locally

The user's local music creator demonstrates a useful production pattern rather than a single
transcription engine:

- `faster-whisper` runs `large-v3-turbo` with word timestamps, beam search, Silero VAD, and a CUDA
  `float16` to CPU `int8` fallback;
- WhisperX 3.8.4 provides a second transcription/alignment path and can force-align known lyrics;
- the worker retries a separated vocal stem and the full mix rather than returning a sparse result;
- the Python runtime and weights are installed separately, remain outside the application source,
  and the feature is disabled when they are absent.

That arrangement is valuable and should seed Maelstrom's first backend. It is optimized for lyric
alignment, however, while a video editor also needs dialogue accuracy, speaker assignment,
timeline-safe edits, caption formats, cache invalidation, and English/Japanese qualification.
MiniMaxM3's current third-party notice does not enumerate WhisperX, faster-whisper, Silero, or the
speech/alignment weights, so Maelstrom must derive and verify each runtime/model license and source
independently rather than treating that project's notice as a complete redistribution record.

## Backend decision and candidates

No model is accepted by reputation alone. The default is selected from repeatable measurements on
Maelstrom's own English/Japanese dialogue, noisy-location, music-under-speech, and long-form corpus.

| Candidate | Intended role | Strengths | Constraint before adoption |
| --- | --- | --- | --- |
| [OpenAI Whisper large-v3-turbo](https://huggingface.co/openai/whisper-large-v3-turbo) through [faster-whisper](https://github.com/SYSTRAN/faster-whisper) | Initial dependable ASR backend | MIT weights; broad multilingual coverage including English and Japanese; proven locally; word timestamps and Silero VAD are available in the runtime | Measure English WER, Japanese CER, timing error, long-form drift, GPU memory, and CPU fallback speed |
| [WhisperX](https://github.com/m-bain/whisperX) | Optional alignment and speaker-assignment stage | BSD-2-Clause code; word alignment and diarization integration; already proven in the local lyric workflow | Alignment models vary by language and may carry separate terms; Japanese timing and every downloaded dependency require separate qualification |
| [Qwen3-ASR 0.6B/1.7B](https://github.com/QwenLM/Qwen3-ASR) with [Qwen3 Forced Aligner](https://huggingface.co/Qwen/Qwen3-ForcedAligner-0.6B-hf) | Required evaluation candidate; potential multilingual/Japanese or music backend | Apache-2.0 code/models; official support includes English and Japanese, songs with background music, and an aligner that includes Japanese | Newer stack and larger runtime surface; must beat the baseline on the local corpus without breaking bounded jobs or packaging |
| [NVIDIA Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) | Optional fast backend for supported languages | CC BY 4.0; punctuation, capitalization, word/segment timestamps, and long-audio support | Its 25-language set is European and excludes Japanese, so it cannot replace the universal EN/JA backend |
| [pyannote Community-1](https://huggingface.co/pyannote/speaker-diarization-community-1) | Optional speaker diarization | CC BY 4.0; offline-capable speaker segmentation | Access requires accepting provider conditions and supplying a token for acquisition; never bundle or silently fetch it |

Whisper, Qwen, and Parakeet are model choices. `faster-whisper`, WhisperX, and ONNX/CTranslate2 are
runtime or alignment choices, so a faster runtime is not automatically a more accurate model.

### Initial recommendation

1. Define a versioned `TranscriptionBackend` protocol before integrating a model.
2. Ship the first local proof with the already proven `large-v3-turbo`/faster-whisper path, using
   built-in word timing and Silero VAD. Keep WhisperX alignment and diarization optional.
3. Qualify Qwen3-ASR and its Japanese-capable forced aligner against the same fixtures before
   choosing the release default. It may become the preferred Japanese or music backend if the
   evidence supports that choice.
4. Offer Parakeet only as a user-visible accelerated option when the selected language is
   supported. Never switch engines or quality silently.

## Architecture

### Optional sidecar boundary

The native Rust editor owns projects, jobs, UI state, transcript persistence, timeline mutations,
and cancellation. A separately installed local worker owns Python/model dependencies. Communication
uses a versioned, length-bounded protocol over local standard IO or a named pipe; arbitrary shell
commands, URLs, and caller-selected output paths are not accepted. Any named pipe is random per
session, restricted by an explicit current-user/session ACL, and authenticated by a one-use secret;
standard IO remains the simpler default.

Installation and inference are separate modes. Only an explicit setup action may use the network,
showing the exact source, terms, size, destination, and checksum before acquisition. Normal editor,
test, and inference paths accept only resolved local files, enable dependency-library offline modes,
and reject model IDs that could trigger an implicit Hugging Face or vendor download. Tests run with
network unavailable and an empty model root to prove that inspection and inference cannot call out.

Each request includes a media fingerprint, decoded mono PCM contract or approved local source,
language policy, requested backend/model ID, time range, and job generation. Each result includes:

- backend, model, runtime, alignment, and diarization versions;
- detected/requested language and confidence;
- segments and tokens with stable IDs, timeline ticks, text, confidence, and optional speaker ID;
- warnings, unsupported-language spans, and provenance needed to reproduce the result.

The job system is cancellable, latest-generation-wins, bounded in concurrency and memory, and never
runs inference on the UI, audio callback, playback, or render thread. Partial progress can appear
incrementally, but stale results cannot replace a newer transcript.

### Persistence and invalidation

Transcript data is derived cache, not source truth. Cache keys include media content fingerprint,
selected channels/mix, source range, backend/model/runtime versions, language policy, and analysis
settings. Project state stores the chosen transcript revision plus user corrections and text-edit
decisions; regenerating analysis must not discard corrections without review.

Trim, slip, speed, replace, relink, channel, and source-interpretation changes invalidate only
affected spans. Deleting the derived cache causes regeneration, never project loss or output change.

### Model and license safety

No weights are added by this plan. The existing `assets/models/manifest.json` registry is only for
small in-process artifacts: its current loader reads every registered file into memory at editor
startup, so multi-gigabyte ASR weights must never be put through it unchanged.

The first protocol slice therefore defines a separate sidecar-owned ASR asset root and a bounded
metadata-only registry. It records an immutable source URL, version, license, checksum, exact size,
conversion/quantization history, backend consumer, redistribution status, and whether user terms
were accepted. The native app validates metadata and file identity without reading weights into
memory; only the sidecar opens a selected local weight on demand. A future unified registry is
acceptable only after it gains explicit externally-managed/no-preload entries with the same
structured provenance and preserves current small-model behavior.

ASR runtimes, weights, caches, and access tokens remain ignored until redistribution is explicitly
proven. Tokens use the operating system's credential store, never project state or logs. The setup
UI discloses download size and terms before installation, and removing ASR assets disables only the
feature.

## Text workspace

The compact default contains Transcript, Captions, and Graphics sub-tabs. The whole Text workspace
can detach, and its transcript list remains virtualized for long programs.

The Transcript view provides:

- source-clip or active-sequence scope, Generate/Regenerate, language, engine, quality, speaker, and
  channel/mix controls in nested options;
- search, speaker and confidence filters, follow-active-monitor, and jump-to-word/segment;
- editable text without moving media, clear low-confidence markers, speaker renaming, and undo;
- selection synchronization between transcript ranges, the viewer, and timeline clips;
- background progress, cancel, missing-model setup, and exact disabled reasons.

The Captions view creates an editable caption track from approved transcript segments. Import/export
starts with SRT and WebVTT, followed by TTML only after its interchange contract is specified.
Preview and export must share caption timing, styling, safe-area layout, and burn-in/sidecar settings.
Caption generation exposes readable minimum/maximum duration, gap, characters-per-second, line
length/count, punctuation-aware break, and language-aware segmentation policies. Japanese/CJK text
must break by grapheme and phrase rules rather than whitespace assumptions.

## Filler-word and pause removal

Filler removal is a non-destructive timeline edit built on reviewed word timestamps, not a hidden
ASR cleanup pass.

1. Detect candidate fillers, repetitions, and long pauses with language-specific rules, confidence,
   punctuation, and neighboring context. English and Japanese use independent reviewed lexicons.
2. Show every candidate in the transcript and timeline, with Preview, Keep, Remove Selected, and
   Remove All Reviewed actions. Low-confidence candidates are never preselected.
3. Convert approved candidates to explicit timeline ranges, validate locks, links, transitions,
   source handles, and sequence boundaries, then present the exact edit count and duration removed.
4. Apply one undoable ripple-edit transaction. Preserve source media and linked A/V sync, use short
   adjustable audio crossfades to avoid clicks, and roll back the entire action on validation error.
   If locks, transition occupancy, sequence boundaries, or insufficient audio handles prevent a
   safe crossfade, reject that candidate with a specific reason instead of weakening the edit.
5. Re-analyze only affected transcript spans and retain an audit record of accepted/rejected
   candidates so regeneration does not repeat resolved suggestions blindly.

Automatic removal may be offered later as an explicit preset, never as the default. Japanese
particles or conversational markers must not be treated as disposable filler without contextual
evidence and user review.

## Delivery slices and gates

1. **Protocol and corpus:** schema, sidecar-owned ASR asset registry, explicit installer/offline
   inference boundary, optional-runtime discovery, license records, deterministic fixtures,
   metrics, and a fake backend. No model download is required for public tests.
2. **Local transcript MVP:** selected clip/sequence generation, English/Japanese, progress/cancel,
   cache, searchable dockable panel, correction, follow/jump, save/reopen, and CPU/GPU capability.
3. **Timing and speakers:** qualified forced alignment, optional diarization, speaker correction,
   and timestamp-drift handling.
4. **Captions:** caption-track authoring, SRT/WebVTT import/export, safe layout, and preview/export
   parity.
5. **Text-based editing:** transcript range selection, ripple deletion, filler/pause review,
   crossfades, single-transaction undo, and partial invalidation.

The feature cannot ship until:

- English WER and Japanese CER/timestamp error meet recorded thresholds on clean, noisy, musical,
  multi-speaker, and long-form fixtures; backend comparisons use identical decoded PCM;
- transcript generation remains off playback-critical paths and does not violate established UI,
  scrub, audio-callback, or preview-resolution budgets;
- cancel, project switch, relink, regeneration, model failure, missing weights, CPU fallback, and
  device loss cannot install stale data or damage project state;
- normal editor, test, and inference paths cannot access the network or trigger an implicit model
  download; only the explicit setup workflow may acquire a checksum-pinned artifact;
- transcript-to-timeline edits preserve locks, links, sync, transitions, captions, undo/redo, and
  preview/export parity;
- public checkout/build/test/package remains fully functional with no model runtime or weights.
