use std::{
    cmp::Ordering,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use image::{imageops::FilterType, GenericImageView, GrayImage, ImageReader};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::image_sequence::is_image_file;

/// 抽帧策略版本。任何改变筛选语义的改动都必须递增它，并写入 filter_summary.json，
/// 使下游与断点续跑能够判断已有的审计产物是否由同一套策略生成。
/// v2：曝光改为「序列自适应门限 + 配额保底」，绝对阈值由 0.02 校准到 0.55，裁切判定改为 >=254 / <=1。
/// v3：时序冗余筛选的参考帧改为「序列中最近一张**保留**帧」（跨窗口携带，不再在窗口边界重置），
///     并新增 gray_diff / gradient_diff / motion_score 三项度量。两项新阈值默认不参与判定
///     （`redundancy_metric` 默认 `LegacyGrayDiff`），标定完成后再切换。
pub const FILTER_STRATEGY_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameFilterConfig {
    pub blur_threshold: f64,
    pub overexposure_ratio: f64,
    pub underexposure_ratio: f64,
    /// 曝光离群裕度：门限取 `max(绝对阈值, 本序列中位数 + 该裕度)`。
    /// 整段素材本身就偏亮/偏暗时，绝对阈值会把每一帧都判成曝光异常，
    /// 加入该裕度后只有明显异于同段素材的帧（闪光、对着灯拍）才算异常。
    pub exposure_outlier_margin: f64,
    pub window_size: usize,
    pub keep_per_window: usize,
    pub analysis_max_edge: u32,
    /// 旧判定量（480px 灰度平均绝对差）的冗余阈值，仅在 `LegacyGrayDiff` 下生效。
    pub min_diff_score: f64,
    /// 时序差异度量的分析小图边长。只影响 `gray_diff` / `gradient_diff` 的成本与灵敏度，
    /// 不改变质量门使用的 `analysis_max_edge`。
    pub diff_analysis_max_edge: u32,
    /// 灰度差异门：`gray_diff` 低于它**且**梯度差异也低于各自门限时，帧才被判为冗余。
    /// 该值取自亮度归一化后的 96px 小图，与 `min_diff_score` **量纲不同**，不可直接互换。
    pub redundancy_gray_threshold: f64,
    /// 梯度差异门。结构（轮廓/位移）变化会先在这里体现，而不是在灰度差上。
    pub redundancy_gradient_threshold: f64,
    /// `motion_score` 的灰度权重。仅用于审计汇总，不单独构成判定门。
    pub gray_diff_weight: f64,
    pub gradient_diff_weight: f64,
    /// 冗余判定量的选择器。默认沿用 v2 的旧量，避免在阈值标定前改变筛选行为。
    pub redundancy_metric: RedundancyMetric,
    /// 是否启用运动候选池：被模糊门拒下、但确有明显变化且清晰度仍可用的帧，
    /// 可以从**窗口配额内**争取名额（不额外增加保留帧总数）。
    /// 默认关闭：真实素材 A/B 完成前不改变默认筛选结果。
    pub enable_motion_candidates: bool,
    /// 每个窗口最多由运动候选补进的名额。候选只在清晰帧不足配额时补位，
    /// 绝不会把已经入选的清晰帧挤掉。
    pub motion_candidate_quota: usize,
    /// 运动候选的"质量尚可"下限：`laplacian_variance >= blur_threshold * 该比例`。
    /// 取 0.6 表示只接纳"被门限判模糊、但细节并未崩掉"的帧；越接近 1 越保守。
    pub motion_candidate_min_quality_ratio: f64,
    /// 是否启用自适应冗余阈值：门限随**本序列自身**的差异分布上浮。
    ///
    /// 真实素材上"最相似的相邻帧"也往往远高于任何绝对常数（实测最小灰度差 0.031–0.084），
    /// 绝对阈值于是几乎从不生效；按序列自身的低分位数定门限，才能只剔掉"相对这段素材而言
    /// 最没有信息量"的那部分帧。门限**只会上浮到绝对阈值之上**，绝不会低于它。
    /// 默认关闭：需要真实素材 A/B 后再翻默认。
    pub enable_adaptive_threshold: bool,
    /// 自适应门限的观察窗口（最近多少次比较）。窗口越大越稳，但适应越慢。
    pub adaptive_window_size: usize,
    /// 自适应门限系数：`门限 = max(绝对阈值, 系数 × 最近窗口的低分位数)`。
    pub adaptive_threshold_fraction: f64,
}

/// 冗余判定量。两种量的图像尺度与归一化方式不同，阈值不可互换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RedundancyMetric {
    /// v2 行为：480px 灰度平均绝对差 >= `min_diff_score` 即保留。
    LegacyGrayDiff,
    /// 亮度归一化灰度差与梯度幅值差**双双低于**各自阈值时判为冗余。
    DualThreshold,
}

impl FrameFilterConfig {
    pub const fn fast() -> Self {
        Self {
            blur_threshold: 60.0,
            overexposure_ratio: 0.55,
            underexposure_ratio: 0.55,
            exposure_outlier_margin: 0.3,
            window_size: 10,
            keep_per_window: 2,
            analysis_max_edge: 480,
            min_diff_score: 0.02,
            diff_analysis_max_edge: 96,
            redundancy_gray_threshold: 0.02,
            redundancy_gradient_threshold: 0.02,
            gray_diff_weight: 1.0,
            gradient_diff_weight: 1.0,
            redundancy_metric: RedundancyMetric::LegacyGrayDiff,
            enable_motion_candidates: false,
            motion_candidate_quota: 1,
            motion_candidate_min_quality_ratio: 0.6,
            enable_adaptive_threshold: false,
            adaptive_window_size: 20,
            adaptive_threshold_fraction: 0.5,
        }
    }

    pub const fn balanced() -> Self {
        Self {
            blur_threshold: 100.0,
            overexposure_ratio: 0.55,
            underexposure_ratio: 0.55,
            exposure_outlier_margin: 0.3,
            window_size: 10,
            keep_per_window: 3,
            analysis_max_edge: 480,
            min_diff_score: 0.02,
            diff_analysis_max_edge: 96,
            redundancy_gray_threshold: 0.02,
            redundancy_gradient_threshold: 0.02,
            gray_diff_weight: 1.0,
            gradient_diff_weight: 1.0,
            redundancy_metric: RedundancyMetric::LegacyGrayDiff,
            enable_motion_candidates: false,
            motion_candidate_quota: 1,
            motion_candidate_min_quality_ratio: 0.6,
            enable_adaptive_threshold: false,
            adaptive_window_size: 20,
            adaptive_threshold_fraction: 0.5,
        }
    }

    pub const fn high() -> Self {
        Self {
            blur_threshold: 100.0,
            overexposure_ratio: 0.55,
            underexposure_ratio: 0.55,
            exposure_outlier_margin: 0.3,
            window_size: 10,
            keep_per_window: 4,
            analysis_max_edge: 480,
            min_diff_score: 0.02,
            diff_analysis_max_edge: 96,
            redundancy_gray_threshold: 0.02,
            redundancy_gradient_threshold: 0.02,
            gray_diff_weight: 1.0,
            gradient_diff_weight: 1.0,
            redundancy_metric: RedundancyMetric::LegacyGrayDiff,
            enable_motion_candidates: false,
            motion_candidate_quota: 1,
            motion_candidate_min_quality_ratio: 0.6,
            enable_adaptive_threshold: false,
            adaptive_window_size: 20,
            adaptive_threshold_fraction: 0.5,
        }
    }
}

/// Expected number of frames the filter keeps out of `extracted` analysed frames.
///
/// `decide_windows` splits the analysed frames into `window_size` chunks and keeps
/// up to `keep_per_window` of each — backfilling to that quota, and always keeping
/// at least one frame per window. The count is therefore predictable before the
/// frames exist, which is what lets runtime estimates use the number of images
/// COLMAP will actually process instead of the pre-filter extraction count.
///
/// The second-layer gap backfill can add a few frames when kept frames end up more
/// than one window apart; once filtering has run, callers should prefer the real
/// `FilterOutcome::kept_frames`.
pub fn expected_kept_frames(extracted: u64, config: &FrameFilterConfig) -> u64 {
    let extracted = extracted.max(1);
    let window_size = config.window_size.max(1) as u64;
    let keep = config.keep_per_window.max(1) as u64;
    let full_windows = extracted / window_size;
    let remainder = extracted % window_size;
    let expected = full_windows * keep + remainder.min(keep);
    expected.clamp(1, extracted)
}

