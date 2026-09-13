use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

use crate::{engines::colmap::MapperBackend, video::FrameFilterConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MapperPreference {
    /// Try the global mapper first, falling back to the incremental one when it
    /// fails, produces an invalid model, or registers fewer than 60% of images.
    PreferGlobal,
    /// Never try the global mapper.
    ForceIncremental,
}

impl MapperPreference {
    pub const fn backend(self, global_available: bool) -> MapperBackend {
        match self {
            Self::PreferGlobal if global_available => MapperBackend::Global,
            _ => MapperBackend::Incremental,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Fast,
    #[default]
    Balanced,
    High,
}

/// Brush training knobs that trade a little quality for training time.
///
/// They are deliberately gated to a single preset instead of being applied to
/// every tier: the savings grow with training length, so only the High preset
/// uses them, while Fast and Balanced keep Brush's own defaults untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrushTuning {
    /// `Some(order)` overrides Brush's `--sh-degree` (default 3). Lowering the
    /// spherical-harmonics order speeds training up but drops view-dependent
    /// effects such as reflections and highlights.
    pub sh_degree: Option<u32>,
    /// `Some(step)` stops densification at that step. Brush stops at 15000 by
    /// default, so this may only ever shorten growth, never extend it.
    pub growth_stop_iter: Option<usize>,
    /// `Some(steps)` overrides Brush's `--refine-every` (default 200). Brush ties
    /// this value to how many images cover the scene, so leave it unset unless a
    /// measurement justifies changing it.
    pub refine_every: Option<usize>,
    /// `Some(count)` caps the gaussian count. Set too low it loses detail, so it
    /// stays unset unless a measurement justifies a cap.
    pub max_splats: Option<usize>,
    /// Export once at the end instead of every few thousand steps. Intermediate
    /// exports copy the whole splat set from device to host and, with a fixed
    /// export name, only overwrite the same file.
    pub single_export: bool,
}

impl BrushTuning {
    /// Brush's own defaults: every knob stays unset so nothing is overridden.
    pub const fn brush_defaults() -> Self {
        Self {
            sh_degree: None,
            growth_stop_iter: None,
            refine_every: None,
            max_splats: None,
            single_export: false,
        }
    }

    /// Tuning for the High preset, where the longest training run makes the
    /// savings worth the quality trade-off.
    pub const fn high_detail() -> Self {
        Self {
            // Keeps some view-dependent shading rather than dropping to degree 1.
            sh_degree: Some(2),
            // Shortens densification (Brush's default is 15000) while leaving
            // room for the late iterations that only refine existing splats.
            growth_stop_iter: Some(12_000),
            refine_every: None,
            max_splats: None,
            single_export: true,
        }
    }

    /// Whether any knob overrides Brush's defaults.
    pub const fn is_brush_defaults(self) -> bool {
        self.sh_degree.is_none()
            && self.growth_stop_iter.is_none()
            && self.refine_every.is_none()
            && self.max_splats.is_none()
            && !self.single_export
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityPreset {
    pub frame_retention_ratio: f64,
    pub brush_iterations: usize,
    pub brush_max_resolution: u32,
    pub brush_tuning: BrushTuning,
    pub enable_smart_filter: bool,
    pub smart_filter_config: FrameFilterConfig,
    pub mapper_backend: MapperPreference,
    pub sequential_overlap: u32,
    pub feature_max_image_size: u32,
    pub feature_max_num_features: u32,
}

impl Quality {
    pub const fn preset(self) -> QualityPreset {
        match self {
            Self::Fast => QualityPreset {
                frame_retention_ratio: 0.30,
                brush_iterations: 8_000,
                brush_max_resolution: 1_200,
                brush_tuning: BrushTuning::brush_defaults(),
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::fast(),
                mapper_backend: MapperPreference::PreferGlobal,
                sequential_overlap: 12,
                feature_max_image_size: 1280,
                feature_max_num_features: 8192,
            },
            Self::Balanced => QualityPreset {
                frame_retention_ratio: 0.50,
                brush_iterations: 15_000,
                brush_max_resolution: 1_600,
                brush_tuning: BrushTuning::brush_defaults(),
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::balanced(),
                // The global mapper is being trialled on the Fast preset only.
                // Measured on phone-orbit footage the incremental mapper
                // registered 3/164 and 7/82 images where the global mapper
                // registered 164/164 and 78/82, so this tier is expected to
                // reconstruct worse and to fail outright on such material until
                // the trial is widened to it.
                mapper_backend: MapperPreference::ForceIncremental,
                sequential_overlap: 15,
                feature_max_image_size: 1600,
                feature_max_num_features: 8192,
            },
            Self::High => QualityPreset {
                frame_retention_ratio: 1.00,
                brush_iterations: 30_000,
                brush_max_resolution: 2_000,
                brush_tuning: BrushTuning::high_detail(),
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::high(),
                // Same trial scope as Balanced: see the note there.
                mapper_backend: MapperPreference::ForceIncremental,
                sequential_overlap: 20,
                feature_max_image_size: 2000,
                feature_max_num_features: 16384,
            },
        }
    }
}

impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::High => "high",
        })
    }
}

