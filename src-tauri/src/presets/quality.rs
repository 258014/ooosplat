use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

use crate::{engines::colmap::MapperBackend, video::FrameFilterConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MapperPreference {
    PreferGlobal,
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityPreset {
    pub frame_retention_ratio: f64,
    pub brush_iterations: usize,
    pub brush_max_resolution: u32,
    pub enable_smart_filter: bool,
    pub smart_filter_config: FrameFilterConfig,
    pub mapper_backend: MapperPreference,
    pub sequential_overlap: u32,
    pub feature_max_image_size: u32,
    pub feature_max_num_features: u32,
    /// Brush `--sh-degree`: spherical-harmonics order of the trained splats.
    /// Lowering it speeds training up but drops view-dependent effects such as
    /// reflections and highlights.
    pub brush_sh_degree: u32,
    /// Brush `--growth-stop-iter`: iteration after which densification stops, so
    /// late iterations only refine the splats that already exist.
    pub brush_growth_stop_iter: Option<usize>,
    /// Brush `--refine-every`: interval between refinement/densification passes.
    pub brush_refine_every: Option<usize>,
    /// Brush `--max-splats`: upper bound on the number of gaussians.
    pub brush_max_splats: Option<usize>,
}

impl Quality {
    pub const fn preset(self) -> QualityPreset {
        match self {
            Self::Fast => QualityPreset {
                frame_retention_ratio: 0.30,
                brush_iterations: 8_000,
                brush_max_resolution: 1_200,
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::fast(),
                mapper_backend: MapperPreference::PreferGlobal,
                sequential_overlap: 12,
                feature_max_image_size: 1280,
                feature_max_num_features: 8192,
                // 60% of 8_000 steps: densification stops early and the remaining
                // iterations refine an existing splat set.
                brush_sh_degree: 1,
                brush_growth_stop_iter: Some(4_800),
                // Brush already defaults to 200 and ties this value to the number
                // of images covering the scene, so leaving it unset avoids a
                // detail loss the presets have no evidence to justify.
                brush_refine_every: None,
                brush_max_splats: None,
            },
            Self::Balanced => QualityPreset {
                frame_retention_ratio: 0.50,
                brush_iterations: 15_000,
                brush_max_resolution: 1_600,
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::balanced(),
                mapper_backend: MapperPreference::PreferGlobal,
                sequential_overlap: 15,
                feature_max_image_size: 1600,
                feature_max_num_features: 8192,
                // 60% of 15_000 steps.
                brush_sh_degree: 1,
                brush_growth_stop_iter: Some(9_000),
                brush_refine_every: None,
                brush_max_splats: None,
            },
            Self::High => QualityPreset {
                frame_retention_ratio: 1.00,
                brush_iterations: 30_000,
                brush_max_resolution: 2_000,
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::high(),
                mapper_backend: MapperPreference::PreferGlobal,
                sequential_overlap: 20,
                feature_max_image_size: 2000,
                feature_max_num_features: 16384,
                // The high tier keeps some view-dependent shading instead of
                // dropping straight to degree 1.
                brush_sh_degree: 2,
                // 60% of 30_000 would be 18_000, which is *later* than Brush's own
                // 15_000 default. Growth must only ever stop earlier, so this tier
                // keeps the built-in default instead of extending densification.
                brush_growth_stop_iter: None,
                brush_refine_every: None,
                brush_max_splats: None,
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
    fn brush_sh_degree_stays_within_brush_supported_range() {
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            let degree = quality.preset().brush_sh_degree;
            // Brush defaults to 3; the presets only ever lower it.
            assert!(
                degree <= 3,
                "{quality:?} raised the SH degree above Brush's default"
            );
        }
    }

    #[test]
    fn growth_stop_iter_only_ever_shortens_densification() {
        // Brush stops densification at step 15000 by default. A preset may ask for
        // an earlier stop, but never a later one, which would extend the slowest
        // part of training.
        const BRUSH_DEFAULT_GROWTH_STOP_ITER: usize = 15_000;
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            let preset = quality.preset();
            if let Some(stop) = preset.brush_growth_stop_iter {
                assert!(
                    stop <= BRUSH_DEFAULT_GROWTH_STOP_ITER,
                    "{quality:?} would extend densification past Brush's default"
                );
                assert!(
                    stop <= preset.brush_iterations,
                    "{quality:?} stops growth after training already ended"
                );
            }
        }
    }

    #[test]
    fn balanced_is_default() {
        assert_eq!(Quality::default(), Quality::Balanced);
    }
}