/// Stable identity for the filtering inputs, including strategy semantics.
///
/// 每一项会影响筛选结果的配置都必须出现在这里：只改参数而不让哈希变化，
/// 旧项目的 filter 检查点就会被静默复用（`filter_checkpoint_complete` 只比对哈希）。
pub fn filter_config_hash(config: &FrameFilterConfig, source_frames: u64) -> String {
    // 枚举写成稳定字符串：用判别值会让枚举重排时哈希静默不变。
    let metric = match config.redundancy_metric {
        RedundancyMetric::LegacyGrayDiff => "legacy",
        RedundancyMetric::DualThreshold => "dual",
    };
    let canonical = format!(
        "v={FILTER_STRATEGY_VERSION};source_frames={source_frames};blur={:.17};over={:.17};under={:.17};margin={:.17};window={};keep={};edge={};diff={:.17};dedge={};tgray={:.17};tgrad={:.17};gw={:.17};grw={:.17};metric={metric};mc={};mcq={};mcr={:.17};ad={};adw={};adf={:.17}",
        config.blur_threshold,
        config.overexposure_ratio,
        config.underexposure_ratio,
        config.exposure_outlier_margin,
        config.window_size,
        config.keep_per_window,
        config.analysis_max_edge,
        config.min_diff_score,
        config.diff_analysis_max_edge,
        config.redundancy_gray_threshold,
        config.redundancy_gradient_threshold,
        config.gray_diff_weight,
        config.gradient_diff_weight,
        config.enable_motion_candidates,
        config.motion_candidate_quota,
        config.motion_candidate_min_quality_ratio,
        config.enable_adaptive_threshold,
        config.adaptive_window_size,
        config.adaptive_threshold_fraction,
    );
    let mut hash = 0xcbf29ce484222325u64;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a-{hash:016x}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameMetrics {
    pub frame_name: String,
    pub timestamp_ms: u64,
    pub laplacian_variance: f64,
    pub overexposure_ratio: f64,
    pub underexposure_ratio: f64,
    pub diff_score: f64,
    /// 与「序列中最近一张保留帧」的亮度归一化灰度差（`diff_analysis_max_edge` 小图）。
    /// 与 `diff_score` **量纲不同**：两者不可用同一个阈值比较。
    pub gray_diff: f64,
    /// 与「最近一张保留帧」的梯度幅值平均绝对差，关注轮廓与结构变化而非整体亮度。
    pub gradient_diff: f64,
    /// `gray_diff_weight * gray_diff + gradient_diff_weight * gradient_diff`（审计汇总）。
    pub motion_score: f64,
    /// 该帧是被模糊门拒下、但因"确有变化且质量尚可"而由运动候选池补进来的。
    pub is_motion_candidate: bool,
    /// 本帧判定时**实际生效**的冗余门限（自适应开启时会高于配置里的绝对阈值）。
    /// 记录下来才能回答"这一帧为什么被判冗余"，而不是只看到一个无法分析的总分。
    pub effective_gray_threshold: f64,
    pub effective_gradient_threshold: f64,
    pub kept: bool,
    pub reject_reason: Option<String>,
    pub forced_keep: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterOutcome {
    pub strategy_version: u32,
    pub total_frames: usize,
    pub kept_frames: usize,
    /// 质量门（blur/exposure）或配额淘汰拦下、且最终确实未保留的帧数。
    pub rejected_blur: usize,
    pub rejected_exposure: usize,
    pub rejected_redundant: usize,
    pub rejected_window_overflow: u32,
    /// 由兜底逻辑（配额保底 / 第一层兜底 / 跨窗口回填）保留的帧数。
    pub forced_keeps: usize,
    /// 本次生效的完整过滤配置。审计产物必须能自证"是哪套参数产出的"。
    pub config: FrameFilterConfig,
    /// 送入筛选的候选帧密度（即 ffmpeg 抽帧率）。源帧率与目标帧率由 plan 与
    /// `PipelineStateFile` 持有，过滤阶段只拿到候选密度，因此这里如实只记录它。
    pub candidate_fps: f64,
    /// 由运动候选池补入的保留帧数。
    pub motion_candidates: usize,
    /// 实际生效的灰度冗余门限的中位数（自适应关闭时等于配置里的绝对阈值）。
    pub adaptive_gray_threshold_p50: f64,
    pub kept_file_names: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameFilterError {
    #[error("输入帧目录不存在：{0}")]
    InputDirectory(PathBuf),
    #[error("输入目录没有可处理的 JPG、JPEG 或 PNG 帧")]
    NoFrames,
    #[error("图像解码失败：{path}：{source}")]
    Decode {
        path: PathBuf,
        source: image::ImageError,
    },
    #[error("帧过滤输出失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("写入过滤摘要失败：{0}")]
    Json(#[from] serde_json::Error),
    #[error("配置无效：{0}")]
    InvalidConfig(&'static str),
}

#[derive(Debug)]
struct AnalyzedFrame {
    path: PathBuf,
    gray: GrayImage,
    /// `diff_analysis_max_edge` 灰度小图，供时序差异度量使用（复用已解码图像，不重新读盘）。
    reduced: GrayImage,
    /// `reduced` 的梯度幅值图，逐帧只算一次。
    gradient: Vec<f32>,
    metrics: FrameMetrics,
}

/// 时序比较的参考帧：序列中最近一张被保留的帧。
///
/// 必须跨窗口边界携带。窗口内的"上一张保留帧"并不等于"最近的保留帧"：
/// 窗口边界若重置参考，边界处就会重新接纳与上一窗尾重复的画面。
#[derive(Debug)]
struct DiffReference {
    gray: GrayImage,
    reduced: GrayImage,
    gradient: Vec<f32>,
}

impl DiffReference {
    fn from_frame(frame: &AnalyzedFrame) -> Self {
        Self {
            gray: frame.gray.clone(),
            reduced: frame.reduced.clone(),
            gradient: frame.gradient.clone(),
        }
    }
}

pub fn filter_frames(
    input_dir: &Path,
    output_dir: &Path,
    config: &FrameFilterConfig,
) -> Result<FilterOutcome, FrameFilterError> {
    filter_frames_at_fps(input_dir, output_dir, config, 1.0)
}

pub fn filter_frames_at_fps(
    input_dir: &Path,
    output_dir: &Path,
    config: &FrameFilterConfig,
    sampling_fps: f64,
) -> Result<FilterOutcome, FrameFilterError> {
    filter_frames_impl(input_dir, output_dir, config, None, sampling_fps)
}

pub fn filter_frames_with_masks(
    input_dir: &Path,
    output_dir: &Path,
    mask_dir: &Path,
    config: &FrameFilterConfig,
) -> Result<FilterOutcome, FrameFilterError> {
    filter_frames_with_masks_at_fps(input_dir, output_dir, mask_dir, config, 1.0)
}

pub fn filter_frames_with_masks_at_fps(
    input_dir: &Path,
    output_dir: &Path,
    mask_dir: &Path,
    config: &FrameFilterConfig,
    sampling_fps: f64,
) -> Result<FilterOutcome, FrameFilterError> {
    filter_frames_impl(input_dir, output_dir, config, Some(mask_dir), sampling_fps)
}

fn filter_frames_impl(
    input_dir: &Path,
    output_dir: &Path,
    config: &FrameFilterConfig,
    mask_dir: Option<&Path>,
    sampling_fps: f64,
) -> Result<FilterOutcome, FrameFilterError> {
    validate_config(config)?;
    if !input_dir.is_dir() {
        return Err(FrameFilterError::InputDirectory(input_dir.to_path_buf()));
    }
    let mut paths = fs::read_dir(input_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file() && is_image_file(path))
        .collect::<Vec<_>>();
    paths.sort_by_key(|left| natural_name(left));
    if paths.is_empty() {
        return Err(FrameFilterError::NoFrames);
    }

    let mut frames = paths
        .par_iter()
        .map(|path| {
            let frame_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let mask_path = mask_dir.and_then(|directory| {
                let candidate = directory.join(format!("{frame_name}.png"));
                candidate.is_file().then_some(candidate)
            });
            analyze_frame(path, config, mask_path.as_deref(), sampling_fps)
        })
        .collect::<Result<Vec<_>, _>>()?;
    decide_windows(&mut frames, config);
    fs::create_dir_all(output_dir)?;
    for frame in &frames {
        if frame.metrics.kept {
            fs::copy(&frame.path, output_dir.join(&frame.metrics.frame_name))?;
        }
    }
    write_metadata(output_dir, &frames)?;

    let outcome = FilterOutcome {
        strategy_version: FILTER_STRATEGY_VERSION,
        total_frames: frames.len(),
        kept_frames: frames.iter().filter(|frame| frame.metrics.kept).count(),
        // 统计口径按最终决策：被保底回填而保留的帧，即便先前打上过 reject_reason，也算保留而非拒绝。
        rejected_blur: count_rejected(&frames, "blur"),
        rejected_exposure: count_rejected(&frames, "exposure"),
        rejected_redundant: count_rejected(&frames, "redundant"),
        rejected_window_overflow: count_rejected(&frames, "window_overflow") as u32,
        forced_keeps: frames
            .iter()
            .filter(|frame| frame.metrics.forced_keep)
            .count(),
        config: *config,
        candidate_fps: sampling_fps,
        motion_candidates: frames
            .iter()
            .filter(|frame| frame.metrics.is_motion_candidate)
            .count(),
        adaptive_gray_threshold_p50: median(
            frames
                .iter()
                .filter(|frame| frame.metrics.reject_reason.is_none())
                .map(|frame| frame.metrics.effective_gray_threshold),
        ),
        kept_file_names: frames
            .iter()
            .filter(|frame| frame.metrics.kept)
            .map(|frame| frame.metrics.frame_name.clone())
            .collect(),
    };
    fs::write(
        output_dir.join("filter_summary.json"),
        serde_json::to_vec_pretty(&outcome)?,
    )?;
    let forced = frames
        .iter()
        .filter(|frame| frame.metrics.forced_keep)
        .count();
    fs::write(
        output_dir.join("filter_forced_keep.log"),
        format!(
            "显式 forced_keep 帧数：{forced}\n{}",
            frames
                .iter()
                .filter(|frame| frame.metrics.forced_keep)
                .map(|frame| frame.metrics.frame_name.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )?;
    Ok(outcome)
}

fn validate_config(config: &FrameFilterConfig) -> Result<(), FrameFilterError> {
    if config.window_size == 0 {
        return Err(FrameFilterError::InvalidConfig("window_size 必须大于 0"));
    }
    if config.keep_per_window == 0 {
        return Err(FrameFilterError::InvalidConfig(
            "keep_per_window 必须大于 0",
        ));
    }
    if config.analysis_max_edge == 0 {
        return Err(FrameFilterError::InvalidConfig(
            "analysis_max_edge 必须大于 0",
        ));
    }
    if config.diff_analysis_max_edge == 0 {
        return Err(FrameFilterError::InvalidConfig(
            "diff_analysis_max_edge 必须大于 0",
        ));
    }
    if !config.redundancy_gray_threshold.is_finite()
        || !config.redundancy_gradient_threshold.is_finite()
        || config.redundancy_gray_threshold < 0.0
        || config.redundancy_gradient_threshold < 0.0
    {
        return Err(FrameFilterError::InvalidConfig("冗余阈值必须是非负有限值"));
    }
    if !config.gray_diff_weight.is_finite()
        || !config.gradient_diff_weight.is_finite()
        || config.gray_diff_weight < 0.0
        || config.gradient_diff_weight < 0.0
    {
        return Err(FrameFilterError::InvalidConfig("差异权重必须是非负有限值"));
    }
    if config.motion_candidate_quota > config.keep_per_window {
        return Err(FrameFilterError::InvalidConfig(
            "运动候选配额不能超过窗口配额",
        ));
    }
    if !config.motion_candidate_min_quality_ratio.is_finite()
        || config.motion_candidate_min_quality_ratio <= 0.0
        || config.motion_candidate_min_quality_ratio > 1.0
    {
        return Err(FrameFilterError::InvalidConfig(
            "运动候选质量比例必须位于 0..=1",
        ));
    }
    if config.adaptive_window_size == 0 {
        return Err(FrameFilterError::InvalidConfig(
            "adaptive_window_size 必须大于 0",
        ));
    }
    if !config.adaptive_threshold_fraction.is_finite()
        || config.adaptive_threshold_fraction <= 0.0
        || config.adaptive_threshold_fraction > 1.0
    {
        return Err(FrameFilterError::InvalidConfig(
            "自适应门限系数必须位于 0..=1",
        ));
    }
    if !(0.0..=1.0).contains(&config.overexposure_ratio)
        || !(0.0..=1.0).contains(&config.underexposure_ratio)
        || !(0.0..=1.0).contains(&config.min_diff_score)
    {
        return Err(FrameFilterError::InvalidConfig(
            "比例和差异阈值必须位于 0..=1",
        ));
    }
    Ok(())
}

fn analyze_frame(
    path: &Path,
    config: &FrameFilterConfig,
    mask_path: Option<&Path>,
    sampling_fps: f64,
) -> Result<AnalyzedFrame, FrameFilterError> {
    let image = ImageReader::open(path)
        .map_err(|source| FrameFilterError::Decode {
            path: path.to_path_buf(),
            source: image::ImageError::IoError(source),
        })?
        .decode()
        .map_err(|source| FrameFilterError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    let (width, height) = image.dimensions();
    let scale = config.analysis_max_edge as f64 / width.max(height) as f64;
    let gray = if scale < 1.0 {
        image
            .resize(
                (width as f64 * scale).round().max(1.0) as u32,
                (height as f64 * scale).round().max(1.0) as u32,
                FilterType::Triangle,
            )
            .to_luma8()
    } else {
        image.to_luma8()
    };
    let visible = mask_path.and_then(|mask| {
        ImageReader::open(mask)
            .and_then(|reader| reader.decode().map_err(std::io::Error::other))
            .ok()
            .map(|mask| {
                let mask = mask
                    .resize_exact(gray.width(), gray.height(), FilterType::Nearest)
                    .to_luma8();
                mask.as_raw()
                    .iter()
                    .map(|&value| value > 0)
                    .collect::<Vec<_>>()
            })
    });
    let (laplacian_variance, overexposure_ratio, underexposure_ratio) =
        gray_metrics(&gray, visible.as_deref());
    // 时序差异度量在更小的图上做，避免逐帧比较 480px 全图。这里复用刚解码好的灰度图，
    // 不重新读盘，新增开销只有一次缩小与一次梯度遍历。
    let reduced = reduced_gray(&gray, config.diff_analysis_max_edge);
    let gradient = gradient_magnitude(&reduced);
    let frame_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    Ok(AnalyzedFrame {
        gray,
        reduced,
        gradient,
        path: path.to_path_buf(),
        metrics: FrameMetrics {
            frame_name: frame_name.clone(),
            timestamp_ms: ((frame_number(&frame_name).saturating_sub(1) as f64 * 1000.0
                / sampling_fps.max(f64::EPSILON))
            .round() as u64),
            laplacian_variance,
            overexposure_ratio,
            underexposure_ratio,
            gray_diff: 0.0,
            gradient_diff: 0.0,
            motion_score: 0.0,
            is_motion_candidate: false,
            // 初始即为配置里的绝对阈值：没有参考帧的帧（序列首帧）从未参与比较，
            // 记 0.0 会让审计列出现无法解释的值。
            effective_gray_threshold: config.redundancy_gray_threshold,
            effective_gradient_threshold: config.redundancy_gradient_threshold,
            diff_score: 1.0,
            kept: false,
            reject_reason: None,
            forced_keep: false,
        },
    })
}

fn gray_metrics(gray: &GrayImage, visible: Option<&[bool]>) -> (f64, f64, f64) {
    let pixels = gray.as_raw();
    let indices = pixels
        .iter()
        .enumerate()
        .filter(|(index, _)| visible.is_none_or(|mask| mask.get(*index).copied().unwrap_or(false)));
    let selected = indices.collect::<Vec<_>>();
    let total = selected.len().max(1) as f64;
    // 只统计真正被裁切的像素：>=254 是 8bit 下的近裁切，<=1 是近全黑。
    // 早期用 >250 会把「曝光正常的白布景/雪景」也算成裁切，导致整段素材被误拒。
    let over = selected.iter().filter(|(_, &value)| value >= 254).count() as f64 / total;
    let under = selected.iter().filter(|(_, &value)| value <= 1).count() as f64 / total;
    let mut responses = Vec::new();
    if gray.width() >= 3 && gray.height() >= 3 {
        for y in 1..gray.height() - 1 {
            for x in 1..gray.width() - 1 {
                let visible_at = |px: u32, py: u32| {
                    visible.is_none_or(|mask| {
                        mask.get((py * gray.width() + px) as usize)
                            .copied()
                            .unwrap_or(false)
                    })
                };
                if !(visible_at(x, y)
                    && visible_at(x - 1, y)
                    && visible_at(x + 1, y)
                    && visible_at(x, y - 1)
                    && visible_at(x, y + 1))
                {
                    continue;
                }
                let center = gray.get_pixel(x, y)[0] as f64;
                let neighbors = gray.get_pixel(x - 1, y)[0] as f64
                    + gray.get_pixel(x + 1, y)[0] as f64
                    + gray.get_pixel(x, y - 1)[0] as f64
                    + gray.get_pixel(x, y + 1)[0] as f64;
                responses.push(neighbors - 4.0 * center);
            }
        }
    }
    let mean = responses.iter().sum::<f64>() / responses.len().max(1) as f64;
    let variance = responses
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / responses.len().max(1) as f64;
    (variance, over, under)
}

/// 当前生效的冗余门限。自适应关闭时就是绝对阈值本身；打开时按最近窗口的低分位数上浮。
fn effective_redundancy_gates(
    config: &FrameFilterConfig,
    recent_gray: &RecentDiffs,
    recent_gradient: &RecentDiffs,
) -> (f64, f64) {
    if !config.enable_adaptive_threshold {
        return (
            config.redundancy_gray_threshold,
            config.redundancy_gradient_threshold,
        );
    }
    (
        adaptive_threshold(
            config.redundancy_gray_threshold,
            recent_gray,
            config.adaptive_threshold_fraction,
        ),
        adaptive_threshold(
            config.redundancy_gradient_threshold,
            recent_gradient,
            config.adaptive_threshold_fraction,
        ),
    )
}

fn decide_windows(frames: &mut [AnalyzedFrame], config: &FrameFilterConfig) {
    // 下游 COLMAP 使用 sequential_matcher --SequentialMatching.overlap 10，依赖帧的时序邻域关系建立匹配图。若采用全局按质量排序取 Top-N，会抽出时间上跳跃的帧，导致匹配图断裂、mapper 分块失败，重建直接失败。
    let (over_gate, under_gate) = adaptive_exposure_gates(frames, config);
    // 参考帧跨窗口携带：过滤会丢弃中间帧，窗口边界若重置参考，边界附近就会重新
    // 接纳与上一窗尾重复的画面。这是 v3 相对 v2 的语义变化。
    let mut reference: Option<DiffReference> = None;
    // 自适应门限的观察窗口同样跨窗口累积：它描述的是"当前这段素材的运动尺度"。
    let mut recent_gray = RecentDiffs::new(config.adaptive_window_size);
    let mut recent_gradient = RecentDiffs::new(config.adaptive_window_size);
    for window in frames.chunks_mut(config.window_size) {
        // 1) 质量门。曝光先判：真正被裁切的帧应记 exposure，
        //    否则一张全白帧会因为均匀图拉普拉斯方差为 0 而被记成 blur，审计原因误导排查。
        for frame in window.iter_mut() {
            if frame.metrics.overexposure_ratio > over_gate
                || frame.metrics.underexposure_ratio > under_gate
            {
                frame.metrics.reject_reason = Some("exposure".into());
            } else if frame.metrics.laplacian_variance < config.blur_threshold {
                frame.metrics.reject_reason = Some("blur".into());
            }
        }
        // 2) 合格帧的时序冗余筛选：参考帧是序列中最近一张**保留**帧，而不是窗口内上一张。
        let mut qualified: Vec<usize> = Vec::new();
        for (index, frame) in window.iter_mut().enumerate() {
            if frame.metrics.reject_reason.is_some() {
                continue;
            }
            let keeps_moving = match reference.as_ref() {
                None => true,
                Some(previous) => {
                    frame.metrics.diff_score =
                        mean_absolute_difference(&previous.gray, &frame.gray);
                    frame.metrics.gray_diff =
                        normalized_gray_difference(&previous.reduced, &frame.reduced);
                    frame.metrics.gradient_diff =
                        gradient_difference(&previous.gradient, &frame.gradient);
                    frame.metrics.motion_score = config.gray_diff_weight * frame.metrics.gray_diff
                        + config.gradient_diff_weight * frame.metrics.gradient_diff;
                    let (gray_gate, gradient_gate) =
                        effective_redundancy_gates(config, &recent_gray, &recent_gradient);
                    frame.metrics.effective_gray_threshold = gray_gate;
                    frame.metrics.effective_gradient_threshold = gradient_gate;
                    let keeps_moving = match config.redundancy_metric {
                        // v2 行为：只比灰度差，阈值取自 480px 全图。
                        RedundancyMetric::LegacyGrayDiff => {
                            frame.metrics.diff_score >= config.min_diff_score
                        }
                        // 两项都低于各自门限才算冗余：结构或亮度任一项明显变化都保留。
                        RedundancyMetric::DualThreshold => {
                            frame.metrics.gray_diff >= gray_gate
                                || frame.metrics.gradient_diff >= gradient_gate
                        }
                    };
                    // 门限先取（因果），再把手上的差异计入观察窗口：本帧不能影响自己的判定。
                    if config.enable_adaptive_threshold {
                        recent_gray.push(frame.metrics.gray_diff);
                        recent_gradient.push(frame.metrics.gradient_diff);
                    }
                    keeps_moving
                }
            };
            if keeps_moving {
                frame.metrics.kept = true;
                reference = Some(DiffReference::from_frame(frame));
                qualified.push(index);
            } else {
                frame.metrics.reject_reason = Some("redundant".into());
            }
        }
        // 2b) 运动候选池：被模糊门拒下、但确有明显变化、且清晰度仍在可用范围内的帧，
        //     可以从窗口配额里争取剩余名额。两个硬边界：
        //     · 只在清晰帧**不足**配额时补位，因此永远不会挤掉已入选的清晰帧；
        //     · 每个窗口最多补 `motion_candidate_quota` 张，且总数仍受窗口配额约束，
        //       所以保留帧数的上界与 v2 完全一致。
        //     候选之间也互相比对（参考推进到刚入选的候选），避免一段运动把名额占满近似重复的帧。
        if config.enable_motion_candidates && qualified.len() < config.keep_per_window {
            let quality_floor = config.blur_threshold * config.motion_candidate_min_quality_ratio;
            let mut motion_reference = reference.as_ref().map(|previous| DiffReference {
                gray: previous.gray.clone(),
                reduced: previous.reduced.clone(),
                gradient: previous.gradient.clone(),
            });
            let mut admitted = 0usize;
            for frame in window.iter_mut() {
                if admitted >= config.motion_candidate_quota
                    || qualified.len() + admitted >= config.keep_per_window
                {
                    break;
                }
                if frame.metrics.reject_reason.as_deref() != Some("blur") {
                    continue;
                }
                if frame.metrics.laplacian_variance < quality_floor {
                    continue;
                }
                let Some(previous) = motion_reference.as_ref() else {
                    continue;
                };
                frame.metrics.diff_score = mean_absolute_difference(&previous.gray, &frame.gray);
                frame.metrics.gray_diff =
                    normalized_gray_difference(&previous.reduced, &frame.reduced);
                frame.metrics.gradient_diff =
                    gradient_difference(&previous.gradient, &frame.gradient);
                frame.metrics.motion_score = config.gray_diff_weight * frame.metrics.gray_diff
                    + config.gradient_diff_weight * frame.metrics.gradient_diff;
                // 「明显变化」沿用与冗余判定同一个选择器与同一套（可能自适应的）门限，
                // 避免出现两套互相矛盾的标准。
                let (gray_gate, gradient_gate) =
                    effective_redundancy_gates(config, &recent_gray, &recent_gradient);
                frame.metrics.effective_gray_threshold = gray_gate;
                frame.metrics.effective_gradient_threshold = gradient_gate;
                let changed = match config.redundancy_metric {
                    RedundancyMetric::LegacyGrayDiff => {
                        frame.metrics.diff_score >= config.min_diff_score
                    }
                    RedundancyMetric::DualThreshold => {
                        frame.metrics.gray_diff >= gray_gate
                            || frame.metrics.gradient_diff >= gradient_gate
                    }
                };
                if config.enable_adaptive_threshold {
                    recent_gray.push(frame.metrics.gray_diff);
                    recent_gradient.push(frame.metrics.gradient_diff);
                }
                if !changed {
                    continue;
                }
                frame.metrics.kept = true;
                frame.metrics.is_motion_candidate = true;
                frame.metrics.reject_reason = None;
                admitted += 1;
                motion_reference = Some(DiffReference::from_frame(frame));
            }
        }
        // 3) 配额内按清晰度淘汰：窗口首个合格帧是时序锚点，必须保留，只在其余候选中淘汰。
        if qualified.len() > config.keep_per_window {
            let mut rest = qualified[1..].to_vec();
            rest.sort_by(|left, right| {
                window[*right]
                    .metrics
                    .laplacian_variance
                    .partial_cmp(&window[*left].metrics.laplacian_variance)
                    .unwrap_or(Ordering::Equal)
            });
            for index in rest
                .into_iter()
                .skip(config.keep_per_window.saturating_sub(1))
            {
                window[index].metrics.kept = false;
                window[index].metrics.reject_reason = Some("window_overflow".into());
            }
        }
        // 4) 配额保底：被曝光门裁掉、但仍保留可用细节的帧，按清晰度回填到配额。
        //    重建需要足够的视角覆盖，所以宁可多几帧近似重复，也不让窗口塌成「每窗一帧」。
        //    低细节帧（blur）不参与回填——无纹理帧补进来对匹配没有帮助，仍由第一层兜底保证至少一帧。
        let kept_now = window.iter().filter(|frame| frame.metrics.kept).count();
        if kept_now < config.keep_per_window {
            let mut deficit = config.keep_per_window - kept_now;
            let mut pool = (0..window.len())
                .filter(|index| {
                    let frame = &window[*index];
                    !frame.metrics.kept
                        && frame.metrics.reject_reason.as_deref() == Some("exposure")
                        && frame.metrics.laplacian_variance >= config.blur_threshold
                })
                .collect::<Vec<_>>();
            pool.sort_by(|left, right| {
                window[*right]
                    .metrics
                    .laplacian_variance
                    .partial_cmp(&window[*left].metrics.laplacian_variance)
                    .unwrap_or(Ordering::Equal)
            });
            for index in pool {
                if deficit == 0 {
                    break;
                }
                window[index].metrics.kept = true;
                window[index].metrics.forced_keep = true;
                window[index].metrics.reject_reason = None;
                deficit -= 1;
            }
        }
        // 5) 第一层兜底：整窗没有任何保留帧时，保留清晰度最高的那一帧。
        if !window.iter().any(|frame| frame.metrics.kept) {
            if let Some(index) = best_laplacian_index(window) {
                window[index].metrics.kept = true;
                window[index].metrics.forced_keep = true;
                window[index].metrics.reject_reason = None;
            }
        }
        // 6) 参考帧对齐：只用**通过质量门**的保留帧做比较锚点。第 3 步可能把第 2 步刚设成
        //    参考的帧淘汰掉，所以必须回看本窗口真实的保留结果；而兜底帧与运动候选是为了
        //    时序覆盖才留下的（可能是模糊帧），拿它们当基准会低估下一窗口的真实差异。
        //    本窗口若没有合格锚点，就保留上一窗口的参考，宁可多留几帧也不误杀。
        if let Some(anchor) = window.iter().rev().find(|frame| {
            frame.metrics.kept && !frame.metrics.forced_keep && !frame.metrics.is_motion_candidate
        }) {
            reference = Some(DiffReference::from_frame(anchor));
        }
    }
    backfill_gaps(frames, config);
}

/// 曝光门限取「绝对阈值」与「本序列中位数 + 离群裕度」中更宽松的一个：
/// 整段素材本身偏亮/偏暗时，绝对阈值会把每一帧都判成异常，只有真正异于同段素材的帧（闪光、对着灯拍）才该剔除。
fn adaptive_exposure_gates(frames: &[AnalyzedFrame], config: &FrameFilterConfig) -> (f64, f64) {
    let over = median(frames.iter().map(|frame| frame.metrics.overexposure_ratio));
    let under = median(frames.iter().map(|frame| frame.metrics.underexposure_ratio));
    (
        config
            .overexposure_ratio
            .max(over + config.exposure_outlier_margin),
        config
            .underexposure_ratio
            .max(under + config.exposure_outlier_margin),
    )
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut sorted = values.collect::<Vec<_>>();
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn best_laplacian_index(window: &[AnalyzedFrame]) -> Option<usize> {
    (0..window.len()).max_by(|left, right| {
        window[*left]
            .metrics
            .laplacian_variance
            .partial_cmp(&window[*right].metrics.laplacian_variance)
            .unwrap_or(Ordering::Equal)
    })
}

/// 第二层兜底：回填相邻保留帧之间的 gap，直到不再存在超过 window_size 的间隔。
/// 每轮在 gap **中点**取一帧标记为 forced_keep，使一次插入把间隔近似对半切开，
/// 从而用最少的帧满足间隔上限（跨窗口参考后，静止素材的保留帧会稀疏得多，
/// 若仍取"清晰度最高"的末帧，每轮只能缩短一格，会把整段静止区间密集填满）。
fn backfill_gaps(frames: &mut [AnalyzedFrame], config: &FrameFilterConfig) {
    loop {
        let kept_indices = frames
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| frame.metrics.kept.then_some(index))
            .collect::<Vec<_>>();
        let Some(pair) = kept_indices
            .windows(2)
            .find(|pair| pair[1] - pair[0] > config.window_size)
        else {
            break;
        };
        if let Some(index) = gap_split_index(frames, pair[0] + 1, pair[1]) {
            frames[index].metrics.kept = true;
            frames[index].metrics.forced_keep = true;
            frames[index].metrics.reject_reason = None;
        } else {
            break;
        }
    }
}

fn mean_absolute_difference(left: &GrayImage, right: &GrayImage) -> f64 {
    if left.dimensions() != right.dimensions() {
        return 1.0;
    }
    left.as_raw()
        .iter()
        .zip(right.as_raw())
        .map(|(a, b)| (*a as f64 - *b as f64).abs() / 255.0)
        .sum::<f64>()
        / left.as_raw().len().max(1) as f64
}

/// 自适应门限取的低分位数：只有"相对本段素材最不相似"的那一小部分才算新信息。
const ADAPTIVE_QUANTILE: f64 = 0.10;

/// 固定容量的最近差异队列（FIFO），用于自适应门限。
///
/// 刻意只保留最近 `capacity` 次比较：门限要跟着**当前**素材的运动尺度走，
/// 而不是被整段素材的平均值拖住。
#[derive(Debug)]
struct RecentDiffs {
    capacity: usize,
    values: Vec<f64>,
}

impl RecentDiffs {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            values: Vec::new(),
        }
    }

    fn push(&mut self, value: f64) {
        if self.values.len() == self.capacity {
            self.values.remove(0);
        }
        self.values.push(value);
    }

    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn values(&self) -> &[f64] {
        &self.values
    }
}

/// 自适应冗余门限：`max(绝对阈值, 系数 × 最近窗口的低分位数)`。
///
/// **只上浮、不下潜**：绝对阈值始终是下限。若允许下潜，「整段都很像」的素材
/// 会把门限压到接近 0，于是任何微小噪声都被当成新信息，冗余筛选形同失效。
/// 反过来，素材本身运动幅度大时门限上浮，剔掉的只是"相对这段素材最没信息量"的帧。
/// 窗口为空（序列开头）时退回绝对阈值，保证从第一对比较开始行为就确定。
fn adaptive_threshold(absolute: f64, recent: &RecentDiffs, fraction: f64) -> f64 {
    if recent.is_empty() {
        return absolute;
    }
    let mut sorted = recent.values().to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let index = ((sorted.len() - 1) as f64 * ADAPTIVE_QUANTILE).round() as usize;
    absolute.max(fraction * sorted[index])
}

fn count_rejected(frames: &[AnalyzedFrame], reason: &str) -> usize {
    frames
        .iter()
        .filter(|frame| {
            !frame.metrics.kept && frame.metrics.reject_reason.as_deref() == Some(reason)
        })
        .count()
}

/// 把灰度图缩到 `max_edge` 以内的小图；本来就不大于该边长时直接克隆。
fn reduced_gray(gray: &GrayImage, max_edge: u32) -> GrayImage {
    let (width, height) = gray.dimensions();
    let scale = max_edge as f64 / width.max(height).max(1) as f64;
    if scale >= 1.0 {
        return gray.clone();
    }
    image::imageops::resize(
        gray,
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
        FilterType::Triangle,
    )
}

/// 中心差分梯度幅值图，量纲与 0..=255 的灰度一致（`(|dx| + |dy|) / 2`），边界像素取 0。
///
/// 刻意不用直方图差异：直方图对**空间位移**不敏感，同一纹理平移几像素后直方图几乎不变，
/// 而我们要抓的正是位移与结构变化。逐像素中心差分对位移敏感，且成本最低（每像素 4 读 3 算）。
fn gradient_magnitude(reduced: &GrayImage) -> Vec<f32> {
    let (width, height) = reduced.dimensions();
    let mut magnitudes = vec![0.0_f32; (width * height) as usize];
    if width < 3 || height < 3 {
        return magnitudes;
    }
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let left = reduced.get_pixel(x - 1, y)[0] as f32;
            let right = reduced.get_pixel(x + 1, y)[0] as f32;
            let up = reduced.get_pixel(x, y - 1)[0] as f32;
            let down = reduced.get_pixel(x, y + 1)[0] as f32;
            magnitudes[(y * width + x) as usize] = ((right - left).abs() + (down - up).abs()) / 2.0;
        }
    }
    magnitudes
}

/// 亮度归一化后的灰度平均绝对差：两图各自减去自身均值再比较，抵消整帧曝光偏移。
///
/// 直接比较原值时，一次整体变亮会和真实位移得到同样的差异分；减去均值后只有
/// 「结构或位置发生变化」的部分才贡献差异。值域 `[0,1]`，`0` 表示结构相同。
fn normalized_gray_difference(left: &GrayImage, right: &GrayImage) -> f64 {
    if left.dimensions() != right.dimensions() {
        return 1.0;
    }
    let left_raw = left.as_raw();
    let right_raw = right.as_raw();
    let count = left_raw.len().max(1) as f64;
    let left_mean = left_raw.iter().map(|value| *value as f64).sum::<f64>() / count;
    let right_mean = right_raw.iter().map(|value| *value as f64).sum::<f64>() / count;
    let offset = left_mean - right_mean;
    left_raw
        .iter()
        .zip(right_raw)
        .map(|(a, b)| ((*a as f64 - *b as f64) - offset).abs() / 255.0)
        .sum::<f64>()
        / count
}

/// 梯度幅值图的平均绝对差，同样归一化到 `[0,1]`。
fn gradient_difference(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 1.0;
    }
    left.iter()
        .zip(right)
        .map(|(a, b)| (*a as f64 - *b as f64).abs() / 255.0)
        .sum::<f64>()
        / left.len() as f64
}

/// 在 `[start, end)` 区间里挑一帧作为跨窗回填：取区间中点，
/// 让一次插入把 gap 近似对半切开，从而用最少的帧满足 `window_size` 间隔上限。
///
/// 旧实现取"清晰度最高"的一帧；在相邻帧质量接近（静止或均匀移动的素材）时会退化成
/// 取区间末帧，于是每轮只能缩短一格距离，把一段静止区间密集填满。
fn gap_split_index(frames: &[AnalyzedFrame], start: usize, end: usize) -> Option<usize> {
    if end <= start {
        return None;
    }
    let middle = start + (end - start) / 2;
    let candidates = if (end - start) % 2 == 1 {
        vec![middle]
    } else {
        vec![middle.saturating_sub(1), middle]
    };
    candidates.into_iter().max_by(|left, right| {
        frames[*left]
            .metrics
            .laplacian_variance
            .partial_cmp(&frames[*right].metrics.laplacian_variance)
            .unwrap_or(Ordering::Equal)
            // 质量相等时取较小下标，保证结果与比较顺序无关。
            .then_with(|| right.cmp(left))
    })
}

fn write_metadata(output_dir: &Path, frames: &[AnalyzedFrame]) -> Result<(), FrameFilterError> {
    let mut file = fs::File::create(output_dir.join("metadata.csv"))?;
    writeln!(file, "frame_name,timestamp_ms,laplacian_variance,overexposure_ratio,underexposure_ratio,diff_score,gray_diff,gradient_diff,motion_score,is_motion_candidate,effective_gray_threshold,effective_gradient_threshold,kept,reject_reason")?;
    for frame in frames {
        writeln!(
            file,
            "{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{},{:.6},{:.6},{},{}",
            csv_escape(&frame.metrics.frame_name),
            frame.metrics.timestamp_ms,
            frame.metrics.laplacian_variance,
            frame.metrics.overexposure_ratio,
            frame.metrics.underexposure_ratio,
            frame.metrics.diff_score,
            frame.metrics.gray_diff,
            frame.metrics.gradient_diff,
            frame.metrics.motion_score,
            frame.metrics.is_motion_candidate,
            frame.metrics.effective_gray_threshold,
            frame.metrics.effective_gradient_threshold,
            frame.metrics.kept,
            frame
                .metrics
                .reject_reason
                .as_deref()
                .map(csv_escape)
                .unwrap_or_default()
        )?;
    }
    Ok(())
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
fn natural_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}
fn frame_number(name: &str) -> u64 {
    name.strip_suffix(".jpg")
        .or_else(|| name.strip_suffix(".jpeg"))
        .or_else(|| name.strip_suffix(".png"))
        .unwrap_or(name)
        .rsplit_once('_')
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma};
    use tempfile::tempdir;

    #[test]
    fn filter_hash_is_stable_and_includes_source_count_and_config() {
        let config = FrameFilterConfig::balanced();
        let first = filter_config_hash(&config, 120);
        assert_eq!(first, filter_config_hash(&config, 120));
        assert_ne!(first, filter_config_hash(&config, 121));
        let mut changed = config;
        changed.keep_per_window += 1;
        assert_ne!(first, filter_config_hash(&changed, 120));
        assert!(first.starts_with("fnv1a-"));
    }

    fn config() -> FrameFilterConfig {
        FrameFilterConfig {
            blur_threshold: 60.0,
            overexposure_ratio: 0.55,
            underexposure_ratio: 0.55,
            exposure_outlier_margin: 0.3,
            window_size: 10,
            keep_per_window: 3,
            analysis_max_edge: 64,
            min_diff_score: 0.01,
            diff_analysis_max_edge: 96,
            redundancy_gray_threshold: 0.02,
            redundancy_gradient_threshold: 0.02,
            gray_diff_weight: 1.0,
            gradient_diff_weight: 1.0,
            redundancy_metric: RedundancyMetric::LegacyGrayDiff,
            enable_motion_candidates: false,
            motion_candidate_quota: 1,
            motion_candidate_min_quality_ratio: 0.6,
            enable_adaptive_threshold: false,
            adaptive_window_size: 20,
            adaptive_threshold_fraction: 0.5,
        }
    }
    fn textured(index: u32, blurred: bool) -> ImageBuffer<Luma<u8>, Vec<u8>> {
        let image = ImageBuffer::from_fn(64, 64, |x, y| {
            let checker = ((x / 8 + y / 8) % 2) * 100;
            Luma([(checker + ((x * 17 + y * 13 + index * 3) % 100)) as u8])
        });
        if blurred {
            image::imageops::blur(&image, 5.0)
        } else {
            image
        }
    }
    fn save_frames(dir: &Path, count: u32, blur_indices: &[u32]) {
        for index in 1..=count {
            textured(index, blur_indices.contains(&index))
                .save(dir.join(format!("frame_{index:06}.jpg")))
                .unwrap();
        }
    }
    fn save_png(dir: &Path, index: u32, image: &ImageBuffer<Luma<u8>, Vec<u8>>) {
        image
            .save(dir.join(format!("frame_{index:06}.png")))
            .unwrap();
    }

    /// 白布景 + 移动的暗色物体：3DGS 实物扫描的常见布景。
    /// 曝光完全正常，但白色像素占比极高——旧版绝对阈值会把它整段判成曝光异常。
    fn bright_backdrop(index: u32) -> ImageBuffer<Luma<u8>, Vec<u8>> {
        ImageBuffer::from_fn(64, 64, |x, y| {
            let shift = (index * 5) % 48;
            if (shift..shift + 12).contains(&x) && (20..44).contains(&y) {
                Luma([0])
            } else {
                Luma([255])
            }
        })
    }

    /// 超出序列曝光门限、但边缘细节充足（能用于匹配）的帧。
    fn overexposed_but_detailed(index: u32) -> ImageBuffer<Luma<u8>, Vec<u8>> {
        ImageBuffer::from_fn(64, 64, |x, y| {
            if x < 13 {
                let shifted = (x + index) % 13;
                if (shifted / 3 + y / 3).is_multiple_of(2) {
                    Luma([255])
                } else {
                    Luma([0])
                }
            } else {
                Luma([255])
            }
        })
    }

    /// 曝光正常、同样有充足细节的对照帧。
    fn normal_but_detailed(index: u32) -> ImageBuffer<Luma<u8>, Vec<u8>> {
        ImageBuffer::from_fn(64, 64, |x, y| {
            if x < 13 {
                let shifted = (x + index) % 13;
                if (shifted / 3 + y / 3).is_multiple_of(2) {
                    Luma([120])
                } else {
                    Luma([20])
                }
            } else {
                Luma([60])
            }
        })
    }

    #[test]
    fn bright_backdrop_is_not_mass_rejected() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        for index in 1..=20 {
            save_png(input.path(), index, &bright_backdrop(index));
        }
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        // 自适应门限：整段都白，就没有任何一帧是"异常亮"，一帧都不该因曝光被拒。
        assert_eq!(result.rejected_exposure, 0);
        // 保留量应达到配额，而不是塌成"每窗一帧"。
        assert_eq!(result.kept_frames, 2 * config().keep_per_window);
        assert_eq!(result.forced_keeps, 0);
    }

    #[test]
    fn exposure_gate_backfills_the_window_quota() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        for index in 1..=10 {
            save_png(input.path(), index, &overexposed_but_detailed(index));
        }
        for index in 11..=20 {
            save_png(input.path(), index, &normal_but_detailed(index));
        }
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        // 第一个窗口整窗被曝光门拦下，但帧本身细节充足：应回填到配额，而不是只留一帧。
        assert_eq!(result.kept_frames, 2 * config().keep_per_window);
        assert_eq!(result.forced_keeps, config().keep_per_window);
        assert_eq!(result.rejected_exposure, 10 - config().keep_per_window);
    }

    #[test]
    fn predicted_kept_frames_match_the_real_filter() {
        // The predictor is used to size runtime estimates before extraction runs,
        // so it must agree with the filter's own window quota.
        for extracted in [10_u32, 20, 30, 35] {
            let input = tempdir().unwrap();
            let output = tempdir().unwrap();
            save_frames(input.path(), extracted, &[]);
            let result = filter_frames(input.path(), output.path(), &config()).unwrap();
            assert_eq!(
                result.kept_frames as u64,
                expected_kept_frames(extracted as u64, &config()),
                "mismatch for {extracted} extracted frames"
            );
        }
    }

    #[test]
    fn predicted_kept_frames_respect_the_window_quota() {
        let config = config();
        // Two full windows keep two quotas.
        assert_eq!(
            expected_kept_frames(20, &config),
            2 * config.keep_per_window as u64
        );
        // A partial window keeps at most one quota and never fewer than one frame.
        assert_eq!(
            expected_kept_frames(5, &config),
            config.keep_per_window as u64
        );
        assert_eq!(expected_kept_frames(1, &config), 1);
        // A window smaller than the quota keeps every frame it has.
        let tiny = FrameFilterConfig {
            window_size: 10,
            keep_per_window: 8,
            ..config
        };
        assert_eq!(expected_kept_frames(4, &tiny), 4);
        // The prediction can never exceed the frames that actually exist.
        for extracted in [1_u64, 3, 7, 99, 1_000] {
            assert!(expected_kept_frames(extracted, &config) <= extracted);
        }
    }

    #[test]
    fn blur_is_rejected() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_frames(input.path(), 20, &[7, 13]);
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        let metadata = fs::read_to_string(output.path().join("metadata.csv")).unwrap();
        let rows = metadata.lines().skip(1).collect::<Vec<_>>();
        assert_eq!(rows.len(), 20);
        assert!(result.rejected_blur >= 2);
        assert!(rows[6].contains(",false,blur"));
        assert!(rows[12].contains(",false,blur"));
    }

    #[test]
    fn exposure_is_rejected() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        for index in 1..=10 {
            let image = ImageBuffer::from_pixel(
                32,
                32,
                Luma([if index == 5 { 255 } else { (index * 10) as u8 }]),
            );
            image
                .save(input.path().join(format!("frame_{index:06}.png")))
                .unwrap();
        }
        let mut exposure_config = config();
        exposure_config.blur_threshold = -1.0;
        let result = filter_frames(input.path(), output.path(), &exposure_config).unwrap();
        assert!(result.rejected_exposure >= 1);
        let csv = fs::read_to_string(output.path().join("metadata.csv")).unwrap();
        assert!(csv
            .lines()
            .any(|line| line.contains("frame_000005.png") && line.contains(",false,exposure")));
    }

    #[test]
    fn summary_records_the_strategy_version() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_frames(input.path(), 3, &[]);
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        assert_eq!(result.strategy_version, FILTER_STRATEGY_VERSION);
        let summary = fs::read_to_string(output.path().join("filter_summary.json")).unwrap();
        assert!(summary.contains(&format!("\"strategyVersion\": {FILTER_STRATEGY_VERSION}")));
    }

    #[test]
    fn all_blurred_windows_use_forced_keep() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_frames(input.path(), 20, &(1..=20).collect::<Vec<_>>());
        let mut blurred_config = config();
        blurred_config.blur_threshold = 1_000_000.0;

        let result = filter_frames(input.path(), output.path(), &blurred_config).unwrap();
        assert_eq!(result.kept_frames, 2);
        let mut forced_log = fs::read_to_string(output.path().join("filter_forced_keep.log"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let header = forced_log.remove(0);
        assert_eq!(header, "显式 forced_keep 帧数：2");
        let forced_numbers = forced_log
            .iter()
            .map(|name| frame_number(name))
            .collect::<Vec<_>>();
        assert_eq!(forced_numbers.len(), 2);
        assert!((1..=10).contains(&forced_numbers[0]));
        assert!((11..=20).contains(&forced_numbers[1]));
        let metadata = fs::read_to_string(output.path().join("metadata.csv")).unwrap();
        assert_eq!(
            metadata
                .lines()
                .filter(|line| line.ends_with(",true,"))
                .count(),
            2
        );
    }

    #[test]
    fn redundant_frames_are_bounded_by_windows() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        for index in 1..=20 {
            textured(1, false)
                .save(input.path().join(format!("frame_{index:06}.jpg")))
                .unwrap();
        }
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        assert!(result.kept_frames <= 2 * config().keep_per_window);
    }

    #[test]
    fn retained_indices_are_strictly_increasing_and_have_no_window_hole() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_frames(input.path(), 30, &[2, 3, 12, 13, 22]);
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        let indices = result
            .kept_file_names
            .iter()
            .map(|name| frame_number(name))
            .collect::<Vec<_>>();
        assert!(indices
            .windows(2)
            .all(|pair| pair[0] < pair[1] && pair[1] - pair[0] <= config().window_size as u64));
    }

    #[test]
    fn noise_has_higher_laplacian_variance_than_solid_color() {
        let solid = ImageBuffer::from_pixel(64, 64, Luma([100]));
        let noisy = textured(4, false);
        assert!(gray_metrics(&noisy, None).0 > gray_metrics(&solid, None).0 * 3.0);
    }

    #[test]
    fn metadata_contains_one_row_per_input_frame() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_frames(input.path(), 20, &[]);
        filter_frames(input.path(), output.path(), &config()).unwrap();
        assert_eq!(
            fs::read_to_string(output.path().join("metadata.csv"))
                .unwrap()
                .lines()
                .count(),
            21
        );
    }

    // ---- S1.1：时序冗余筛选（跨窗口参考 + 灰度/梯度度量）----

    /// 整体提亮：像素值加常量但**不裁切**，因此灰度直方图只平移、结构不变。
    fn shift_brightness(
        image: &ImageBuffer<Luma<u8>, Vec<u8>>,
        delta: u8,
    ) -> ImageBuffer<Luma<u8>, Vec<u8>> {
        ImageBuffer::from_fn(image.width(), image.height(), |x, y| {
            let value = image.get_pixel(x, y)[0].saturating_add(delta);
            // 裁切会引入真实的结构变化，测试里必须避免。
            assert!(image.get_pixel(x, y)[0] as u16 + delta as u16 <= 255);
            Luma([value])
        })
    }

    /// 只放两帧并返回保留帧名，用于直接观察冗余判定结果。
    fn kept_names_for_pair(
        config: &FrameFilterConfig,
        first: &ImageBuffer<Luma<u8>, Vec<u8>>,
        second: &ImageBuffer<Luma<u8>, Vec<u8>>,
    ) -> Vec<String> {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_png(input.path(), 1, first);
        save_png(input.path(), 2, second);
        filter_frames(input.path(), output.path(), config)
            .unwrap()
            .kept_file_names
    }

    fn is_kept(names: &[String], index: u32) -> bool {
        names.contains(&format!("frame_{index:06}.png"))
    }

    #[test]
    fn redundancy_reference_carries_across_window_boundaries() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        let first = textured(1, false);
        let second = textured(7, false);
        let third = textured(13, false);
        for index in 1..=5 {
            save_png(input.path(), index, &first);
        }
        for index in 6..=10 {
            save_png(input.path(), index, &second);
        }
        for index in 11..=15 {
            save_png(input.path(), index, &second);
        }
        for index in 16..=20 {
            save_png(input.path(), index, &third);
        }
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        let kept = &result.kept_file_names;
        // 第 6 帧与窗口首帧不同 → 保留；第 11 帧与上一窗口最后保留的帧内容相同，
        // 跨窗口参考下必须判为冗余。v2 每窗重置参考、无条件接纳窗口首帧，会保留第 11 帧。
        assert!(is_kept(kept, 6), "第 6 帧应保留：{kept:?}");
        assert!(
            !is_kept(kept, 11),
            "第 11 帧与上一窗口末张保留帧相同，应判为冗余：{kept:?}"
        );
        assert!(is_kept(kept, 16), "第 16 帧结构变化明显，应保留：{kept:?}");
    }

    #[test]
    fn redundancy_compares_against_the_last_kept_frame_not_the_previous_input_frame() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        let frame = textured(1, false);
        save_png(input.path(), 1, &frame);
        // 中间帧会被质量门拒绝，因此它绝不能成为参考帧。
        save_png(input.path(), 2, &textured(2, true));
        save_png(input.path(), 3, &frame);
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        assert!(
            !is_kept(&result.kept_file_names, 3),
            "第 3 帧应与最近**保留**帧（第 1 帧）比较而被判冗余：{:?}",
            result.kept_file_names
        );
        let csv = fs::read_to_string(output.path().join("metadata.csv")).unwrap();
        assert!(csv
            .lines()
            .any(|line| line.contains("frame_000003.png") && line.contains(",false,redundant")));
    }

    #[test]
    fn gray_diff_is_brightness_normalised() {
        let base = textured(1, false);
        let brighter = shift_brightness(&base, 40);
        let base_reduced = reduced_gray(&base, 96);
        let brighter_reduced = reduced_gray(&brighter, 96);
        let diff = normalized_gray_difference(&base_reduced, &brighter_reduced);
        // 亮度归一化减去各自均值后，纯提亮不构成结构变化。
        assert!(diff < 0.01, "亮度整体偏移不应算成差异，实测 {diff}");
        // 对照：v2 使用的未归一化平均绝对差会把整帧提亮算成"有明显变化"。
        assert!(mean_absolute_difference(&base_reduced, &brighter_reduced) > 0.1);
    }

    #[test]
    fn gradient_diff_ignores_brightness_and_sees_structure() {
        let base = textured(1, false);
        let brighter = shift_brightness(&base, 40);
        // 同样的平均亮度量级，但结构完全不同。
        let restructured = ImageBuffer::from_fn(64, 64, |x, y| {
            Luma([if (x / 2 + y / 2) % 2 == 0 { 60 } else { 160 }])
        });
        let base_gradient = gradient_magnitude(&reduced_gray(&base, 96));
        let brightness_diff = gradient_difference(
            &base_gradient,
            &gradient_magnitude(&reduced_gray(&brighter, 96)),
        );
        let structure_diff = gradient_difference(
            &base_gradient,
            &gradient_magnitude(&reduced_gray(&restructured, 96)),
        );
        assert_eq!(brightness_diff, 0.0, "纯提亮不改变梯度幅值图");
        assert!(
            structure_diff > 0.01,
            "结构变化必须体现在梯度差异上，实测 {structure_diff}"
        );
    }

    #[test]
    fn dual_metric_rejects_a_pure_brightness_shift_the_legacy_metric_keeps() {
        let base = textured(1, false);
        let brighter = shift_brightness(&base, 40);
        let legacy = kept_names_for_pair(&config(), &base, &brighter);
        assert!(
            is_kept(&legacy, 2),
            "v2 判定量把整帧提亮当成变化，会保留第二帧：{legacy:?}"
        );

        let mut dual = config();
        dual.redundancy_metric = RedundancyMetric::DualThreshold;
        dual.redundancy_gray_threshold = 0.02;
        dual.redundancy_gradient_threshold = 0.02;
        let with_dual = kept_names_for_pair(&dual, &base, &brighter);
        assert!(
            !is_kept(&with_dual, 2),
            "归一化后没有结构变化，双阈值判定应判为冗余：{with_dual:?}"
        );
    }

    #[test]
    fn dual_metric_keeps_a_frame_whose_structure_changed() {
        let mut dual = config();
        dual.redundancy_metric = RedundancyMetric::DualThreshold;
        dual.redundancy_gray_threshold = 0.02;
        dual.redundancy_gradient_threshold = 0.02;
        let kept = kept_names_for_pair(&dual, &textured(1, false), &textured(9, false));
        assert!(
            is_kept(&kept, 2),
            "结构明显变化时必须保留，不能被双阈值误杀：{kept:?}"
        );
    }

    /// 构造一帧已解码的分析结果，供直接驱动 `decide_windows` 的单元级用例使用。
    fn analyzed(image: &ImageBuffer<Luma<u8>, Vec<u8>>, index: u32) -> AnalyzedFrame {
        let laplacian_variance = gray_metrics(image, None).0;
        analyzed_with(image, index, laplacian_variance)
    }

    /// 同上，但清晰度由调用方指定：运动候选用例需要精确落在
    /// `[blur_threshold * ratio, blur_threshold)` 这个"被门限拒下但细节尚可"的区间里。
    fn analyzed_with(
        image: &ImageBuffer<Luma<u8>, Vec<u8>>,
        index: u32,
        laplacian_variance: f64,
    ) -> AnalyzedFrame {
        let gray = image.clone();
        let reduced = reduced_gray(&gray, 96);
        let gradient = gradient_magnitude(&reduced);
        let (_, overexposure_ratio, underexposure_ratio) = gray_metrics(&gray, None);
        let frame_name = format!("frame_{index:06}.png");
        AnalyzedFrame {
            path: PathBuf::from(&frame_name),
            gray,
            reduced,
            gradient,
            metrics: FrameMetrics {
                frame_name,
                timestamp_ms: 0,
                laplacian_variance,
                overexposure_ratio,
                underexposure_ratio,
                diff_score: 1.0,
                gray_diff: 0.0,
                gradient_diff: 0.0,
                motion_score: 0.0,
                is_motion_candidate: false,
                effective_gray_threshold: 0.0,
                effective_gradient_threshold: 0.0,
                kept: false,
                reject_reason: None,
                forced_keep: false,
            },
        }
    }

    #[test]
    fn decide_windows_records_metrics_against_the_carried_reference() {
        let config = config();
        let mut frames = (1..=5)
            .map(|index| analyzed(&textured(1, false), index))
            .collect::<Vec<_>>();
        frames.extend((6..=10).map(|index| analyzed(&textured(9, false), index)));
        decide_windows(&mut frames, &config);

        // 序列首帧没有参考帧，三项度量保持初值。
        assert!(frames[0].metrics.kept);
        assert_eq!(frames[0].metrics.gray_diff, 0.0);
        assert_eq!(frames[0].metrics.gradient_diff, 0.0);

        // 与参考完全相同的后续帧：判为冗余，且差异确实为 0（说明比较真的发生了）。
        assert_eq!(
            frames[1].metrics.reject_reason.as_deref(),
            Some("redundant")
        );
        assert_eq!(frames[1].metrics.gray_diff, 0.0);
        assert_eq!(frames[1].metrics.motion_score, 0.0);

        // 结构变化帧：保留，并把与参考的差异记录下来。
        assert!(frames[5].metrics.kept, "结构变化的帧必须保留");
        assert!(frames[5].metrics.gray_diff > 0.0);
        assert!(frames[5].metrics.gradient_diff > 0.0);
        assert!(
            (frames[5].metrics.motion_score
                - (config.gray_diff_weight * frames[5].metrics.gray_diff
                    + config.gradient_diff_weight * frames[5].metrics.gradient_diff))
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn strategy_version_is_three_and_enters_the_hash() {
        assert_eq!(FILTER_STRATEGY_VERSION, 3);
        let hash = filter_config_hash(&config(), 20);
        assert!(hash.starts_with("fnv1a-"));
        // 版本或判定量变化必须改变哈希，否则旧缓存会被静默复用。
        let mut dual = config();
        dual.redundancy_metric = RedundancyMetric::DualThreshold;
        assert_ne!(hash, filter_config_hash(&dual, 20));
    }

    #[test]
    fn every_new_config_field_changes_the_hash() {
        let base = filter_config_hash(&config(), 20);
        let variants = [
            FrameFilterConfig {
                diff_analysis_max_edge: 64,
                ..config()
            },
            FrameFilterConfig {
                redundancy_gray_threshold: 0.03,
                ..config()
            },
            FrameFilterConfig {
                redundancy_gradient_threshold: 0.03,
                ..config()
            },
            FrameFilterConfig {
                gray_diff_weight: 0.5,
                ..config()
            },
            FrameFilterConfig {
                gradient_diff_weight: 0.5,
                ..config()
            },
            FrameFilterConfig {
                redundancy_metric: RedundancyMetric::DualThreshold,
                ..config()
            },
            FrameFilterConfig {
                enable_motion_candidates: true,
                ..config()
            },
            FrameFilterConfig {
                motion_candidate_quota: 2,
                ..config()
            },
            FrameFilterConfig {
                motion_candidate_min_quality_ratio: 0.9,
                ..config()
            },
            FrameFilterConfig {
                enable_adaptive_threshold: true,
                ..config()
            },
            FrameFilterConfig {
                adaptive_window_size: 64,
                ..config()
            },
            FrameFilterConfig {
                adaptive_threshold_fraction: 0.25,
                ..config()
            },
        ];
        for variant in variants {
            assert_ne!(
                base,
                filter_config_hash(&variant, 20),
                "配置变化必须让哈希失效：{variant:?}"
            );
        }
    }

    #[test]
    fn default_redundancy_metric_is_the_legacy_one() {
        // 阈值标定完成前不允许改变默认筛选行为。
        assert_eq!(
            FrameFilterConfig::balanced().redundancy_metric,
            RedundancyMetric::LegacyGrayDiff
        );
        assert_eq!(
            FrameFilterConfig::fast().redundancy_metric,
            RedundancyMetric::LegacyGrayDiff
        );
        assert_eq!(
            FrameFilterConfig::high().redundancy_metric,
            RedundancyMetric::LegacyGrayDiff
        );
    }

    #[test]
    fn identical_frames_still_keep_a_bounded_contiguous_subset() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        for index in 1..=30 {
            save_png(input.path(), index, &textured(1, false));
        }
        let result = filter_frames(input.path(), output.path(), &config()).unwrap();
        assert!(result.kept_frames >= 1, "不允许输出空集");
        let indices = result
            .kept_file_names
            .iter()
            .map(|name| frame_number(name))
            .collect::<Vec<_>>();
        assert!(indices
            .windows(2)
            .all(|pair| pair[1] - pair[0] <= config().window_size as u64));
        // 静止素材不应被逐帧填满：跨窗参考 + 中点回填的组合必须保持稀疏。
        assert!(
            result.kept_frames <= 2 * config().keep_per_window,
            "静止序列的保留量应受窗口配额约束，实测 {}",
            result.kept_frames
        );
    }

    fn percentile(sorted: &[f64], fraction: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
        sorted[index]
    }

    // ---- S1.5：审计产物 ----

    /// 每一帧都必须能回答"为什么保留/为什么被拒"：新增列齐全，且列数与表头一致。
    #[test]
    fn metadata_records_the_new_metrics_and_effective_thresholds() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_frames(input.path(), 20, &[]);
        let config = config();
        filter_frames(input.path(), output.path(), &config).unwrap();

        let csv = fs::read_to_string(output.path().join("metadata.csv")).unwrap();
        let header = csv.lines().next().unwrap();
        for column in [
            "gray_diff",
            "gradient_diff",
            "motion_score",
            "is_motion_candidate",
            "effective_gray_threshold",
            "effective_gradient_threshold",
        ] {
            assert!(header.contains(column), "缺少审计列 {column}：{header}");
        }
        let columns = header.split(',').collect::<Vec<_>>();
        let expected = columns.len();
        for row in csv.lines().skip(1) {
            assert_eq!(row.split(',').count(), expected, "列数与表头不一致：{row}");
        }

        // 自适应关闭时，实际生效门限必须等于配置里的绝对阈值（而不是 0 或未初始化值）。
        let gate_index = columns
            .iter()
            .position(|column| *column == "effective_gray_threshold")
            .unwrap();
        let first_row = csv.lines().nth(1).unwrap();
        let gate = first_row.split(',').nth(gate_index).unwrap();
        assert_eq!(
            gate.parse::<f64>().unwrap(),
            config.redundancy_gray_threshold
        );
    }

    #[test]
    fn summary_records_config_candidate_fps_and_motion_candidates() {
        let input = tempdir().unwrap();
        let output = tempdir().unwrap();
        save_frames(input.path(), 20, &[]);
        let config = config();
        let outcome = filter_frames_at_fps(input.path(), output.path(), &config, 12.5).unwrap();
        assert_eq!(outcome.strategy_version, FILTER_STRATEGY_VERSION);
        assert_eq!(outcome.candidate_fps, 12.5, "候选密度必须如实记录");
        assert_eq!(outcome.motion_candidates, 0, "默认不开运动候选");
        assert_eq!(outcome.config.keep_per_window, config.keep_per_window);
        assert_eq!(
            outcome.adaptive_gray_threshold_p50, config.redundancy_gray_threshold,
            "未启用自适应时统计值应等于绝对阈值"
        );

        let summary = fs::read_to_string(output.path().join("filter_summary.json")).unwrap();
        for key in [
            "\"config\"",
            "\"candidateFps\": 12.5",
            "\"motionCandidates\": 0",
            "\"adaptiveGrayThresholdP50\"",
            "\"strategyVersion\": 3",
        ] {
            assert!(summary.contains(key), "摘要缺少 {key}：{summary}");
        }
    }

    // ---- S1.4：自适应冗余阈值 ----

    fn recent(values: &[f64]) -> RecentDiffs {
        let mut diffs = RecentDiffs::new(values.len().max(1));
        for value in values {
            diffs.push(*value);
        }
        diffs
    }

    /// 空窗口（序列开头）退回绝对阈值：第一对比较的行为必须是确定的。
    #[test]
    fn adaptive_threshold_falls_back_to_the_absolute_floor() {
        let empty = recent(&[]);
        assert_eq!(adaptive_threshold(0.02, &empty, 0.5), 0.02);
    }

    /// 门限只上浮、不下潜：绝对阈值始终是下限。
    #[test]
    fn adaptive_threshold_never_drops_below_the_absolute_floor() {
        // 整段都极其相似：低分位数远小于绝对阈值，门限必须停在绝对阈值上。
        let tiny = recent(&[0.001, 0.002, 0.0015, 0.003]);
        assert_eq!(adaptive_threshold(0.02, &tiny, 0.5), 0.02);
        // 运动幅度大：门限随低分位数上浮。
        let lively = recent(&[0.12, 0.15, 0.14, 0.13, 0.16]);
        let gate = adaptive_threshold(0.02, &lively, 0.5);
        assert!(gate > 0.02, "运动幅度大时门限应上浮，实测 {gate}");
        assert!((gate - 0.5 * 0.12).abs() < 1e-12, "系数应作用在低分位数上");
    }

    /// 窗口是 FIFO：加入更近期的差异后，旧值必须被挤出去（门限跟着当前素材走）。
    #[test]
    fn adaptive_window_is_a_fifo_of_the_recent_differences() {
        let mut diffs = RecentDiffs::new(3);
        for value in [0.30, 0.30, 0.30] {
            diffs.push(value);
        }
        assert_eq!(diffs.values().len(), 3);
        for value in [0.05, 0.05, 0.05] {
            diffs.push(value);
        }
        assert_eq!(diffs.values(), &[0.05, 0.05, 0.05]);
        assert!(adaptive_threshold(0.02, &diffs, 0.5) < 0.03);
    }

    /// 自适应默认关闭；开启后必须只在 DualThreshold 下改变判定（Legacy 不受影响）。
    #[test]
    fn adaptive_threshold_is_disabled_by_default() {
        assert!(!FrameFilterConfig::balanced().enable_adaptive_threshold);
        assert_eq!(FrameFilterConfig::balanced().adaptive_window_size, 20);
        assert_eq!(
            FrameFilterConfig::balanced().adaptive_threshold_fraction,
            0.5
        );
        let config = config();
        let gates = effective_redundancy_gates(
            &config,
            &recent(&[0.5, 0.5, 0.5]),
            &recent(&[0.5, 0.5, 0.5]),
        );
        assert_eq!(
            gates,
            (
                config.redundancy_gray_threshold,
                config.redundancy_gradient_threshold
            )
        );
    }

    /// 行为级：一段大位移之后紧跟的小位移帧，绝对门限会保留，自适应门限应剔除。
    #[test]
    fn adaptive_threshold_prunes_the_least_informative_frames_of_a_lively_sequence() {
        // 白底 + 竖直暗条，位移以像素计：位移越大差异越大。
        fn bar(shift: u32) -> ImageBuffer<Luma<u8>, Vec<u8>> {
            ImageBuffer::from_fn(64, 64, |x, y| {
                let start = shift % 48;
                if (start..start + 12).contains(&x) && (20..44).contains(&y) {
                    Luma([0])
                } else {
                    Luma([255])
                }
            })
        }
        let build = |config: &FrameFilterConfig| {
            let mut frames = Vec::new();
            // 前 6 帧每次跳 12 像素：差异远高于任何门限，同时把观察窗口"抬高"。
            for (index, shift) in [0_u32, 12, 24, 36, 48, 12].iter().enumerate() {
                frames.push(analyzed(&bar(*shift), index as u32 + 1));
            }
            // 随后只有 1 像素 / 2 像素的小位移：介于绝对下限与上浮后的门限之间。
            frames.push(analyzed(&bar(13), 7));
            frames.push(analyzed(&bar(14), 8));
            let mut config = *config;
            config.window_size = 20;
            config.keep_per_window = 20;
            decide_windows(&mut frames, &config);
            frames
        };

        let absolute = FrameFilterConfig {
            redundancy_metric: RedundancyMetric::DualThreshold,
            // 本样本是纯平移：梯度幅值图近似平移不变（实测大位移帧的 gradient_diff 只有
            // 0.017–0.029，而 gray_diff 达 0.141）。把梯度门限抬高以隔离灰度通道，
            // 两个配置的这一项完全相同，因此差异只可能来自自适应门限本身。
            redundancy_gradient_threshold: 0.5,
            ..config()
        };
        let adaptive = FrameFilterConfig {
            enable_adaptive_threshold: true,
            ..absolute
        };

        let with_absolute = build(&absolute);
        let with_adaptive = build(&adaptive);

        // 2 像素位移：绝对门限下是有信息量的新帧。
        assert!(
            with_absolute[7].metrics.kept,
            "绝对门限下 2 像素位移应被保留，实测 gray_diff={} gradient_diff={}",
            with_absolute[7].metrics.gray_diff, with_absolute[7].metrics.gradient_diff
        );
        // 同一帧在自适应门限下被剔除：相对前段的大位移而言它没有新信息。
        assert!(
            !with_adaptive[7].metrics.kept,
            "自适应门限下应剔除最小位移帧，实测 gray_diff={} gradient_diff={}",
            with_adaptive[7].metrics.gray_diff, with_adaptive[7].metrics.gradient_diff
        );
        assert_eq!(
            with_adaptive[7].metrics.reject_reason.as_deref(),
            Some("redundant")
        );
    }

    // ---- S1.2：运动候选池 ----

    /// 运动候选启用时的统一配置：配额 1、质量下限 0.6。
    fn motion_config() -> FrameFilterConfig {
        FrameFilterConfig {
            enable_motion_candidates: true,
            motion_candidate_quota: 1,
            motion_candidate_min_quality_ratio: 0.6,
            enable_adaptive_threshold: false,
            adaptive_window_size: 20,
            adaptive_threshold_fraction: 0.5,
            ..config()
        }
    }

    /// 清晰锚点 → 模糊但差异明显 → 清晰帧：窗口清晰帧只有 2 张（配额 3），留出 1 个名额。
    fn motion_fixture(config: &FrameFilterConfig) -> Vec<AnalyzedFrame> {
        let floor = config.blur_threshold * config.motion_candidate_min_quality_ratio;
        vec![
            analyzed_with(&textured(1, false), 1, config.blur_threshold * 3.0),
            analyzed_with(&textured(9, true), 2, floor + 1.0),
            analyzed_with(&textured(21, false), 3, config.blur_threshold * 3.0),
        ]
    }

    #[test]
    fn motion_candidates_are_disabled_by_default() {
        assert!(!FrameFilterConfig::balanced().enable_motion_candidates);
        let config = FrameFilterConfig {
            enable_motion_candidates: false,
            ..motion_config()
        };
        let mut frames = motion_fixture(&config);
        decide_windows(&mut frames, &config);
        // 关闭时模糊帧必须保持被拒：默认结果与 v2 一致。
        assert!(!frames[1].metrics.kept);
        assert!(!frames[1].metrics.is_motion_candidate);
        assert_eq!(frames[1].metrics.reject_reason.as_deref(), Some("blur"));
    }

    #[test]
    fn blurred_frame_with_change_enters_the_motion_pool_when_enabled() {
        let config = motion_config();
        let mut frames = motion_fixture(&config);
        decide_windows(&mut frames, &config);
        assert!(
            frames[1].metrics.kept,
            "模糊但确有明显变化、且清晰度在 0.6 倍下限之上的帧应进入运动候选"
        );
        assert!(frames[1].metrics.is_motion_candidate);
        assert_eq!(frames[1].metrics.reject_reason, None);
        assert!(frames[1].metrics.gray_diff > 0.0);
    }

    #[test]
    fn too_blurry_frames_are_not_motion_candidates() {
        let config = motion_config();
        let floor = config.blur_threshold * config.motion_candidate_min_quality_ratio;
        let mut frames = vec![
            analyzed_with(&textured(1, false), 1, config.blur_threshold * 3.0),
            analyzed_with(&textured(9, true), 2, floor - 1.0),
            analyzed_with(&textured(21, false), 3, config.blur_threshold * 3.0),
        ];
        decide_windows(&mut frames, &config);
        assert!(
            !frames[1].metrics.kept,
            "低于质量下限的帧不能因为「有变化」就被放进来"
        );
        assert_eq!(frames[1].metrics.reject_reason.as_deref(), Some("blur"));
    }

    #[test]
    fn motion_candidates_never_displace_clear_frames() {
        // 清晰帧已经占满窗口配额，此时运动候选一张都不该进来。
        let config = FrameFilterConfig {
            keep_per_window: 2,
            ..motion_config()
        };
        let floor = config.blur_threshold * config.motion_candidate_min_quality_ratio;
        let mut frames = vec![
            analyzed_with(&textured(1, false), 1, config.blur_threshold * 3.0),
            analyzed_with(&textured(9, true), 2, floor + 1.0),
            analyzed_with(&textured(21, false), 3, config.blur_threshold * 3.0),
            analyzed_with(&textured(33, false), 4, config.blur_threshold * 3.0),
        ];
        decide_windows(&mut frames, &config);
        assert_eq!(
            frames.iter().filter(|frame| frame.metrics.kept).count(),
            config.keep_per_window,
            "窗口保留量必须等于配额"
        );
        assert!(
            !frames.iter().any(|frame| frame.metrics.is_motion_candidate),
            "清晰帧充足时不允许运动候选占名额"
        );
        assert!(!frames[1].metrics.kept);
    }

    #[test]
    fn motion_candidates_are_bounded_by_the_quota_and_deduplicated() {
        // 同一窗口里连续 4 张内容相同的模糊帧：只有第一张能进来（配额 1），
        // 其余会与刚入选的候选比对而被判重复。
        let config = motion_config();
        let floor = config.blur_threshold * config.motion_candidate_min_quality_ratio;
        let mut frames = vec![
            analyzed_with(&textured(1, false), 1, config.blur_threshold * 3.0),
            analyzed_with(&textured(9, true), 2, floor + 1.0),
            analyzed_with(&textured(9, true), 3, floor + 1.0),
            analyzed_with(&textured(9, true), 4, floor + 1.0),
            analyzed_with(&textured(9, true), 5, floor + 1.0),
        ];
        decide_windows(&mut frames, &config);
        let admitted = frames
            .iter()
            .filter(|frame| frame.metrics.is_motion_candidate)
            .count();
        assert_eq!(admitted, 1, "候选数量必须受配额约束");
        assert!(frames[1].metrics.kept);
        assert!(!frames[2].metrics.kept, "与刚入选的候选重复，不应再次入选");
    }

    #[test]
    fn motion_candidates_do_not_raise_the_window_bound() {
        let config = motion_config();
        let floor = config.blur_threshold * config.motion_candidate_min_quality_ratio;
        let mut frames = Vec::new();
        for window in 0..3u32 {
            frames.push(analyzed_with(
                &textured(1, false),
                window * 10 + 1,
                config.blur_threshold * 3.0,
            ));
            for slot in 1..10u32 {
                frames.push(analyzed_with(
                    &textured(9, true),
                    window * 10 + 1 + slot,
                    floor + 1.0,
                ));
            }
        }
        decide_windows(&mut frames, &config);
        let kept = frames.iter().filter(|frame| frame.metrics.kept).count();
        let bound = 3 * config.keep_per_window;
        assert!(
            kept <= bound,
            "运动候选不得抬高窗口配额上界：kept={kept} bound={bound}"
        );
    }

    /// 标定与性能实测：对真实素材打印两套度量的分布与每帧分析耗时。
    ///
    /// 默认 `#[ignore]`，不参与常规测试；需要显式运行：
    /// `OOOSPLAT_CALIBRATION_DIR=<帧目录> cargo test --lib calibration_report -- --ignored --nocapture`
    /// 对同一项目的 `work/frames`（密集、多为近重复）与 `work/frames_filtered`（已保留）
    /// 各跑一次，两条分布之间就是阈值应当落进去的区间。只读帧，不写任何项目文件。
    #[test]
    #[ignore]
    fn calibration_report_for_real_frames() {
        let Some(dir) = std::env::var_os("OOOSPLAT_CALIBRATION_DIR") else {
            eprintln!("未设置 OOOSPLAT_CALIBRATION_DIR，跳过");
            return;
        };
        let dir = PathBuf::from(dir);
        let mut paths = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file() && is_image_file(path))
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| natural_name(path));
        assert!(!paths.is_empty(), "目录中没有可用帧：{}", dir.display());

        // 等距抽样，控制标定耗时且不受序列长度影响。设 OOOSPLAT_CALIBRATION_CONSECUTIVE=1
        // 改为取**连续**帧：只有相邻帧之间的差异才能回答"当前生效的门限到底会不会触发"。
        let consecutive = std::env::var("OOOSPLAT_CALIBRATION_CONSECUTIVE").is_ok();
        let sample = 120.min(paths.len());
        let stride = if consecutive {
            1
        } else {
            paths.len().div_ceil(sample)
        };
        let sampled = paths
            .iter()
            .step_by(stride)
            .take(sample)
            .collect::<Vec<_>>();

        let config = FrameFilterConfig::balanced();
        let started = std::time::Instant::now();
        let frames = sampled
            .iter()
            .map(|path| analyze_frame(path, &config, None, 30.0).unwrap())
            .collect::<Vec<_>>();
        let analysis_elapsed = started.elapsed();

        // 把新增部分的成本单独测出来，才能与"既有分析成本"比较（构建 profile 相同才有可比性）。
        let started_new = std::time::Instant::now();
        for frame in &frames {
            let reduced = reduced_gray(&frame.gray, config.diff_analysis_max_edge);
            let _ = gradient_magnitude(&reduced);
        }
        let new_metric_elapsed = started_new.elapsed();
        let started_diff = std::time::Instant::now();
        for pair in frames.windows(2) {
            let _ = normalized_gray_difference(&pair[0].reduced, &pair[1].reduced);
            let _ = gradient_difference(&pair[0].gradient, &pair[1].gradient);
        }
        let diff_elapsed = started_diff.elapsed();

        let mut gray = Vec::new();
        let mut gradient = Vec::new();
        for pair in frames.windows(2) {
            gray.push(normalized_gray_difference(
                &pair[0].reduced,
                &pair[1].reduced,
            ));
            gradient.push(gradient_difference(&pair[0].gradient, &pair[1].gradient));
        }
        assert!(!gray.is_empty(), "至少需要两帧才能统计相邻差异");
        let mut gray_sorted = gray.clone();
        let mut gradient_sorted = gradient.clone();
        gray_sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
        gradient_sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));

        // 自适应门限的实际效果：显式开启后按因果顺序重放这套比较，统计会被判为冗余的比例。
        // 注意必须显式打开——档位默认是关闭的，否则量到的是"未启用"。
        let adaptive_config = FrameFilterConfig {
            enable_adaptive_threshold: true,
            redundancy_metric: RedundancyMetric::DualThreshold,
            ..config
        };
        let mut recent_gray = RecentDiffs::new(adaptive_config.adaptive_window_size);
        let mut recent_gradient = RecentDiffs::new(adaptive_config.adaptive_window_size);
        let mut redundant_absolute = 0usize;
        let mut redundant_adaptive = 0usize;
        let mut gate_gray = Vec::new();
        for (gray_diff, gradient_diff) in gray.iter().zip(&gradient) {
            if *gray_diff < adaptive_config.redundancy_gray_threshold
                && *gradient_diff < adaptive_config.redundancy_gradient_threshold
            {
                redundant_absolute += 1;
            }
            let (gray_gate, gradient_gate) =
                effective_redundancy_gates(&adaptive_config, &recent_gray, &recent_gradient);
            gate_gray.push(gray_gate);
            if *gray_diff < gray_gate && *gradient_diff < gradient_gate {
                redundant_adaptive += 1;
            }
            recent_gray.push(*gray_diff);
            recent_gradient.push(*gradient_diff);
        }
        let mut gate_sorted = gate_gray.clone();
        gate_sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
        let pairs = gray.len() as f64;

        let count = frames.len() as f64;
        let analysis_ms = analysis_elapsed.as_secs_f64() * 1000.0 / count;
        let new_ms = new_metric_elapsed.as_secs_f64() * 1000.0 / count;
        let diff_ms = diff_elapsed.as_secs_f64() * 1000.0 / (count - 1.0).max(1.0);
        println!("=== 标定：{} ===", dir.display());
        println!("抽样 {} / 共 {} 帧", frames.len(), paths.len());
        println!(
            "耗时（当前构建 profile）：既有分析 {analysis_ms:.2} ms/帧；新增小图+梯度 {new_ms:.2} ms/帧（占既有 {:.1}%）；每对差异 {diff_ms:.2} ms",
            new_ms / analysis_ms.max(f64::MIN_POSITIVE) * 100.0
        );
        for (name, values) in [
            ("gray_diff", &gray_sorted),
            ("gradient_diff", &gradient_sorted),
        ] {
            println!(
                "{name}: min={:.4} p01={:.4} p10={:.4} p25={:.4} p50={:.4} p75={:.4} p90={:.4} max={:.4}",
                values[0],
                percentile(values, 0.01),
                percentile(values, 0.10),
                percentile(values, 0.25),
                percentile(values, 0.50),
                percentile(values, 0.75),
                percentile(values, 0.90),
                values[values.len() - 1],
            );
        }
        println!(
            "冗余判定（DualThreshold，{} 个相邻对）：绝对门限剔 {:.1}%；自适应门限剔 {:.1}%（门限 p50={:.4} p90={:.4}）",
            gray.len(),
            redundant_absolute as f64 / pairs * 100.0,
            redundant_adaptive as f64 / pairs * 100.0,
            percentile(&gate_sorted, 0.50),
            percentile(&gate_sorted, 0.90),
        );

        // 当前**默认生效**的判定量：480px 未归一化平均绝对差，阈值 min_diff_score。
        // 若真实相邻帧几乎都远高于它，"跨窗口参考"这类改动在实拍素材上就是零效果。
        let legacy = frames
            .windows(2)
            .map(|pair| mean_absolute_difference(&pair[0].gray, &pair[1].gray))
            .collect::<Vec<_>>();
        let mut legacy_sorted = legacy.clone();
        legacy_sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
        let below = legacy
            .iter()
            .filter(|value| **value < config.min_diff_score)
            .count() as f64
            / legacy.len().max(1) as f64
            * 100.0;
        println!(
            "旧判定量 diff_score(480px MAE)：min={:.4} p10={:.4} p50={:.4} p90={:.4}；低于 min_diff_score={:.2} 的相邻对占比 {:.1}%",
            legacy_sorted[0],
            percentile(&legacy_sorted, 0.10),
            percentile(&legacy_sorted, 0.50),
            percentile(&legacy_sorted, 0.90),
            config.min_diff_score,
            below,
        );
    }
}