impl FromStr for Quality {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "high" => Ok(Self::High),
            _ => Err(format!("unknown quality preset: {value}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_centralized_and_exact() {
        assert_eq!(Quality::Fast.preset().frame_retention_ratio, 0.30);
        assert_eq!(Quality::Balanced.preset().frame_retention_ratio, 0.50);
        assert_eq!(Quality::High.preset().frame_retention_ratio, 1.00);
        assert_eq!(Quality::Fast.preset().brush_iterations, 8_000);
        assert_eq!(Quality::Balanced.preset().brush_max_resolution, 1_600);
        assert!(Quality::Fast.preset().enable_smart_filter);
        assert_eq!(
            Quality::Fast.preset().smart_filter_config.keep_per_window,
            2
        );
        assert_eq!(
            Quality::Balanced
                .preset()
                .smart_filter_config
                .keep_per_window,
            3
        );
        assert_eq!(
            Quality::High.preset().smart_filter_config.keep_per_window,
            4
        );
    }

    #[test]
    fn matching_and_feature_limits_follow_the_quality_ladder() {
        assert_eq!(Quality::Fast.preset().sequential_overlap, 12);
        assert_eq!(Quality::Balanced.preset().sequential_overlap, 15);
        assert_eq!(Quality::High.preset().sequential_overlap, 20);
        assert_eq!(Quality::Fast.preset().feature_max_image_size, 1280);
        assert_eq!(Quality::Balanced.preset().feature_max_image_size, 1600);
        assert_eq!(Quality::High.preset().feature_max_image_size, 2_000);
        assert_eq!(Quality::Fast.preset().feature_max_num_features, 8192);
        assert_eq!(Quality::Balanced.preset().feature_max_num_features, 8192);
        assert_eq!(Quality::High.preset().feature_max_num_features, 16_384);
    }

    #[test]
    fn feature_count_never_drops_below_the_pose_accuracy_floor() {
        // Below 8192 features per image, pose accuracy measurably degrades.
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            assert!(quality.preset().feature_max_num_features >= 8192);
        }
    }

    #[test]
    fn only_the_fast_preset_trials_the_global_mapper() {
        // The global mapper is deliberately trialled on one preset first, so a
        // change that widens or narrows that scope has to be intentional.
        assert_eq!(
            Quality::Fast.preset().mapper_backend,
            MapperPreference::PreferGlobal
        );
        assert_eq!(
            Quality::Balanced.preset().mapper_backend,
            MapperPreference::ForceIncremental
        );
        assert_eq!(
            Quality::High.preset().mapper_backend,
            MapperPreference::ForceIncremental
        );

        // The preference must still degrade safely when the engine has no global
        // mapper at all.
        assert_eq!(
            MapperPreference::PreferGlobal.backend(false),
            MapperBackend::Incremental
        );
        assert_eq!(
            MapperPreference::PreferGlobal.backend(true),
            MapperBackend::Global
        );
        assert_eq!(
            MapperPreference::ForceIncremental.backend(true),
            MapperBackend::Incremental
        );
    }

    #[test]
    fn balanced_is_default() {
        assert_eq!(Quality::default(), Quality::Balanced);
    }
}
