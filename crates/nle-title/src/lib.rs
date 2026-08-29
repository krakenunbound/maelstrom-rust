//! Deterministic, CPU-only title plate rasterization.

use std::{fmt, sync::Arc};

use ab_glyph::{Font, FontArc, OutlinedGlyph, PxScale, ScaleFont, point};
use nle_timeline::{Tick, TitleAlignment, TitleColor, TitleOverlay};

/// The project-wide UI and title font.  It is embedded so title rendering has
/// no machine-local font dependency.
pub const NOTO_SANS_JP: &[u8] =
    include_bytes!("../../../assets/fonts/noto-sans-jp/NotoSansJP-Regular.otf");

pub const MAX_TITLE_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_TITLE_LINES: usize = 256;
pub const MAX_TITLE_DIMENSION: u32 = 4096;
pub const MAX_TITLE_RGBA_BYTES: usize = 64 * 1024 * 1024;
const MAX_BLUR_RADIUS: u32 = 64;

/// A tightly bounded, straight-alpha RGBA8 title plate. `content_origin_*`
/// maps plate pixel zero back to the title layout coordinate system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleRaster {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    pub content_origin_x: i32,
    pub content_origin_y: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleRasterError {
    EmptyText,
    TextTooLarge,
    TooManyLines,
    InvalidFontSize,
    InvalidOpacity,
    InvalidOutlineWidth,
    InvalidShadowOffset,
    InvalidShadowBlur,
    PlateTooLarge,
    FontInvalid,
}

impl fmt::Display for TitleRasterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyText => "title text is empty",
            Self::TextTooLarge => "title text exceeds 16 KiB",
            Self::TooManyLines => "title has more than 256 lines",
            Self::InvalidFontSize => "title font size must be finite and positive",
            Self::InvalidOpacity => "title opacity must be finite and between zero and one",
            Self::InvalidOutlineWidth => "title outline width must be finite and non-negative",
            Self::InvalidShadowOffset => "title shadow offset must be finite",
            Self::InvalidShadowBlur => "title shadow blur must be finite and bounded",
            Self::PlateTooLarge => "title raster plate exceeds configured limits",
            Self::FontInvalid => "embedded title font is invalid",
        };
        f.write_str(message)
    }
}

impl std::error::Error for TitleRasterError {}

#[derive(Clone)]
struct DrawGlyph {
    outlined: OutlinedGlyph,
}

