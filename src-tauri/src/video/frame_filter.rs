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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameFilterConfig {
    pub blur_threshold: f64,
    pub overexposure_ratio: f64,
    pub underexposure_ratio: f64,
    pub window_size: usize,
    pub keep_per_window: usize,
    pub analysis_max_edge: u32,
    pub min_diff_score: f64,
}

impl FrameFilterConfig {
    pub const fn fast() -> Self {
        Self {
            blur_threshold: 60.0,
            overexposure_ratio: 0.02,
            underexposure_ratio: 0.02,
            window_size: 10,
            keep_per_window: 2,
            analysis_max_edge: 480,
            min_diff_score: 0.02,
        }
    }

    pub const fn balanced() -> Self {
        Self {
            blur_threshold: 100.0,
            overexposure_ratio: 0.02,
            underexposure_ratio: 0.02,
            window_size: 10,
            keep_per_window: 3,
            analysis_max_edge: 480,
            min_diff_score: 0.02,
        }
    }

    pub const fn high() -> Self {
        Self {
            blur_threshold: 100.0,
            overexposure_ratio: 0.02,
            underexposure_ratio: 0.02,
            window_size: 10,
            keep_per_window: 4,
            analysis_max_edge: 480,
            min_diff_score: 0.02,
        }
    }
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
    pub kept: bool,
    pub reject_reason: Option<String>,
    pub forced_keep: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterOutcome {
    pub total_frames: usize,
    pub kept_frames: usize,
    pub rejected_blur: usize,
    pub rejected_exposure: usize,
    pub rejected_redundant: usize,
    pub rejected_window_overflow: u32,
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
    metrics: FrameMetrics,
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
        total_frames: frames.len(),
        kept_frames: frames.iter().filter(|frame| frame.metrics.kept).count(),
        rejected_blur: frames
            .iter()
            .filter(|frame| frame.metrics.reject_reason.as_deref() == Some("blur"))
            .count(),
        rejected_exposure: frames
            .iter()
            .filter(|frame| frame.metrics.reject_reason.as_deref() == Some("exposure"))
            .count(),
        rejected_redundant: frames
            .iter()
            .filter(|frame| frame.metrics.reject_reason.as_deref() == Some("redundant"))
            .count(),
        rejected_window_overflow: frames
            .iter()
            .filter(|frame| frame.metrics.reject_reason.as_deref() == Some("window_overflow"))
            .count() as u32,
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
    let frame_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    Ok(AnalyzedFrame {
        gray,
        path: path.to_path_buf(),
        metrics: FrameMetrics {
            frame_name: frame_name.clone(),
            timestamp_ms: ((frame_number(&frame_name).saturating_sub(1) as f64 * 1000.0
                / sampling_fps.max(f64::EPSILON))
            .round() as u64),
            laplacian_variance,
            overexposure_ratio,
            underexposure_ratio,
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
    let over = selected.iter().filter(|(_, &value)| value > 250).count() as f64 / total;
    let under = selected.iter().filter(|(_, &value)| value < 5).count() as f64 / total;
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

fn decide_windows(frames: &mut [AnalyzedFrame], config: &FrameFilterConfig) {
    // 下游 COLMAP 使用 sequential_matcher --SequentialMatching.overlap 10，依赖帧的时序邻域关系建立匹配图。若采用全局按质量排序取 Top-N，会抽出时间上跳跃的帧，导致匹配图断裂、mapper 分块失败，重建直接失败。
    for window in frames.chunks_mut(config.window_size) {
        let mut previous_kept: Option<GrayImage> = None;
        let mut candidates = Vec::new();
        for frame in window.iter_mut() {
            if frame.metrics.laplacian_variance < config.blur_threshold {
                frame.metrics.reject_reason = Some("blur".into());
            } else if frame.metrics.overexposure_ratio > config.overexposure_ratio
                || frame.metrics.underexposure_ratio > config.underexposure_ratio
            {
                frame.metrics.reject_reason = Some("exposure".into());
            } else {
                frame.metrics.diff_score = previous_kept.as_ref().map_or(1.0, |previous| {
                    mean_absolute_difference(previous, &frame.gray)
                });
                if candidates.is_empty() {
                    frame.metrics.kept = true;
                    previous_kept = Some(frame.gray.clone());
                    candidates.push(frame);
                } else if frame.metrics.diff_score < config.min_diff_score {
                    frame.metrics.reject_reason = Some("redundant".into());
                } else {
                    frame.metrics.kept = true;
                    previous_kept = Some(frame.gray.clone());
                    candidates.push(frame);
                }
            }
        }
        if candidates.len() > config.keep_per_window {
            // 窗口首个合格帧是时序锚点，必须保留；只在其余候选中按清晰度淘汰。
            let first_name = candidates[0].metrics.frame_name.clone();
            candidates[1..].sort_by(|left, right| {
                right
                    .metrics
                    .laplacian_variance
                    .partial_cmp(&left.metrics.laplacian_variance)
                    .unwrap_or(Ordering::Equal)
            });
            for frame in candidates
                .into_iter()
                .filter(|frame| frame.metrics.frame_name != first_name)
                .skip(config.keep_per_window.saturating_sub(1))
            {
                frame.metrics.kept = false;
                frame.metrics.reject_reason = Some("window_overflow".into());
            }
        }
        if !window.iter().any(|frame| frame.metrics.kept) {
            if let Some(frame) = window.iter_mut().max_by(|left, right| {
                left.metrics
                    .laplacian_variance
                    .partial_cmp(&right.metrics.laplacian_variance)
                    .unwrap_or(Ordering::Equal)
            }) {
                frame.metrics.kept = true;
                frame.metrics.forced_keep = true;
                frame.metrics.reject_reason = None;
            }
        }
    }
    // 第二层兜底循环回填，直到所有相邻保留帧间隔都满足窗口上限。
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
        let range = pair[0] + 1..pair[1];
        if let Some(index) = range.max_by(|left, right| {
            frames[*left]
                .metrics
                .laplacian_variance
                .partial_cmp(&frames[*right].metrics.laplacian_variance)
                .unwrap_or(Ordering::Equal)
        }) {
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

fn write_metadata(output_dir: &Path, frames: &[AnalyzedFrame]) -> Result<(), FrameFilterError> {
    let mut file = fs::File::create(output_dir.join("metadata.csv"))?;
    writeln!(file, "frame_name,timestamp_ms,laplacian_variance,overexposure_ratio,underexposure_ratio,diff_score,kept,reject_reason")?;
    for frame in frames {
        writeln!(
            file,
            "{},{},{:.6},{:.6},{:.6},{:.6},{},{}",
            csv_escape(&frame.metrics.frame_name),
            frame.metrics.timestamp_ms,
            frame.metrics.laplacian_variance,
            frame.metrics.overexposure_ratio,
            frame.metrics.underexposure_ratio,
            frame.metrics.diff_score,
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

    fn config() -> FrameFilterConfig {
        FrameFilterConfig {
            blur_threshold: 60.0,
            overexposure_ratio: 0.95,
            underexposure_ratio: 0.95,
            window_size: 10,
            keep_per_window: 3,
            analysis_max_edge: 64,
            min_diff_score: 0.01,
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
}
