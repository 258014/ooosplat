use serde::Serialize;

use crate::{
    engines::MapperBackend,
    presets::Quality,
    video::{expected_kept_frames, resolved_filter_config, FramePlan, VideoInfo},
};

const INCREMENTAL_RECONSTRUCTION_COEFFICIENT: f64 = 176.0;
const GLOBAL_RECONSTRUCTION_COEFFICIENT: f64 = 40.0;

/// Number of images a video run actually hands to COLMAP.
///
/// Smart frame selection oversamples the extraction by 1.5x and the filter then
/// keeps a bounded number of frames per window, so neither the matcher nor the
/// mapper ever sees `plan.estimated_frames`. Every component that scales with
/// images has to be estimated from the post-filter count, otherwise the estimate
/// is inflated by the oversampling factor and by the rejected frames.
///
/// Image-sequence inputs skip smart filtering entirely (`smart_filter_enabled`
/// requires a video), so this rule only applies to video runs.
pub fn expected_mapped_frames(plan: &FramePlan, quality: Quality) -> u64 {
    let preset = quality.preset();
    if !preset.enable_smart_filter {
        return plan.estimated_frames.max(1);
    }
    expected_kept_frames(
        plan.estimated_frames,
        &resolved_filter_config(&preset, plan.sampling_fps),
    )
}