/// Rasterizes an overlay into a transparent, tight plate. The output contains
/// style alpha only; callers apply [`title_fade_opacity`] exactly once. It does
/// not depend on installed fonts, GPU state, locale, or runtime file I/O.
pub fn rasterize_title(title: &TitleOverlay) -> Result<TitleRaster, TitleRasterError> {
    validate(title)?;
    let font = FontArc::try_from_slice(NOTO_SANS_JP).map_err(|_| TitleRasterError::FontInvalid)?;
    let scale = PxScale::from(title.font_size);
    let scaled = font.as_scaled(scale);
    let lines: Vec<&str> = title.text.split('\n').collect();
    let line_height = scaled.height().ceil().max(1.0);
    let line_widths: Vec<f32> = lines
        .iter()
        .map(|line| advance_width(&scaled, line))
        .collect();
    let max_width = line_widths.iter().copied().fold(0.0_f32, f32::max);
    let mut glyphs = Vec::new();
    let mut bounds: Option<(i32, i32, i32, i32)> = None;

    for (line_index, line) in lines.iter().enumerate() {
        let base_x = match title.alignment {
            TitleAlignment::Left => 0.0,
            TitleAlignment::Center => (max_width - line_widths[line_index]) * 0.5,
            TitleAlignment::Right => max_width - line_widths[line_index],
        };
        // Font ascent puts the first line baseline in a conventional layout space.
        let baseline_y = scaled.ascent() + line_index as f32 * line_height;
        let mut x = base_x;
        for character in line.chars() {
            let glyph_id = font.glyph_id(character);
            let glyph = glyph_id.with_scale_and_position(scale, point(x, baseline_y));
            x += scaled.h_advance(glyph_id);
            if let Some(outlined) = font.outline_glyph(glyph.clone()) {
                let rect = outlined.px_bounds();
                extend_bounds(
                    &mut bounds,
                    rect.min.x.floor() as i32,
                    rect.min.y.floor() as i32,
                    rect.max.x.ceil() as i32,
                    rect.max.y.ceil() as i32,
                );
                glyphs.push(DrawGlyph { outlined });
            }
        }
    }
    let Some((min_x, min_y, max_x, max_y)) = bounds else {
        return Err(TitleRasterError::EmptyText);
    };
    let outline = title.outline_width.ceil() as i32;
    let blur = title.shadow_blur.ceil() as i32;
    let shadow_x = title.shadow_offset_x.round() as i32;
    let shadow_y = title.shadow_offset_y.round() as i32;
    let left = min_x.min(min_x.saturating_add(shadow_x)) - outline - blur;
    let top = min_y.min(min_y.saturating_add(shadow_y)) - outline - blur;
    let right = max_x.max(max_x.saturating_add(shadow_x)) + outline + blur;
    let bottom = max_y.max(max_y.saturating_add(shadow_y)) + outline + blur;
    let width = u32::try_from(
        right
            .checked_sub(left)
            .ok_or(TitleRasterError::PlateTooLarge)?,
    )
    .map_err(|_| TitleRasterError::PlateTooLarge)?;
    let height = u32::try_from(
        bottom
            .checked_sub(top)
            .ok_or(TitleRasterError::PlateTooLarge)?,
    )
    .map_err(|_| TitleRasterError::PlateTooLarge)?;
    check_plate_size(width, height)?;
    let length = width as usize * height as usize;
    let mut coverage = vec![0_u8; length];
    for draw in &glyphs {
        paint_coverage(&mut coverage, width, height, left, top, &draw.outlined);
    }
    let mut rgba = vec![0_u8; length * 4];
    if title.shadow_color.a != 0 {
        let shadow = box_blur(&coverage, width, height, blur as u32);
        paint_layer(
            &mut rgba,
            &shadow,
            width,
            height,
            shadow_x,
            shadow_y,
            title.shadow_color,
        );
    }
    if title.outline_color.a != 0 && outline > 0 {
        let outline_coverage = max_dilate(&coverage, width, height, outline as u32);
        paint_layer(
            &mut rgba,
            &outline_coverage,
            width,
            height,
            0,
            0,
            title.outline_color,
        );
    }
    paint_layer(&mut rgba, &coverage, width, height, 0, 0, title.fill);
    Ok(TitleRaster {
        width,
        height,
        rgba: Arc::from(rgba),
        content_origin_x: left,
        content_origin_y: top,
    })
}

/// Shared title fade evaluation. The local tick is measured from the title's
/// start; values before/after its duration are invisible.
pub fn title_fade_opacity(title: &TitleOverlay, local_tick: Tick) -> f32 {
    if !title.enabled
        || !title.opacity.is_finite()
        || title.opacity <= 0.0
        || local_tick.0 < 0
        || local_tick >= title.duration
    {
        return 0.0;
    }
    let duration = title.duration.0;
    if duration <= 0 {
        return 0.0;
    }
    let mut factor = 1.0_f32;
    if title.fade_in.0 > 0 && local_tick.0 < title.fade_in.0 {
        factor = factor.min(local_tick.0 as f32 / title.fade_in.0 as f32);
    }
    let remaining = duration.saturating_sub(local_tick.0);
    if title.fade_out.0 > 0 && remaining < title.fade_out.0 {
        factor = factor.min(remaining as f32 / title.fade_out.0 as f32);
    }
    title.opacity.clamp(0.0, 1.0) * factor.clamp(0.0, 1.0)
}

