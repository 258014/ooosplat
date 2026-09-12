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
    fn balanced_is_default() {
        assert_eq!(Quality::default(), Quality::Balanced);
    }
}
