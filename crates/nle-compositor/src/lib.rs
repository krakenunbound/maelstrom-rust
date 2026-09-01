//! Deterministic, allocation-free planning for preview and export compositors.
//!
//! The plan is deliberately GPU/CPU neutral: callers upload its project-pixel
//! quads and source UVs to their own renderer. Layers occupy fixed slots in
//! bottom-to-top order, so a renderer never has to infer z-order.

#[cfg(test)]
use nle_timeline::ClipData;
use nle_timeline::{Clip, ClipId, ClipSizingMode, ClipTransform, Fade, Tick};

/// The maximum concurrently composited video layers supported by the preview.
pub const MAX_COMPOSITE_LAYERS: usize = 4;
/// Gamma and cutoff used by the monitor's video fade envelope. Export lowering evaluates the
/// same curve so a rendered fade does not visibly jump from its live preview.
pub const VIDEO_FADE_GAMMA: f32 = 1.5;
pub const VIDEO_FADE_BLACK_CUTOFF: f32 = 0.08;

/// Non-zero pixel dimensions for source or project frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

impl PixelSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn is_nonzero(self) -> bool {
        self.width != 0 && self.height != 0
    }
}

/// One source layer to be composed. Input slots are bottom-to-top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositeLayerInput {
    pub clip_id: ClipId,
    pub source_size: PixelSize,
    pub transform: ClipTransform,
    /// Runtime fade multiplier. It is intentionally not persisted on the clip.
    pub fade_opacity: f32,
}

/// A complete immutable composition request. Empty slots are skipped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositionRequest {
    pub project_size: PixelSize,
    pub layers: [Option<CompositeLayerInput>; MAX_COMPOSITE_LAYERS],
}

/// Project-pixel coordinate, where positive Y moves down the monitor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Source-local normalized texture coordinate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Uv {
    pub u: f32,
    pub v: f32,
}

/// One transformed layer quad. Vertices are TL, TR, BR, BL after rotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositeQuad {
    pub clip_id: ClipId,
    pub positions: [Point; 4],
    pub uvs: [Uv; 4],
    pub opacity: f32,
}

/// A fixed-capacity compositing plan. Slots preserve request order bottom-to-top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositionPlan {
    pub project_size: PixelSize,
    pub layers: [Option<CompositeQuad>; MAX_COMPOSITE_LAYERS],
}

/// Evaluates the timeline's quadratic fade handle from the silent/transparent edge toward full
/// level. A neutral curve is linear; negative and positive values bend the middle sample.
pub fn fade_envelope_value(fade: Fade, outer_to_full: f32) -> f32 {
    let t = outer_to_full.clamp(0.0, 1.0);
    let control = 0.5 + fade.curve.clamp(-1.0, 1.0) * 0.5;
    (2.0 * (1.0 - t) * t * control + t * t).clamp(0.0, 1.0)
}

/// Converts the shared fade envelope into the monitor/export video alpha response.
pub fn video_fade_opacity(envelope: f32) -> f32 {
    let strengthened = envelope.clamp(0.0, 1.0).powf(VIDEO_FADE_GAMMA);
    if strengthened <= VIDEO_FADE_BLACK_CUTOFF {
        0.0
    } else {
        ((strengthened - VIDEO_FADE_BLACK_CUTOFF) / (1.0 - VIDEO_FADE_BLACK_CUTOFF)).clamp(0.0, 1.0)
    }
}

/// Clip-local video fade multiplier at one timeline-relative tick.
pub fn video_opacity_at(clip: &Clip, relative_tick: Tick) -> f32 {
    let relative = relative_tick.0.clamp(0, clip.duration.0);
    let mut opacity: f32 = 1.0;
    if clip.fade_in.duration.0 > 0 && relative < clip.fade_in.duration.0 {
        opacity = opacity.min(video_fade_opacity(fade_envelope_value(
            clip.fade_in,
            relative as f32 / clip.fade_in.duration.0 as f32,
        )));
    }
    let fade_out_start = clip.duration.0.saturating_sub(clip.fade_out.duration.0);
    if clip.fade_out.duration.0 > 0 && relative > fade_out_start {
        opacity = opacity.min(video_fade_opacity(fade_envelope_value(
            clip.fade_out,
            clip.duration.0.saturating_sub(relative) as f32 / clip.fade_out.duration.0 as f32,
        )));
    }
    opacity
}