fn validate(title: &TitleOverlay) -> Result<(), TitleRasterError> {
    if title.text.is_empty() {
        return Err(TitleRasterError::EmptyText);
    }
    if title.text.len() > MAX_TITLE_TEXT_BYTES {
        return Err(TitleRasterError::TextTooLarge);
    }
    if title.text.split('\n').count() > MAX_TITLE_LINES {
        return Err(TitleRasterError::TooManyLines);
    }
    if !title.font_size.is_finite() || title.font_size <= 0.0 {
        return Err(TitleRasterError::InvalidFontSize);
    }
    if title.font_size > MAX_TITLE_DIMENSION as f32 {
        return Err(TitleRasterError::PlateTooLarge);
    }
    if !title.opacity.is_finite() || !(0.0..=1.0).contains(&title.opacity) {
        return Err(TitleRasterError::InvalidOpacity);
    }
    if !title.outline_width.is_finite() || title.outline_width < 0.0 {
        return Err(TitleRasterError::InvalidOutlineWidth);
    }
    if title.outline_width > MAX_TITLE_DIMENSION as f32 {
        return Err(TitleRasterError::PlateTooLarge);
    }
    if !title.shadow_offset_x.is_finite() || !title.shadow_offset_y.is_finite() {
        return Err(TitleRasterError::InvalidShadowOffset);
    }
    if title.shadow_offset_x.abs() > MAX_TITLE_DIMENSION as f32
        || title.shadow_offset_y.abs() > MAX_TITLE_DIMENSION as f32
    {
        return Err(TitleRasterError::PlateTooLarge);
    }
    if !title.shadow_blur.is_finite()
        || !(0.0..=MAX_BLUR_RADIUS as f32).contains(&title.shadow_blur)
    {
        return Err(TitleRasterError::InvalidShadowBlur);
    }
    Ok(())
}

fn advance_width<F: Font>(font: &impl ScaleFont<F>, line: &str) -> f32 {
    line.chars().map(|c| font.h_advance(font.glyph_id(c))).sum()
}
fn extend_bounds(bounds: &mut Option<(i32, i32, i32, i32)>, x0: i32, y0: i32, x1: i32, y1: i32) {
    *bounds = Some(match *bounds {
        Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
        None => (x0, y0, x1, y1),
    });
}
fn check_plate_size(width: u32, height: u32) -> Result<(), TitleRasterError> {
    if width == 0
        || height == 0
        || width > MAX_TITLE_DIMENSION
        || height > MAX_TITLE_DIMENSION
        || (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|&n| n <= MAX_TITLE_RGBA_BYTES)
            .is_none()
    {
        Err(TitleRasterError::PlateTooLarge)
    } else {
        Ok(())
    }
}
fn paint_coverage(
    output: &mut [u8],
    width: u32,
    height: u32,
    left: i32,
    top: i32,
    glyph: &OutlinedGlyph,
) {
    glyph.draw(|x, y, alpha| {
        let x = x as i32 + glyph.px_bounds().min.x.floor() as i32 - left;
        let y = y as i32 + glyph.px_bounds().min.y.floor() as i32 - top;
        if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
            let index = y as usize * width as usize + x as usize;
            output[index] = output[index].max((alpha * 255.0 + 0.5) as u8);
        }
    });
}
/// Exact square dilation implemented as horizontal and vertical sliding maxima.
/// This is O(width * height), independent of the dilation radius.
fn max_dilate(input: &[u8], width: u32, height: u32, radius: u32) -> Vec<u8> {
    if radius == 0 {
        return input.to_vec();
    }
    let width = width as usize;
    let height = height as usize;
    let mut horizontal = vec![0; input.len()];
    for y in 0..height {
        max_filter_line(
            &input[y * width..(y + 1) * width],
            &mut horizontal[y * width..(y + 1) * width],
            radius,
        );
    }
    let mut column = vec![0; height];
    let mut filtered = vec![0; height];
    let mut output = vec![0; input.len()];
    for x in 0..width {
        for y in 0..height {
            column[y] = horizontal[y * width + x];
        }
        max_filter_line(&column, &mut filtered, radius);
        for y in 0..height {
            output[y * width + x] = filtered[y];
        }
    }
    output
}

