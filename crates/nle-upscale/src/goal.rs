//! Aspect-preserving output goals, matching Kraken Upscale named sizes.

/// UI order on the Kraken Upscale page: 1080p, 1440p, 4K, ×2, ×3, ×4.
pub const GOALS: [u32; 6] = [1080, 1440, 2160, 2, 3, 4];

pub fn goal_from_index(index: u8) -> u32 {
    GOALS[(index as usize).min(GOALS.len() - 1)]
}

/// Exact aspect-preserving output. Named goals target the short edge.
pub fn dimensions(width: u32, height: u32, goal: u32, even: bool) -> (u32, u32) {
    if goal <= 4 {
        return (
            adjust(width.saturating_mul(goal), even),
            adjust(height.saturating_mul(goal), even),
        );
    }
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let (out_width, out_height) = if width >= height {
        let out_height = goal;
        let out_width = ((width as u64 * goal as u64 + height as u64 / 2) / height as u64)
            .min(u32::MAX as u64) as u32;
        (out_width, out_height)
    } else {
        let out_width = goal;
        let out_height = ((height as u64 * goal as u64 + width as u64 / 2) / width as u64)
            .min(u32::MAX as u64) as u32;
        (out_width, out_height)
    };
    (adjust(out_width, even), adjust(out_height, even))
}

fn adjust(value: u32, even: bool) -> u32 {
    if even && !value.is_multiple_of(2) {
        value.saturating_add(1)
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_1080p_keeps_16_9() {
        assert_eq!(dimensions(1280, 720, 1080, true), (1920, 1080));
    }

    #[test]
    fn multiplier_two_x() {
        assert_eq!(dimensions(1280, 720, 2, true), (2560, 1440));
    }

    #[test]
    fn portrait_4k_targets_short_edge() {
        assert_eq!(dimensions(720, 1280, 2160, true), (2160, 3840));
    }
}
