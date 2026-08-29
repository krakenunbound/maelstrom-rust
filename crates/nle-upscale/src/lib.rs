//! Native NVIDIA RTX Video Super Resolution for Maelstrom.
//!
//! This crate binds NVIDIA Video Effects `VideoSuperRes` through the C API.
//! It does not launch the standalone Kraken Upscale app.

mod goal;
mod job;
pub mod rtx_vsr;

pub use goal::{dimensions, goal_from_index};
pub use job::{UpscaleEvent, UpscaleJob, UpscaleRequest};
pub use rtx_vsr::{Quality, runtime_dir};

/// NVIDIA GPU plus a complete `rtx-vsr` runtime folder.
pub fn capability(has_nvidia_gpu: bool) -> (bool, String) {
    if !has_nvidia_gpu {
        return (
            false,
            "No NVIDIA GPU detected. Kraken Upscale needs an NVIDIA RTX GPU for RTX VSR."
                .to_owned(),
        );
    }
    match runtime_dir() {
        Some(dir) => (true, format!("NVIDIA RTX VSR ready — {}", dir.display())),
        None => (
            false,
            "NVIDIA GPU found, but the RTX VSR runtime folder (rtx-vsr) is missing.".to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_nvidia_is_gated() {
        let (ready, reason) = capability(false);
        assert!(!ready);
        assert!(reason.contains("NVIDIA"));
    }
}