fn max_filter_line(input: &[u8], output: &mut [u8], radius: u32) {
    let radius = radius as usize;
    let mut deque = std::collections::VecDeque::<(usize, u8)>::new();
    let end = input.len() + radius * 2;
    for padded_index in 0..end {
        let value = padded_index
            .checked_sub(radius)
            .and_then(|index| input.get(index))
            .copied()
            .unwrap_or(0);
        while deque.back().is_some_and(|&(_, previous)| previous <= value) {
            deque.pop_back();
        }
        deque.push_back((padded_index, value));
        let window_start = padded_index.saturating_sub(radius * 2);
        while deque
            .front()
            .is_some_and(|&(index, _)| index < window_start)
        {
            deque.pop_front();
        }
        if padded_index >= radius * 2 {
            output[padded_index - radius * 2] = deque.front().map(|&(_, value)| value).unwrap_or(0);
        }
    }
}

/// Deterministic separable box blur with transparent pixels beyond the plate.
/// The fixed denominator makes edge falloff match a transparent background.
fn box_blur(input: &[u8], width: u32, height: u32, radius: u32) -> Vec<u8> {
    if radius == 0 {
        return input.to_vec();
    }
    let width = width as usize;
    let height = height as usize;
    let mut horizontal = vec![0; input.len()];
    for y in 0..height {
        box_blur_line(
            &input[y * width..(y + 1) * width],
            &mut horizontal[y * width..(y + 1) * width],
            radius,
        );
    }
    let mut column = vec![0; height];
    let mut filtered = vec![0; height];
    let mut output = vec![0; input.len()];
    for x in 0..width {
        for y in 0..height {
            column[y] = horizontal[y * width + x];
        }
        box_blur_line(&column, &mut filtered, radius);
        for y in 0..height {
            output[y * width + x] = filtered[y];
        }
    }
    output
}

fn box_blur_line(input: &[u8], output: &mut [u8], radius: u32) {
    let radius = radius as usize;
    let denominator = (radius * 2 + 1) as u32;
    let mut sum: u32 = input
        .iter()
        .take(radius + 1)
        .map(|&value| u32::from(value))
        .sum();
    for (index, value) in output.iter_mut().enumerate() {
        *value = ((sum + denominator / 2) / denominator) as u8;
        if index >= radius {
            sum -= u32::from(input[index - radius]);
        }
        if let Some(&next) = input.get(index + radius + 1) {
            sum += u32::from(next);
        }
    }
}
fn paint_layer(
    output: &mut [u8],
    coverage: &[u8],
    width: u32,
    height: u32,
    offset_x: i32,
    offset_y: i32,
    color: TitleColor,
) {
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let source_x = x - offset_x;
            let source_y = y - offset_y;
            if source_x < 0 || source_y < 0 || source_x >= width as i32 || source_y >= height as i32
            {
                continue;
            }
            let coverage_alpha =
                coverage[source_y as usize * width as usize + source_x as usize] as f32 / 255.0;
            let alpha = coverage_alpha * (color.a as f32 / 255.0);
            if alpha <= 0.0 {
                continue;
            }
            let index = (y as usize * width as usize + x as usize) * 4;
            let dst_alpha = output[index + 3] as f32 / 255.0;
            let out_alpha = alpha + dst_alpha * (1.0 - alpha);
            for channel in 0..3 {
                let dst = output[index + channel] as f32 / 255.0;
                let src = color.channel(channel) as f32 / 255.0;
                output[index + channel] =
                    (((src * alpha + dst * dst_alpha * (1.0 - alpha)) / out_alpha) * 255.0 + 0.5)
                        as u8;
            }
            output[index + 3] = (out_alpha * 255.0 + 0.5) as u8;
        }
    }
}

