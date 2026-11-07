//! Evaluated curves/wheels/HSL data shared by CPU reference and the GPU effect stage.
//! Curve tables use the CPU renderer's exact 1024-sample monotone/periodic interpolation.
use crate::effects::{FxCtx, b, choice, curve_lut, curve_param, f, hue_lut, is_identity_curve, on, vs_sat_lut, wheel_rgb};
use filmcraft_project::EffectInstance;

pub const CURVE_SAMPLES: usize = 1024;
#[derive(Clone, Debug, PartialEq)]
pub struct HslSecondary {
    /// Hue center/range, saturation minimum, luma minimum/maximum, softness.
    pub key: [f32; 6],
    /// Temperature, tint, saturation multiplier, hue shift (turns).
    pub correction: [f32; 4],
    pub show: u32,
    pub denoise: f32,
    pub median_radius: u32,
    pub blur_x: Vec<u32>,
    pub blur_y: Vec<u32>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LumetriExtra {
    /// Master/R/G/B, Hue-vs-Sat/Hue/Luma, Luma-vs-Sat, Sat-vs-Sat. Empty = bypass.
    pub curves: [Vec<f32>; 9],
    pub wheels: [[f32; 3]; 3],
    pub lightness: [f32; 3],
    pub hsl: Option<HslSecondary>,
    pub look_lut: Option<std::sync::Arc<filmcraft_color::Lut>>,
    pub look_intensity: f32,
    effect: EffectInstance,
    time: filmcraft_time::Tick,
    scale: f32,
    working: filmcraft_color::WorkingSpace,
    valid: bool,
}
pub(crate) fn valid_inputs(e: &EffectInstance) -> bool {
    e.params.values().all(|p| {
        let valid = |v: &filmcraft_project::ParamValue| match v {
            filmcraft_project::ParamValue::Float(v) => v.is_finite() && v.abs() <= f64::from(f32::MAX),
            filmcraft_project::ParamValue::Vec2(v) => v.x.is_finite() && v.y.is_finite() && v.x.abs() <= 1.0e7 && v.y.abs() <= 1.0e7,
            filmcraft_project::ParamValue::Color(v) => v.iter().all(|v| v.is_finite()),
            filmcraft_project::ParamValue::Curve(v) => v.len() <= 4096 && v.iter().flatten().all(|v| v.is_finite() && v.abs() <= 1.0e6),
            _ => true,
        };
        valid(&p.value) && p.keyframes.len() <= 16_384 && p.keyframes.iter().all(|k| valid(&k.value))
    })
}
impl LumetriExtra {
    pub(crate) fn eval(e: &EffectInstance, cx: &FxCtx, w: usize, h: usize) -> Self {
        let valid = valid_inputs(e);
        let mut curves: [Vec<f32>; 9] = std::array::from_fn(|_| Vec::new());
        if valid && on(e, "curves_on") {
            for (slot, id) in curves.iter_mut().take(4).zip(["curve_luma", "curve_red", "curve_green", "curve_blue"]) {
                if let Some(c) = curve_param(e, id).filter(|c| !is_identity_curve(c)) {
                    *slot = curve_lut(&c, CURVE_SAMPLES);
                }
            }
            for (slot, id) in curves.iter_mut().skip(4).zip(["hue_vs_sat", "hue_vs_hue", "hue_vs_luma", "luma_vs_sat", "sat_vs_sat"]) {
                if let Some(c) = curve_param(e, id) {
                    *slot =
                        if matches!(id, "luma_vs_sat" | "sat_vs_sat") { vs_sat_lut(&c, CURVE_SAMPLES) } else { hue_lut(&c, CURVE_SAMPLES) }.unwrap_or_default();
                }
            }
        }
        let mut wheels = [[0.0; 3]; 3];
        let mut lightness = [0.0; 3];
        if on(e, "wheels_on") {
            for (slot, id) in wheels.iter_mut().zip(["wheel_shadows", "wheel_midtones", "wheel_highlights"]) {
                *slot = wheel_rgb(e.param(id).map(|p| p.vec2_at(cx.t)).unwrap_or_default());
            }
            for (slot, id) in lightness.iter_mut().zip(["wheel_shadows_l", "wheel_midtones_l", "wheel_highlights_l"]) {
                *slot = f(e, id, cx) / 100.0;
            }
        }
        let hsl = b(e, "hsl_on").then(|| {
            let denoise = f(e, "hsl_denoise", cx).clamp(0.0, 100.0) / 100.0;
            let blur = f(e, "hsl_blur", cx).clamp(0.0, 100.0) / 100.0;
            let sigma = blur * 20.0 * cx.px_scale;
            let (blur_x, blur_y) = if sigma >= 0.3 { crate::effects::gaussian_boxes(w, h, sigma, sigma) } else { (Vec::new(), Vec::new()) };
            HslSecondary {
                key: [
                    f(e, "hsl_hue", cx) / 360.0,
                    (f(e, "hsl_hue_range", cx) / 360.0).max(1e-3),
                    f(e, "hsl_sat_min", cx) / 100.0,
                    f(e, "hsl_luma_min", cx) / 100.0,
                    f(e, "hsl_luma_max", cx) / 100.0,
                    (f(e, "hsl_soft", cx) / 100.0 * 0.3).max(0.01),
                ],
                correction: [f(e, "hsl_temp", cx) / 100.0, f(e, "hsl_tint", cx) / 100.0, f(e, "hsl_sat", cx) / 100.0, f(e, "hsl_hue_shift", cx) / 360.0],
                show: choice(e, "hsl_show_mask"),
                denoise,
                median_radius: ((1.0 + 2.0 * denoise) * cx.px_scale).round().max(1.0) as u32,
                blur_x,
                blur_y,
            }
        });
        let look_lut = if on(e, "creative_on") { crate::luts::resolve(cx.project, crate::effects::text(e, "look_lut")) } else { None };
        let look_intensity = f(e, "look_intensity", cx) / 100.0;
        Self { curves, wheels, lightness, hsl, look_lut, look_intensity, effect: e.clone(), time: cx.t, scale: cx.px_scale, working: cx.working, valid }
    }
    pub fn gpu_ok(&self) -> bool {
        let finite = |values: &[f32]| values.iter().all(|v| v.is_finite());
        self.valid
            && self.scale.is_finite()
            && self.curves.iter().all(|v| (v.is_empty() || v.len() == CURVE_SAMPLES) && finite(v))
            && self.wheels.iter().all(|v| finite(v))
            && finite(&self.lightness)
            && self.look_intensity.is_finite()
            && self.look_lut.as_ref().is_none_or(|lut| crate::gpufx::lut_gpu_ok(lut))
            && self.hsl.as_ref().is_none_or(|h| finite(&h.key) && finite(&h.correction) && h.denoise.is_finite() && h.median_radius <= 3 && h.show <= 3)
    }
    pub(crate) fn apply(&self, image: &mut crate::Image) {
        let cx = FxCtx {
            t: self.time,
            px_scale: self.scale,
            seconds: 0.0,
            timecode: "",
            clip_name: "",
            project: None,
            env: None,
            frame_rate: 24.0,
            working: self.working,
        };
        crate::effects::lumetri_advanced_with_lut(image, &self.effect, &cx, self.look_lut.as_deref());
        crate::effects::hsl_secondary(image, &self.effect, &cx);
    }
}