/// Builds a GPU/CPU-neutral composition plan without heap allocation.
///
/// Crop is applied in source-local UV space before sizing. `Fit` and `Fill`
/// preserve the cropped aspect ratio, `Stretch` fills both project axes, and
/// `Original` uses cropped source pixels. The transformed rectangle starts
/// `anchor_*` is held at the project center plus normalized position offset;
/// the offset uses the unscaled Fit rectangle to match the existing monitor
/// position convention. Positive rotation is clockwise in screen coordinates.
/// Invalid project dimensions return `None`; an invalid layer remains an empty
/// transparent slot so it cannot suppress ready layers.
pub fn plan_composition(request: CompositionRequest) -> Option<CompositionPlan> {
    if !request.project_size.is_nonzero() {
        return None;
    }

    let mut layers = [None; MAX_COMPOSITE_LAYERS];
    for (slot, input) in request.layers.into_iter().enumerate() {
        if let Some(input) = input {
            layers[slot] = plan_layer(request.project_size, input);
        }
    }
    Some(CompositionPlan {
        project_size: request.project_size,
        layers,
    })
}

fn plan_layer(project_size: PixelSize, input: CompositeLayerInput) -> Option<CompositeQuad> {
    if !input.source_size.is_nonzero()
        || !input.fade_opacity.is_finite()
        || !input.transform.is_finite()
    {
        return None;
    }
    let transform = input.transform.clamped();
    let content_width =
        input.source_size.width as f32 * (1.0 - transform.crop_left - transform.crop_right);
    let content_height =
        input.source_size.height as f32 * (1.0 - transform.crop_top - transform.crop_bottom);
    if !content_width.is_finite()
        || !content_height.is_finite()
        || content_width <= 0.0
        || content_height <= 0.0
    {
        return None;
    }

    let project_width = project_size.width as f32;
    let project_height = project_size.height as f32;
    let fit_scale = (project_width / content_width).min(project_height / content_height);
    let fill_scale = (project_width / content_width).max(project_height / content_height);
    let (base_width, base_height) = match transform.sizing_mode {
        ClipSizingMode::Fit => (content_width * fit_scale, content_height * fit_scale),
        ClipSizingMode::Fill => (content_width * fill_scale, content_height * fill_scale),
        ClipSizingMode::Stretch => (project_width, project_height),
        ClipSizingMode::Original => (content_width, content_height),
    };
    let fit_width = content_width * fit_scale;
    let fit_height = content_height * fit_scale;
    let anchor_target = Point {
        x: project_width * 0.5 + transform.pos_x * fit_width * 0.5,
        y: project_height * 0.5 + transform.pos_y * fit_height * 0.5,
    };
    let anchor_local = Point {
        x: base_width * transform.anchor_x,
        y: base_height * transform.anchor_y,
    };
    let radians = transform.rotation_degrees.to_radians();
    let (mut sin, mut cos) = radians.sin_cos();
    // Keep cardinal rotations byte-for-byte stable for caches and tests.
    if sin.abs() < 1e-6 {
        sin = 0.0;
    }
    if cos.abs() < 1e-6 {
        cos = 0.0;
    }
    let positions = [
        Point { x: 0.0, y: 0.0 },
        Point {
            x: base_width,
            y: 0.0,
        },
        Point {
            x: base_width,
            y: base_height,
        },
        Point {
            x: 0.0,
            y: base_height,
        },
    ]
    .map(|point| {
        let scaled = Point {
            x: (point.x - anchor_local.x) * transform.scale_x,
            y: (point.y - anchor_local.y) * transform.scale_y,
        };
        let rotated = rotate_clockwise(scaled, Point { x: 0.0, y: 0.0 }, sin, cos);
        Point {
            x: anchor_target.x + rotated.x,
            y: anchor_target.y + rotated.y,
        }
    });

    let (left, right) = if transform.flip_h {
        (1.0 - transform.crop_right, transform.crop_left)
    } else {
        (transform.crop_left, 1.0 - transform.crop_right)
    };
    let (top, bottom) = if transform.flip_v {
        (1.0 - transform.crop_bottom, transform.crop_top)
    } else {
        (transform.crop_top, 1.0 - transform.crop_bottom)
    };
    let uvs = [
        Uv { u: left, v: top },
        Uv { u: right, v: top },
        Uv {
            u: right,
            v: bottom,
        },
        Uv { u: left, v: bottom },
    ];

    Some(CompositeQuad {
        clip_id: input.clip_id,
        positions,
        uvs,
        opacity: (transform.opacity * input.fade_opacity).clamp(0.0, 1.0),
    })
}