trait ColorChannel {
    fn channel(&self, channel: usize) -> u8;
}
impl ColorChannel for TitleColor {
    fn channel(&self, channel: usize) -> u8 {
        match channel {
            0 => self.r,
            1 => self.g,
            _ => self.b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nle_timeline::TitleId;

    fn title(text: &str) -> TitleOverlay {
        TitleOverlay {
            id: TitleId(1),
            start: Tick(0),
            duration: Tick(100),
            text: text.into(),
            font_size: 36.0,
            alignment: TitleAlignment::Left,
            position_x: 0.5,
            position_y: 0.5,
            fill: TitleColor::rgba(240, 200, 80, 255),
            outline_color: TitleColor::rgba(10, 20, 30, 255),
            outline_width: 2.0,
            shadow_color: TitleColor::rgba(30, 40, 50, 200),
            shadow_offset_x: 3.0,
            shadow_offset_y: 2.0,
            shadow_blur: 2.0,
            opacity: 0.75,
            fade_in: Tick(10),
            fade_out: Tick(20),
            enabled: true,
            z_order: 0,
        }
    }

    #[test]
    fn raster_is_deterministic_and_supports_utf8_multiline() {
        let title = title("Hello 日本語\nsecond line");
        let first = rasterize_title(&title).unwrap();
        let second = rasterize_title(&title).unwrap();
        assert_eq!(first, second);
        assert!(first.width > 0 && first.height > 0);
        assert!(first.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn alignment_changes_layout_and_effect_layers_contribute_alpha() {
        let left = title("wide\ni");
        let mut center = left.clone();
        let mut right = left.clone();
        center.alignment = TitleAlignment::Center;
        right.alignment = TitleAlignment::Right;
        let left = rasterize_title(&left).unwrap();
        let center = rasterize_title(&center).unwrap();
        let right = rasterize_title(&right).unwrap();
        assert_ne!(left.rgba, center.rgba);
        assert_ne!(center.rgba, right.rgba);
        for plate in [&left, &center, &right] {
            assert!(plate.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
        }
    }

    #[test]
    fn fade_boundaries_are_shared_and_exact() {
        let title = title("fade");
        assert_eq!(title_fade_opacity(&title, Tick(-1)), 0.0);
        assert_eq!(title_fade_opacity(&title, Tick(0)), 0.0);
        assert!((title_fade_opacity(&title, Tick(5)) - 0.375).abs() < 0.0001);
        assert!((title_fade_opacity(&title, Tick(10)) - 0.75).abs() < 0.0001);
        assert!((title_fade_opacity(&title, Tick(90)) - 0.375).abs() < 0.0001);
        assert_eq!(title_fade_opacity(&title, Tick(100)), 0.0);
    }

    #[test]
    fn invalid_and_oversized_inputs_return_errors_without_panicking() {
        let mut empty = title("");
        assert_eq!(rasterize_title(&empty), Err(TitleRasterError::EmptyText));
        empty.text = "x".repeat(MAX_TITLE_TEXT_BYTES + 1);
        assert_eq!(rasterize_title(&empty), Err(TitleRasterError::TextTooLarge));
        empty.text = (0..=MAX_TITLE_LINES)
            .map(|_| "x")
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rasterize_title(&empty), Err(TitleRasterError::TooManyLines));
        let mut huge = title("huge");
        huge.font_size = 100_000.0;
        assert_eq!(rasterize_title(&huge), Err(TitleRasterError::PlateTooLarge));
        huge.font_size = f32::NAN;
        assert_eq!(
            rasterize_title(&huge),
            Err(TitleRasterError::InvalidFontSize)
        );
        assert!(std::panic::catch_unwind(|| rasterize_title(&huge)).is_ok());
    }

    #[test]
    fn maximum_valid_style_raster_completes_with_linear_filters() {
        // This plate is deliberately large enough that the former nested
        // radius scans would perform billions of alpha comparisons.
        let mut title = title(&vec!["W".repeat(10); 4].join("\n"));
        title.font_size = 256.0;
        title.outline_width = 32.0;
        title.shadow_blur = 64.0;
        let plate = rasterize_title(&title).expect("maximum model style remains renderable");
        assert!(plate.width <= MAX_TITLE_DIMENSION && plate.height <= MAX_TITLE_DIMENSION);
        assert!(plate.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }
}
