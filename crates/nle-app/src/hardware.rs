//! Startup graphics inventory. Codec capability confirmation belongs to the media backend.

use std::collections::HashSet;

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
}