fn rotate_clockwise(point: Point, pivot: Point, sin: f32, cos: f32) -> Point {
    let x = point.x - pivot.x;
    let y = point.y - pivot.y;
    Point {
        x: pivot.x + x * cos - y * sin,
        y: pivot.y + x * sin + y * cos,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: ClipSizingMode) -> CompositionRequest {
        CompositionRequest {
            project_size: PixelSize::new(200, 100),
            layers: [
                Some(CompositeLayerInput {
                    clip_id: ClipId(1),
                    source_size: PixelSize::new(50, 50),
                    transform: ClipTransform {
                        sizing_mode: mode,
                        ..Default::default()
                    },
                    fade_opacity: 1.0,
                }),
                None,
                None,
                None,
            ],
        }
    }

    fn quad(request: CompositionRequest) -> CompositeQuad {
        plan_composition(request).unwrap().layers[0].unwrap()
    }

    #[test]
    fn sizing_modes_have_exact_geometry() {
        let fit = quad(request(ClipSizingMode::Fit));
        assert_eq!(
            fit.positions,
            [
                Point { x: 50.0, y: 0.0 },
                Point { x: 150.0, y: 0.0 },
                Point { x: 150.0, y: 100.0 },
                Point { x: 50.0, y: 100.0 },
            ]
        );
        let fill = quad(request(ClipSizingMode::Fill));
        assert_eq!(
            fill.positions,
            [
                Point { x: 0.0, y: -50.0 },
                Point { x: 200.0, y: -50.0 },
                Point { x: 200.0, y: 150.0 },
                Point { x: 0.0, y: 150.0 },
            ]
        );
        let stretch = quad(request(ClipSizingMode::Stretch));
        assert_eq!(
            stretch.positions,
            [
                Point { x: 0.0, y: 0.0 },
                Point { x: 200.0, y: 0.0 },
                Point { x: 200.0, y: 100.0 },
                Point { x: 0.0, y: 100.0 },
            ]
        );
        let original = quad(request(ClipSizingMode::Original));
        assert_eq!(
            original.positions,
            [
                Point { x: 75.0, y: 25.0 },
                Point { x: 125.0, y: 25.0 },
                Point { x: 125.0, y: 75.0 },
                Point { x: 75.0, y: 75.0 },
            ]
        );
    }

    #[test]
    fn center_and_noncenter_anchor_rotation_are_deterministic() {
        let mut centered = request(ClipSizingMode::Stretch);
        centered.layers[0]
            .as_mut()
            .unwrap()
            .transform
            .rotation_degrees = 90.0;
        let centered = quad(centered);
        assert_eq!(
            centered.positions,
            [
                Point { x: 150.0, y: -50.0 },
                Point { x: 150.0, y: 150.0 },
                Point { x: 50.0, y: 150.0 },
                Point { x: 50.0, y: -50.0 },
            ]
        );
        let mut anchored = request(ClipSizingMode::Stretch);
        let xf = &mut anchored.layers[0].as_mut().unwrap().transform;
        xf.rotation_degrees = 90.0;
        xf.anchor_x = 0.0;
        xf.anchor_y = 0.0;
        let anchored = quad(anchored);
        assert_eq!(
            anchored.positions,
            [
                Point { x: 100.0, y: 50.0 },
                Point { x: 100.0, y: 250.0 },
                Point { x: 0.0, y: 250.0 },
                Point { x: 0.0, y: 50.0 },
            ]
        );
    }

    #[test]
    fn noncenter_anchor_holds_its_target_during_scale() {
        let mut request = request(ClipSizingMode::Stretch);
        let transform = &mut request.layers[0].as_mut().unwrap().transform;
        transform.anchor_x = 0.0;
        transform.anchor_y = 0.0;
        transform.scale_x = 0.5;
        transform.scale_y = 0.5;
        let quad = quad(request);
        assert_eq!(
            quad.positions,
            [
                Point { x: 100.0, y: 50.0 },
                Point { x: 200.0, y: 50.0 },
                Point { x: 200.0, y: 100.0 },
                Point { x: 100.0, y: 100.0 },
            ]
        );
    }

    #[test]
    fn crop_flip_and_opacity_use_source_local_values() {
        let mut request = request(ClipSizingMode::Fit);
        let layer = request.layers[0].as_mut().unwrap();
        layer.transform.crop_left = 0.1;
        layer.transform.crop_right = 0.2;
        layer.transform.crop_top = 0.25;
        layer.transform.crop_bottom = 0.1;
        layer.transform.flip_h = true;
        layer.transform.flip_v = true;
        layer.transform.opacity = 0.5;
        layer.fade_opacity = 0.4;
        let quad = quad(request);
        assert_eq!(
            quad.uvs,
            [
                Uv { u: 0.8, v: 0.9 },
                Uv { u: 0.1, v: 0.9 },
                Uv { u: 0.1, v: 0.25 },
                Uv { u: 0.8, v: 0.25 },
            ]
        );
        assert_eq!(quad.opacity, 0.2);
    }

    #[test]
    fn fixed_order_and_capacity_are_preserved() {
        let mut request = request(ClipSizingMode::Fit);
        for (slot, id) in [3, 8, 13, 21].into_iter().enumerate() {
            request.layers[slot] = Some(CompositeLayerInput {
                clip_id: ClipId(id),
                source_size: PixelSize::new(100, 100),
                transform: ClipTransform::default(),
                fade_opacity: 1.0,
            });
        }
        let plan = plan_composition(request).unwrap();
        assert_eq!(
            plan.layers.map(|quad| quad.unwrap().clip_id),
            [ClipId(3), ClipId(8), ClipId(13), ClipId(21)]
        );
    }

    #[test]
    fn invalid_dimensions_and_nonfinite_inputs_are_rejected() {
        let mut invalid_size = request(ClipSizingMode::Fit);
        invalid_size.project_size.width = 0;
        assert!(plan_composition(invalid_size).is_none());
        let mut invalid_opacity = request(ClipSizingMode::Fit);
        invalid_opacity.layers[0].as_mut().unwrap().fade_opacity = f32::NAN;
        assert!(plan_composition(invalid_opacity).unwrap().layers[0].is_none());
    }

    #[test]
    fn invalid_layer_is_transparent_without_suppressing_ready_layers() {
        let mut request = request(ClipSizingMode::Fit);
        for (slot, id) in [3, 8, 13, 21].into_iter().enumerate() {
            request.layers[slot] = Some(CompositeLayerInput {
                clip_id: ClipId(id),
                source_size: PixelSize::new(100, 100),
                transform: ClipTransform::default(),
                fade_opacity: 1.0,
            });
        }
        request.layers[2].as_mut().unwrap().source_size.height = 0;
        let plan = plan_composition(request).unwrap();
        assert_eq!(plan.layers[0].unwrap().clip_id, ClipId(3));
        assert_eq!(plan.layers[1].unwrap().clip_id, ClipId(8));
        assert!(plan.layers[2].is_none());
        assert_eq!(plan.layers[3].unwrap().clip_id, ClipId(21));
    }

    #[test]
    fn video_fade_curve_is_shared_and_reaches_exact_endpoints() {
        let positive = Fade {
            duration: Tick(1_000_000),
            curve: 1.0,
        };
        let negative = Fade {
            duration: Tick(1_000_000),
            curve: -1.0,
        };
        assert_eq!(fade_envelope_value(positive, 0.0), 0.0);
        assert_eq!(fade_envelope_value(positive, 1.0), 1.0);
        assert!((fade_envelope_value(positive, 0.5) - 0.75).abs() < 1e-6);
        assert!((fade_envelope_value(negative, 0.5) - 0.25).abs() < 1e-6);
        assert_eq!(video_fade_opacity(0.0), 0.0);
        assert_eq!(video_fade_opacity(1.0), 1.0);

        let clip = Clip::new(ClipData {
            id: ClipId(1),
            media: nle_timeline::MediaId(1),
            track_id: nle_timeline::TrackId(1),
            link_id: None,
            enabled: true,
            start: Tick(0),
            duration: Tick(4_000_000),
            source_in: Tick(0),
            gain_db: 0.0,
            gain_left_db: 0.0,
            gain_right_db: 0.0,
            effects: Vec::new(),
            video_effects: Vec::new().into(),
            transform: ClipTransform::default(),
            fade_in: positive,
            fade_out: positive,
        });
        assert_eq!(video_opacity_at(&clip, Tick(0)), 0.0);
        assert_eq!(video_opacity_at(&clip, Tick(2_000_000)), 1.0);
        assert_eq!(video_opacity_at(&clip, Tick(4_000_000)), 0.0);
    }
}
