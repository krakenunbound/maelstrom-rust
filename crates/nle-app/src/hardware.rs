//! Startup graphics inventory. Codec capability confirmation belongs to the media backend.

use std::collections::HashSet;

/// Stable machine facts included with performance reports.
///
/// Unavailable platform facts remain `None` and serialize as JSON `null`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineProfile {
    pub cpu_identity: Option<String>,
    pub logical_cpu_count: usize,
    pub total_physical_memory_bytes: Option<u64>,
}

/// Detects lightweight, non-identifying machine facts for diagnostics reports.
pub fn detect_machine() -> MachineProfile {
    MachineProfile {
        cpu_identity: cpu_identity(),
        logical_cpu_count: std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1),
        total_physical_memory_bytes: total_physical_memory_bytes(),
    }
}

fn cpu_identity() -> Option<String> {
    #[cfg(windows)]
    {
        std::env::var("PROCESSOR_IDENTIFIER")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
fn total_physical_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is initialized with its documented byte size and remains
    // valid and exclusively mutable for the duration of this system call.
    if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
        Some(status.ullTotalPhys)
    } else {
        None
    }
}

#[cfg(not(windows))]
fn total_physical_memory_bytes() -> Option<u64> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdapterClass {
    Discrete,
    Integrated,
    Virtual,
    Software,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GraphicsAdapter {
    pub name: String,
    pub vendor: u32,
    pub device: u32,
    pub class: AdapterClass,
    pub backend: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HardwareProfile {
    pub adapters: Vec<GraphicsAdapter>,
    /// Preferred for wgpu rendering, compositing, effects, and monitor presentation.
    pub render_adapter: Option<String>,
    /// Candidate for future hardware video decode/encode work.
    pub media_adapter: Option<String>,
    /// Presence is only a candidate; FFmpeg/Media Foundation must confirm codec support.
    pub intel_quick_sync_candidate: bool,
    pub has_discrete_gpu: bool,
    pub has_integrated_gpu: bool,
    /// NVIDIA vendor (0x10DE) adapter present. RTX VSR still needs the runtime folder.
    pub has_nvidia_gpu: bool,
}

pub fn detect() -> HardwareProfile {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
        .into_iter()
        .map(|adapter| {
            let info = adapter.get_info();
            GraphicsAdapter {
                name: info.name,
                vendor: info.vendor,
                device: info.device,
                class: match info.device_type {
                    wgpu::DeviceType::DiscreteGpu => AdapterClass::Discrete,
                    wgpu::DeviceType::IntegratedGpu => AdapterClass::Integrated,
                    wgpu::DeviceType::VirtualGpu => AdapterClass::Virtual,
                    wgpu::DeviceType::Cpu => AdapterClass::Software,
                    wgpu::DeviceType::Other => AdapterClass::Other,
                },
                backend: format!("{:?}", info.backend),
            }
        })
        .collect();
    build_profile(adapters)
}

fn build_profile(adapters: Vec<GraphicsAdapter>) -> HardwareProfile {
    let mut seen = HashSet::new();
    let adapters: Vec<_> = adapters
        .into_iter()
        .filter(|adapter| {
            seen.insert((
                adapter.vendor,
                adapter.device,
                adapter.class,
                adapter.name.clone(),
            ))
        })
        .collect();
    let has_discrete_gpu = adapters
        .iter()
        .any(|adapter| adapter.class == AdapterClass::Discrete);
    let has_integrated_gpu = adapters
        .iter()
        .any(|adapter| adapter.class == AdapterClass::Integrated);
    let quick_sync = adapters
        .iter()
        .find(|adapter| adapter.class == AdapterClass::Integrated && adapter.vendor == 0x8086);
    let render_adapter = adapters
        .iter()
        .find(|adapter| adapter.class == AdapterClass::Discrete)
        .or_else(|| {
            adapters
                .iter()
                .find(|adapter| adapter.class == AdapterClass::Integrated)
        })
        .map(|adapter| adapter.name.clone());
    let media_adapter = quick_sync
        .or_else(|| {
            adapters
                .iter()
                .find(|adapter| adapter.class == AdapterClass::Discrete)
        })
        .or_else(|| adapters.first())
        .map(|adapter| adapter.name.clone());
    let intel_quick_sync_candidate = quick_sync.is_some();
    let has_nvidia_gpu = adapters.iter().any(|adapter| adapter.vendor == 0x10DE);

    HardwareProfile {
        adapters,
        render_adapter,
        media_adapter,
        intel_quick_sync_candidate,
        has_discrete_gpu,
        has_integrated_gpu,
        has_nvidia_gpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(name: &str, vendor: u32, class: AdapterClass) -> GraphicsAdapter {
        GraphicsAdapter {
            name: name.into(),
            vendor,
            device: name.len() as u32,
            class,
            backend: "Dx12".into(),
        }
    }

    #[test]
    fn hybrid_system_uses_discrete_for_render_and_intel_for_media() {
        let profile = build_profile(vec![
            adapter("Discrete", 0x10de, AdapterClass::Discrete),
            adapter("Intel UHD", 0x8086, AdapterClass::Integrated),
        ]);
        assert_eq!(profile.render_adapter.as_deref(), Some("Discrete"));
        assert_eq!(profile.media_adapter.as_deref(), Some("Intel UHD"));
        assert!(profile.has_discrete_gpu);
        assert!(profile.has_integrated_gpu);
        assert!(profile.has_nvidia_gpu);
        assert!(profile.intel_quick_sync_candidate);
    }

    #[test]
    fn duplicate_backend_entries_are_collapsed() {
        let first = adapter("GPU", 0x10de, AdapterClass::Discrete);
        let mut second = first.clone();
        second.backend = "Vulkan".into();
        let profile = build_profile(vec![first, second]);
        assert_eq!(profile.adapters.len(), 1);
    }

    #[test]
    fn software_only_system_does_not_claim_hardware_acceleration() {
        let profile = build_profile(vec![adapter("WARP", 0x1414, AdapterClass::Software)]);
        assert!(!profile.has_discrete_gpu);
        assert!(!profile.has_integrated_gpu);
        assert!(!profile.intel_quick_sync_candidate);
        assert!(!profile.has_nvidia_gpu);
        assert!(profile.render_adapter.is_none());
    }

    #[test]
    fn machine_detection_returns_safe_report_values() {
        let profile = detect_machine();
        #[cfg(windows)]
        assert!(
            profile
                .cpu_identity
                .as_deref()
                .is_some_and(|cpu| !cpu.trim().is_empty())
        );
        assert!(profile.logical_cpu_count >= 1);
        #[cfg(windows)]
        assert!(
            profile
                .total_physical_memory_bytes
                .is_some_and(|bytes| bytes > 0)
        );
    }
}
