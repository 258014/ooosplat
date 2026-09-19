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

    /// Pick the mapper for a run of `frames` images.
    ///
    /// The global mapper's cost grows superlinearly with the number of observations
    /// (images × features per image), and past a point its global positioning stage
    /// does not finish at all. Measured on one 1920×1080 handheld clip with the Fast
    /// tier parameters (1280px / 8192 features):
    ///
    /// | frames | incremental | global |
    /// | --- | --- | --- |
    /// | 299 | 349 s | 145 s, single model |
    /// | 600 | 436 s, **12 fragments** (largest 23.3%) | 432 s, **single model 94.8%** |
    /// | 1200 | 2697 s, 21 fragments | **> 68 min, never finished** |
    /// | 2121 | 3049 s, 21 fragments | **> 58 min, cancelled by the user** |
    ///
    /// So below the threshold the global mapper is both faster and far better; above
    /// it, the incremental mapper is the only one that returns at all (fragmented, but
    /// usable). The threshold therefore scales with `feature_max_num_features`: a tier
    /// that extracts twice the features per image produces twice the observations, so
    /// it can afford roughly half the frames.
    pub const fn backend_for_frames(
        self,
        global_available: bool,
        frames: u64,
        feature_max_num_features: u32,
    ) -> MapperBackend {
        match self {
            Self::PreferGlobal
                if global_available
                    && frames <= global_mapper_frame_limit(feature_max_num_features) =>
            {
                MapperBackend::Global
            }
            _ => MapperBackend::Incremental,
        }
    }
}