fn reconstruction_estimate_ms(frames: u64, backend: MapperBackend) -> f64 {
    let frame_count = frames.max(1) as f64;
    // TODO: regress these coefficients against S0 mapper timing baselines.
    match backend {
        MapperBackend::Incremental => {
            INCREMENTAL_RECONSTRUCTION_COEFFICIENT * frame_count.powf(1.5)
        }
        MapperBackend::Global => GLOBAL_RECONSTRUCTION_COEFFICIENT * frame_count.powf(1.1),
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeSample {
    pub quality: Quality,
    /// Mapper backend this historical run actually used.
    ///
    /// The two backends have separate cost models, so a sample only calibrates
    /// machine speed when it is normalised by the model that produced it;
    /// otherwise the ratio measures the gap between the two models instead.
    pub backend: MapperBackend,
    /// Images this historical run actually fed to COLMAP, measured on the same
    /// basis as the estimate so the calibration ratio stays meaningful.
    pub mapped_frames: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimateConfidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEstimate {
    pub estimated_ms: u64,
    pub lower_bound_ms: u64,
    pub upper_bound_ms: u64,
    pub confidence: EstimateConfidence,
    pub sample_count: usize,
    pub basis: String,
}

pub fn estimate_runtime(
    video: &VideoInfo,
    plan: &FramePlan,
    quality: Quality,
    samples: &[RuntimeSample],
) -> RuntimeEstimate {
    estimate_runtime_with_backend(video, plan, quality, samples, MapperBackend::Global)
}

pub fn estimate_runtime_with_backend(
    video: &VideoInfo,
    plan: &FramePlan,
    quality: Quality,
    samples: &[RuntimeSample],
    backend: MapperBackend,
) -> RuntimeEstimate {
    estimate_runtime_with_backend_and_mapped_frames(
        video,
        quality,
        samples,
        backend,
        expected_mapped_frames(plan, quality),
    )
}

/// Estimates from the *observed* number of images a run fed to COLMAP.
///
/// Use this when a pipeline already recorded its real frame counts: the count is
/// a measurement, not a prediction, so it must not be re-derived from the plan.
pub fn estimate_runtime_with_backend_and_mapped_frames(
    video: &VideoInfo,
    quality: Quality,
    samples: &[RuntimeSample],
    backend: MapperBackend,
    mapped_frames: u64,
) -> RuntimeEstimate {
    estimate_runtime_for_input(
        video.total_frames,
        mapped_frames,
        quality,
        samples,
        backend,
        "视频总帧",
    )
}

pub fn estimate_runtime_for_images(
    image_count: u64,
    plan: &FramePlan,
    quality: Quality,
    samples: &[RuntimeSample],
) -> RuntimeEstimate {
    estimate_runtime_for_images_with_backend(
        image_count,
        plan,
        quality,
        samples,
        MapperBackend::Global,
    )
}

pub fn estimate_runtime_for_images_with_backend(
    image_count: u64,
    plan: &FramePlan,
    quality: Quality,
    samples: &[RuntimeSample],
    backend: MapperBackend,
) -> RuntimeEstimate {
    // Image sequences are never filtered, so every input image reaches COLMAP.
    let mut estimate = estimate_runtime_for_input(
        image_count,
        plan.estimated_frames.max(1),
        quality,
        samples,
        backend,
        "输入图片",
    );
    estimate.basis = if estimate.sample_count == 0 {
        format!("根据 {image_count} 张输入图片和质量档位估算；完成任务后会自动校准")
    } else {
        format!(
            "根据 {image_count} 张输入图片、质量档位和本机 {} 个历史任务校准",
            estimate.sample_count
        )
    };
    estimate
}

fn estimate_runtime_for_input(
    source_count: u64,
    mapped_frames: u64,
    quality: Quality,
    samples: &[RuntimeSample],
    backend: MapperBackend,
    _source_label: &str,
) -> RuntimeEstimate {
    let mapped_frames = mapped_frames.max(1);
    let base = base_estimate_ms_with_backend(mapped_frames, quality, backend);
    let valid_samples = samples
        .iter()
        .filter(|sample| sample.duration_ms >= 10_000 && sample.mapped_frames > 0)
        .collect::<Vec<_>>();
    let same_quality = valid_samples
        .iter()
        .copied()
        .filter(|sample| sample.quality == quality)
        .collect::<Vec<_>>();
    let nearby_same_quality = same_quality
        .iter()
        .copied()
        .filter(|sample| {
            let smaller = sample.mapped_frames.min(mapped_frames).max(1) as f64;
            let larger = sample.mapped_frames.max(mapped_frames).max(1) as f64;
            larger / smaller <= 1.25
        })
        .collect::<Vec<_>>();
    let (calibration_source, calibration_label) = if !nearby_same_quality.is_empty() {
        (nearby_same_quality, "同档位、相近帧数")
    } else if !same_quality.is_empty() {
        (same_quality, "同档位")
    } else {
        (valid_samples, "跨档位")
    };
    let mut calibration = calibration_source
        .into_iter()
        .map(|sample| {
            let expected =
                base_estimate_ms_with_backend(sample.mapped_frames, sample.quality, sample.backend);
            (sample.duration_ms as f64 / expected.max(1) as f64).clamp(0.15, 5.0)
        })
        .collect::<Vec<_>>();
    calibration.sort_by(f64::total_cmp);
    let sample_count = calibration.len();
    let factor = median(&calibration).unwrap_or(1.0);
    let estimated_ms = (base as f64 * factor).round().max(1_000.0) as u64;
    let (confidence, lower_factor, upper_factor) = match sample_count {
        0 => (EstimateConfidence::Low, 0.55, 1.75),
        1..=2 => (EstimateConfidence::Low, 0.60, 1.60),
        3..=5 => (EstimateConfidence::Medium, 0.72, 1.38),
        _ => (EstimateConfidence::High, 0.82, 1.22),
    };
    RuntimeEstimate {
        estimated_ms,
        lower_bound_ms: (estimated_ms as f64 * lower_factor).round() as u64,
        upper_bound_ms: (estimated_ms as f64 * upper_factor).round() as u64,
        confidence,
        sample_count,
        basis: if sample_count == 0 {
            format!(
                "根据输入 {} 总帧、实际进入 COLMAP 的 {} 帧和质量档位估算；完成任务后会自动校准",
                source_count, mapped_frames
            )
        } else {
            format!(
                "根据输入 {} 总帧、实际进入 COLMAP 的 {} 帧、质量档位和本机 {sample_count} 个{calibration_label}任务校准",
                source_count, mapped_frames,
            )
        },
    }
}

#[cfg(test)]
fn base_estimate_ms(frames: u64, quality: Quality) -> u64 {
    base_estimate_ms_with_backend(frames, quality, MapperBackend::Global)
}

fn base_estimate_ms_with_backend(frames: u64, quality: Quality, backend: MapperBackend) -> u64 {
    let mapped_count = frames.max(1) as f64;
    // Every component scales with the images that actually reach COLMAP: frame
    // extraction, feature extraction, matching and the mapper all consume the
    // same post-filter frame set.
    let preparation_ms = 8_000.0 + mapped_count * 55.0;
    let reconstruction_ms = reconstruction_estimate_ms(mapped_count as u64, backend);
    let brush_ms = estimate_brush_stage_ms(quality) as f64;
    (preparation_ms + reconstruction_ms + brush_ms).round() as u64
}

/// Brush v0.3.0 does not expose its current training step on stdout/stderr.
/// This duration model is therefore used only to provide a clearly labelled,
/// best-effort progress indicator while the process is alive.
pub(crate) fn estimate_brush_stage_ms(quality: Quality) -> u64 {
    let preset = quality.preset();
    let resolution_factor = (preset.brush_max_resolution as f64 / 960.0).powf(1.35);
    let iteration_factor = preset.brush_iterations as f64 / 6_000.0;
    (80_000.0 * resolution_factor * iteration_factor)
        .round()
        .max(1_000.0) as u64
}

pub(crate) fn estimate_calibrated_brush_stage_ms(
    video: &VideoInfo,
    plan: &FramePlan,
    quality: Quality,
    samples: &[RuntimeSample],
    backend: MapperBackend,
) -> u64 {
    let base_total_ms =
        base_estimate_ms_with_backend(expected_mapped_frames(plan, quality), quality, backend)
            .max(1);
    let calibrated_total_ms =
        estimate_runtime_with_backend(video, plan, quality, samples, backend).estimated_ms;
    let calibration = calibrated_total_ms as f64 / base_total_ms as f64;
    (estimate_brush_stage_ms(quality) as f64 * calibration)
        .round()
        .max(1_000.0) as u64
}

pub(crate) fn estimate_calibrated_brush_stage_ms_for_images(
    image_count: u64,
    plan: &FramePlan,
    quality: Quality,
    samples: &[RuntimeSample],
    backend: MapperBackend,
) -> u64 {
    let base_total_ms =
        base_estimate_ms_with_backend(plan.estimated_frames, quality, backend).max(1);
    let calibrated_total_ms = estimate_runtime_for_input(
        image_count,
        plan.estimated_frames.max(1),
        quality,
        samples,
        backend,
        "输入图片",
    )
    .estimated_ms;
    let calibration = calibrated_total_ms as f64 / base_total_ms as f64;
    (estimate_brush_stage_ms(quality) as f64 * calibration)
        .round()
        .max(1_000.0) as u64
}

fn median(values: &[f64]) -> Option<f64> {
    match values.len() {
        0 => None,
        length if length % 2 == 1 => Some(values[length / 2]),
        length => Some((values[length / 2 - 1] + values[length / 2]) / 2.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{FrameSelectionStrategy, SmartFrameSelection};

    fn video() -> VideoInfo {
        VideoInfo {
            duration: 12.52,
            width: 3840,
            height: 2160,
            fps: 60.0,
            total_frames: 752,
            codec: "hevc".into(),
            rotation: 0,
            pixel_format: "yuv420p".into(),
            has_alpha: false,
        }
    }

    /// A frame plan whose planned extraction count is the value under test.
    fn plan_with_frames(estimated_frames: u64) -> FramePlan {
        FramePlan {
            retention_ratio: 0.5,
            sampling_fps: 30.0,
            estimated_frames,
        }
    }

    #[test]
    fn mapper_backend_estimate_prefers_global_complexity() {
        let global = base_estimate_ms_with_backend(2_000, Quality::Balanced, MapperBackend::Global);
        let incremental =
            base_estimate_ms_with_backend(2_000, Quality::Balanced, MapperBackend::Incremental);
        assert!(global < incremental);
        assert!(incremental > global * 2);
    }

    #[test]
    fn quality_and_frame_count_increase_the_estimate() {
        let fast = base_estimate_ms(48, Quality::Fast);
        let balanced = base_estimate_ms(72, Quality::Balanced);
        let high = base_estimate_ms(120, Quality::High);
        assert!(fast < balanced && balanced < high);
    }

    #[test]
    fn calibration_normalises_each_sample_by_its_own_backend() {
        let video = video();
        let plan = plan_with_frames(533);
        let mapped = expected_mapped_frames(&plan, Quality::Balanced);
        let global_base =
            base_estimate_ms_with_backend(mapped, Quality::Balanced, MapperBackend::Global);
        let incremental_base =
            base_estimate_ms_with_backend(mapped, Quality::Balanced, MapperBackend::Incremental);
        assert!(
            reconstruction_estimate_ms(mapped, MapperBackend::Incremental)
                > reconstruction_estimate_ms(mapped, MapperBackend::Global) * 2.0,
            "the two backend models must differ enough for the mistake to matter"
        );

        // A historical run of the same size on this same machine, but using the
        // other backend. Normalising it by the estimate's backend would read that
        // model gap as machine speed and inflate every later estimate.
        let sample = RuntimeSample {
            quality: Quality::Balanced,
            backend: MapperBackend::Incremental,
            mapped_frames: mapped,
            duration_ms: incremental_base,
        };
        let estimate = estimate_runtime_with_backend(
            &video,
            &plan,
            Quality::Balanced,
            &[sample],
            MapperBackend::Global,
        );
        assert_eq!(
            estimate.estimated_ms, global_base,
            "a machine-speed ratio of 1.0 must leave the estimate on its own model"
        );
    }

    #[test]
    fn estimates_use_the_post_filter_frame_count() {
        let plan = plan_with_frames(533);
        let quality = Quality::Balanced;
        let mapped = expected_mapped_frames(&plan, quality);
        // Smart frame selection oversamples before filtering, so the frames that
        // reach COLMAP are strictly fewer than the planned extraction count.
        assert!(mapped < plan.estimated_frames);
        assert_eq!(
            mapped,
            expected_kept_frames(
                plan.estimated_frames,
                &resolved_filter_config(&quality.preset(), plan.sampling_fps)
            )
        );

        let estimate = estimate_runtime(&video(), &plan, quality, &[]);
        assert_eq!(
            estimate.estimated_ms,
            base_estimate_ms_with_backend(mapped, quality, MapperBackend::Global)
        );
        assert!(estimate.basis.contains("实际进入 COLMAP"));
    }

    #[test]
    fn higher_quality_presets_map_more_frames() {
        // 每档的抽帧数并不相同：目标密度越高，候选密度与最终保留数都更高。
        // 早前这里把三档的 estimated_frames 钉成同一个值，掩盖了真实关系。
        let video = VideoInfo {
            duration: 60.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 1800,
            codec: "h264".into(),
            rotation: 0,
            pixel_format: "yuv420p".into(),
            has_alpha: false,
        };
        let plans = [Quality::Fast, Quality::Balanced, Quality::High]
            .map(|quality| SmartFrameSelection.create_plan(&video, &quality.preset()));
        let fast = expected_mapped_frames(&plans[0], Quality::Fast);
        let balanced = expected_mapped_frames(&plans[1], Quality::Balanced);
        let high = expected_mapped_frames(&plans[2], Quality::High);
        assert!(fast > 0, "每档都必须保留可用的帧数");
        assert!(
            fast < balanced && balanced < high,
            "{fast} {balanced} {high}"
        );
        assert!(high <= plans[2].estimated_frames);
    }

    #[test]
    fn brush_stage_estimate_increases_with_the_quality_preset() {
        assert!(
            estimate_brush_stage_ms(Quality::Fast) < estimate_brush_stage_ms(Quality::Balanced)
        );
        assert!(
            estimate_brush_stage_ms(Quality::Balanced) < estimate_brush_stage_ms(Quality::High)
        );
    }

    #[test]
    fn brush_stage_estimate_uses_the_same_local_history_calibration() {
        let video = video();
        let plan = plan_with_frames(533);
        let mapped = expected_mapped_frames(&plan, Quality::Balanced);
        let base_total = base_estimate_ms(mapped, Quality::Balanced);
        let sample = RuntimeSample {
            quality: Quality::Balanced,
            backend: MapperBackend::Global,
            mapped_frames: mapped,
            duration_ms: base_total * 2,
        };

        let calibrated = estimate_calibrated_brush_stage_ms(
            &video,
            &plan,
            Quality::Balanced,
            &[sample],
            MapperBackend::Global,
        );
        assert_eq!(calibrated, estimate_brush_stage_ms(Quality::Balanced) * 2);
    }

    #[test]
    fn completed_local_runs_calibrate_and_narrow_the_range() {
        let video = video();
        let plan = FramePlan {
            retention_ratio: 0.064,
            sampling_fps: 3.83,
            estimated_frames: 48,
        };
        let sample = RuntimeSample {
            quality: Quality::Fast,
            backend: MapperBackend::Global,
            mapped_frames: 226,
            duration_ms: 858_613,
        };
        let estimate = estimate_runtime(
            &video,
            &plan,
            Quality::Fast,
            &[sample.clone(), sample.clone(), sample],
        );
        assert_eq!(estimate.confidence, EstimateConfidence::Medium);
        assert_eq!(estimate.sample_count, 3);
        assert!(estimate.lower_bound_ms < estimate.estimated_ms);
        assert!(estimate.upper_bound_ms > estimate.estimated_ms);
    }

    #[test]
    fn same_quality_samples_take_priority_and_use_the_median() {
        let video = video();
        let plan = plan_with_frames(533);
        let mapped = expected_mapped_frames(&plan, Quality::Balanced);
        let samples = [
            RuntimeSample {
                quality: Quality::Balanced,
                backend: MapperBackend::Global,
                mapped_frames: mapped,
                duration_ms: base_estimate_ms(mapped, Quality::Balanced) * 2,
            },
            RuntimeSample {
                quality: Quality::Fast,
                backend: MapperBackend::Global,
                mapped_frames: 320,
                duration_ms: 374_000,
            },
            RuntimeSample {
                quality: Quality::High,
                backend: MapperBackend::Global,
                mapped_frames: 416,
                duration_ms: 10_464_000,
            },
        ];
        let estimate = estimate_runtime(&video, &plan, Quality::Balanced, &samples);
        assert_eq!(estimate.sample_count, 1);
        assert_eq!(
            estimate.estimated_ms,
            base_estimate_ms(mapped, Quality::Balanced) * 2
        );
        assert!(estimate.basis.contains("1 个同档位、相近帧数任务"));
    }

    #[test]
    fn nearby_frame_counts_do_not_mix_unrelated_runs() {
        let video = video();
        let plan = plan_with_frames(533);
        let mapped = expected_mapped_frames(&plan, Quality::Fast);
        let base = base_estimate_ms(mapped, Quality::Fast);
        // Durations are taken relative to the base model so the calibration ratios
        // stay inside the clamp and the median is exactly 1.5.
        let samples = [
            RuntimeSample {
                quality: Quality::Fast,
                backend: MapperBackend::Global,
                mapped_frames: mapped,
                duration_ms: base,
            },
            RuntimeSample {
                quality: Quality::Fast,
                backend: MapperBackend::Global,
                mapped_frames: mapped,
                duration_ms: base * 2,
            },
            RuntimeSample {
                quality: Quality::Fast,
                backend: MapperBackend::Global,
                mapped_frames: mapped * 4,
                duration_ms: base * 2,
            },
        ];
        let estimate = estimate_runtime(&video, &plan, Quality::Fast, &samples);
        // The far run is >25% away, so it must not join the calibration set: had
        // it been included the median ratio would be 2.0, not 1.5.
        assert_eq!(estimate.sample_count, 2);
        assert_eq!(estimate.estimated_ms, (base as f64 * 1.5).round() as u64);
        assert!(estimate.basis.contains("相近帧数"));
    }
}