/// Largest frame count for which the global mapper is still tried, for a tier that
/// extracts `feature_max_num_features` features per image.
///
/// The reference point is 600 frames at 8192 features (measured to be the last size
/// where the global mapper is the better choice on both axes). Framing it as a total
/// observation budget keeps the two tiers that double the feature count honest.
pub const fn global_mapper_frame_limit(feature_max_num_features: u32) -> u64 {
    /// Observations the global mapper can still finish comfortably.
    const OBSERVATION_BUDGET: u64 = 600 * 8192;
    // `const fn` 里不能用 `.max()`（Ord 在常量上下文尚未稳定），手写等价判断。
    let per_image = if feature_max_num_features < 1 {
        1
    } else {
        feature_max_num_features as u64
    };
    let limit = OBSERVATION_BUDGET / per_image;
    if limit < 1 {
        1
    } else {
        limit
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
    /// 期望的**最终**保留密度（帧/秒）。`None` 表示沿用 `frame_retention_ratio × 源帧率`。
    /// 它参与 L0 的三段式约束 `target_fps <= candidate_fps <= source_fps`，
    /// 但**不直接**决定最终保留数——那仍由 `smart_filter_config` 的窗口配额决定。
    pub target_fps: Option<f64>,
    /// 送进智能筛选前的**候选**密度（帧/秒）。`None` 表示沿用
    /// `target_fps × 1.5` 的超采样比例。
    ///
    /// 绝对帧率与比例的区别很重要：比例会随源帧率线性放大（60fps 源会抽到 27 帧/秒），
    /// 绝对帧率则在任何源帧率下都给出同一个候选密度。
    pub candidate_fps: Option<f64>,
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
                // 沿用比例公式：候选 = target × 1.5，行为与 v2 一致。
                target_fps: Some(6.0),
                candidate_fps: Some(12.0),
            },
            Self::Balanced => QualityPreset {
                frame_retention_ratio: 0.50,
                brush_iterations: 15_000,
                brush_max_resolution: 1_600,
                brush_tuning: BrushTuning::brush_defaults(),
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::balanced(),
                // 0.5.0 起三档统一用 global mapper。早期只在 Fast 试点，依据是手机环绕素材上
                // 增量 mapper 只注册 3/164、7/82，而 global mapper 注册 164/164、78/82。
                // 试点铺开的依据：选帧已换成"按运动量重新分配窗口配额"的那一版，
                // 同一段素材上增量 mapper 现在也能全量注册（Fast 档重复三次均为单模型）。
                // 注意 `PreferGlobal` 在断点续跑里接受任何已记录的 backend，所以既有项目
                // 不会被强制改用 global；新项目与重新生成的才走 global。
                mapper_backend: MapperPreference::PreferGlobal,
                sequential_overlap: 15,
                feature_max_image_size: 1600,
                feature_max_num_features: 8192,
                // 沿用比例公式：候选 = target × 1.5，行为与 v2 一致。
                target_fps: Some(8.0),
                candidate_fps: Some(20.0),
            },
            Self::High => QualityPreset {
                frame_retention_ratio: 1.00,
                brush_iterations: 30_000,
                brush_max_resolution: 2_000,
                brush_tuning: BrushTuning::high_detail(),
                enable_smart_filter: true,
                smart_filter_config: FrameFilterConfig::high(),
                // Same choice as Balanced: every tier prefers the global mapper since 0.5.0;
                // see the note there.
                mapper_backend: MapperPreference::PreferGlobal,
                sequential_overlap: 20,
                feature_max_image_size: 2000,
                feature_max_num_features: 16384,
                // 沿用比例公式：候选 = target × 1.5，行为与 v2 一致。
                target_fps: Some(10.0),
                candidate_fps: Some(30.0),
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
        // L0：三档默认都不指定绝对帧率（沿用 `retention_ratio × 1.5 × 源帧率`），
        // 因此抽帧行为与 v2 一致；显式帧率只在标定后按档位开启。
        for preset in [
            Quality::Fast.preset(),
            Quality::Balanced.preset(),
            Quality::High.preset(),
        ] {
            assert!(preset.target_fps.is_some() && preset.candidate_fps.is_some());
            // 具体数值由 absolute_frame_rates_follow_the_ladder 精确断言。
        }
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

    /// 匹配器的时序邻域必须覆盖筛选允许的最大帧间隔。
    ///
    /// `backfill_gaps` 只保证相邻保留帧的间隔不超过 `window_size`（筛选自己知道的上限），
    /// 而顺序匹配真正能连上的范围是 `sequential_overlap`。两者是隐式耦合：一旦某档把
    /// overlap 调到小于 window_size，某个窗口末尾与下一个窗口开头之间就会出现匹配器
    /// 覆盖不到的断层，症状是重建分裂成多个模型——而不是报错。这条断言把耦合显式化。
    #[test]
    fn sequential_overlap_covers_the_largest_gap_the_filter_can_leave() {
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            let preset = quality.preset();
            let window = preset.smart_filter_config.window_size;
            assert!(
                preset.sequential_overlap as usize >= window,
                "{quality:?}：sequential_overlap={} 小于 window_size={window}，\
                 筛选留下的最大间隔会超出匹配器的邻域范围",
                preset.sequential_overlap
            );
        }
    }

    #[test]
    fn feature_count_never_drops_below_the_pose_accuracy_floor() {
        // Below 8192 features per image, pose accuracy measurably degrades.
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            assert!(quality.preset().feature_max_num_features >= 8192);
        }
    }

    /// 按帧数切 mapper：小规模用 global，超规模退回增量。
    ///
    /// 依据（本机实测，Fast 参数）：299 帧 global 145 s 单模型；600 帧 global 432 s 单模型
    /// 94.8%（增量同期碎成 12 块、最大 23.3%）；1200 帧 global >68 分钟未完成（增量 45 分钟
    /// 跑完）；2121 帧 global >58 分钟被取消（增量 51 分钟跑完）。
    #[test]
    fn mapper_backend_switches_by_frame_count() {
        let global_available = true;
        // 8192 特征：600 帧以内用 global。
        for frames in [1_u64, 163, 299, 600] {
            assert_eq!(
                MapperPreference::PreferGlobal.backend_for_frames(global_available, frames, 8192),
                MapperBackend::Global,
                "{frames} 帧应使用 global mapper"
            );
        }
        for frames in [601_u64, 1200, 2121] {
            assert_eq!(
                MapperPreference::PreferGlobal.backend_for_frames(global_available, frames, 8192),
                MapperBackend::Incremental,
                "{frames} 帧应退回 incremental mapper"
            );
        }

        // 特征数翻倍的档位观测数也翻倍，可承受帧数减半。
        assert_eq!(global_mapper_frame_limit(8192), 600);
        assert_eq!(global_mapper_frame_limit(16_384), 300);
        assert_eq!(
            MapperPreference::PreferGlobal.backend_for_frames(global_available, 400, 16_384),
            MapperBackend::Incremental,
            "16384 特征档位在 400 帧就应退回增量"
        );

        // 引擎没有 global mapper 时无条件退回增量；ForceIncremental 永远不回 global。
        assert_eq!(
            MapperPreference::PreferGlobal.backend_for_frames(false, 100, 8192),
            MapperBackend::Incremental
        );
        assert_eq!(
            MapperPreference::ForceIncremental.backend_for_frames(true, 100, 8192),
            MapperBackend::Incremental
        );
    }

    #[test]
    fn every_preset_prefers_the_global_mapper() {
        // 0.5.0 起三档统一用 global mapper。这条测试是刻意的"改动必须是有意的"闸门：
        // 早期只在 Fast 试点，后来按实测铺开到全部档位，将来若再收窄也必须先改这里。
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            assert_eq!(
                quality.preset().mapper_backend,
                MapperPreference::PreferGlobal,
                "{quality:?} 应与其他档位一致地优先使用 global mapper"
            );
        }

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
