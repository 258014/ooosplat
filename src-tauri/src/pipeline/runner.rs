use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use chrono::Utc;
use serde::Serialize;

const GLOBAL_MAPPER_MIN_REGISTERED_RATIO: f64 = 0.60;

fn mapper_backend_label(backend: crate::engines::MapperBackend) -> &'static str {
    match backend {
        crate::engines::MapperBackend::Global => "Global Mapper",
        crate::engines::MapperBackend::Incremental => "Incremental Mapper",
    }
}

use crate::{
    engines::{
        brush, colmap,
        ffmpeg::{extract_uniform_frames, validate_extraction},
        ffprobe::probe_video,
        EngineKind, EnginePaths,
    },
    error::{Result, SplatError},
    pipeline::{
        estimate::{
            estimate_calibrated_brush_stage_ms, estimate_calibrated_brush_stage_ms_for_images,
        },
        progress::stage_progress_range,
        EventKind, EventLevel, PipelineEngine, PipelineEvent, PipelineStage,
    },
    presets::Quality,
    process::{ProcessManager, ProcessObserver, ProcessUpdate},
    project::{
        catalog, manager::atomic_replace_file, FrameState, PipelineStateFile, ProjectInputType,
        ProjectManager, ProjectMetadata, ProjectOutput, ProjectPaths, ProjectStatus,
        ReshootProvenance,
    },
    reconstruction::{
        ply::inspect_gaussian_ply,
        validator::{ReconstructionQuality, ReconstructionReport, ReconstructionValidator},
    },
    video::{
        filter_frames_with_masks_at_fps, prepare_image_sequence, validate_prepared_image_sequence,
        FramePlan, FrameSelectionStrategy, ImageSequenceInfo, SmartFrameSelection, VideoInfo,
    },
};

pub struct PreparedFrames {
    pub input_type: ProjectInputType,
    pub video: Option<VideoInfo>,
    pub image_sequence: Option<ImageSequenceInfo>,
    pub plan: FramePlan,
    pub extracted_frames: u64,
    pub image_format: String,
    pub mask_count: u64,
    pub has_alpha: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineResult {
    pub project_id: String,
    pub project_path: PathBuf,
    pub final_ply: PathBuf,
    pub file_size: u64,
    pub splat_count: u64,
    pub input_images: u64,
    pub registered_images: u64,
    pub registered_ratio: f64,
    pub points_3d: u64,
    pub duration_ms: u64,
    pub completed_at: chrono::DateTime<Utc>,
    pub warning: Option<String>,
    pub logs_directory: PathBuf,
    #[serde(skip)]
    pub(crate) source_duration_seconds: Option<f64>,
}

#[derive(Clone)]
struct EventSink {
    emit: Arc<dyn Fn(PipelineEvent) + Send + Sync>,
    sequence: Arc<AtomicU64>,
    last_progress_milli_percent: Arc<AtomicU64>,
    last_stage: Arc<std::sync::Mutex<Option<PipelineStage>>>,
    dispatch: Arc<std::sync::Mutex<()>>,
    started: Instant,
}

impl EventSink {
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        stage: PipelineStage,
        engine: Option<PipelineEngine>,
        kind: EventKind,
        level: EventLevel,
        stage_progress: Option<f32>,
        indeterminate: bool,
        message: impl Into<String>,
        current: Option<u64>,
        total: Option<u64>,
        unit: Option<&str>,
    ) {
        if !matches!(
            stage,
            PipelineStage::Completed | PipelineStage::Failed | PipelineStage::Cancelled
        ) {
            *self
                .last_stage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stage);
        }
        let (start, end) = stage_progress_range(stage);
        let progress = stage_progress
            .map(|value| start + (end - start) * value.clamp(0.0, 1.0))
            .unwrap_or(start);
        self.last_progress_milli_percent.fetch_max(
            (progress.max(0.0) * 1_000.0).round() as u64,
            Ordering::Relaxed,
        );
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (self.emit)(PipelineEvent {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp: Utc::now(),
            kind,
            level,
            stage,
            engine,
            progress,
            stage_progress: stage_progress.map(|value| value.clamp(0.0, 1.0) * 100.0),
            indeterminate,
            message: message.into(),
            current,
            total,
            unit: unit.map(str::to_owned),
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            acceleration: None,
        });
    }

    fn stage(&self, stage: PipelineStage, progress: f32, message: impl Into<String>) {
        self.send(
            stage,
            Some(PipelineEngine::System),
            EventKind::Stage,
            EventLevel::Info,
            Some(progress),
            false,
            message,
            None,
            None,
            None,
        );
    }

    fn acceleration(&self, status: crate::engines::ColmapAccelerationStatus) {
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (self.emit)(PipelineEvent {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp: Utc::now(),
            kind: EventKind::Capability,
            level: if status.use_gpu() {
                EventLevel::Info
            } else {
                EventLevel::Warning
            },
            stage: PipelineStage::Created,
            engine: Some(PipelineEngine::Colmap),
            progress: 0.0,
            stage_progress: None,
            indeterminate: false,
            message: status.reason.clone(),
            current: None,
            total: None,
            unit: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            acceleration: Some(status),
        });
    }

    fn terminal(&self, error: &SplatError) {
        let cancelled = matches!(error, SplatError::Cancelled);
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (self.emit)(PipelineEvent {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp: Utc::now(),
            kind: EventKind::Stage,
            level: if cancelled {
                EventLevel::Warning
            } else {
                EventLevel::Error
            },
            stage: if cancelled {
                PipelineStage::Cancelled
            } else {
                PipelineStage::Failed
            },
            engine: Some(PipelineEngine::System),
            progress: self.last_progress_milli_percent.load(Ordering::Relaxed) as f32 / 1_000.0,
            stage_progress: None,
            indeterminate: false,
            message: error.to_string(),
            current: None,
            total: None,
            unit: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            acceleration: None,
        });
    }
}

#[derive(Debug, Clone)]
pub struct PipelineFailureContext {
    pub failed_stage: Option<PipelineStage>,
    pub project_id: Option<uuid::Uuid>,
    pub project_path: Option<PathBuf>,
    pub logs_directory: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct ActiveProjectContext {
    project_id: uuid::Uuid,
    project_path: PathBuf,
    logs_directory: PathBuf,
}

pub struct PipelineRunner {
    engines: EnginePaths,
    process_manager: ProcessManager,
    events: EventSink,
    active_project: Arc<std::sync::Mutex<Option<ActiveProjectContext>>>,
}

impl PipelineRunner {
    pub fn new(engines: EnginePaths, emit: impl Fn(PipelineEvent) + Send + Sync + 'static) -> Self {
        Self {
            engines,
            process_manager: ProcessManager::new(),
            events: EventSink {
                emit: Arc::new(emit),
                sequence: Arc::new(AtomicU64::new(0)),
                last_progress_milli_percent: Arc::new(AtomicU64::new(0)),
                last_stage: Arc::new(std::sync::Mutex::new(None)),
                dispatch: Arc::new(std::sync::Mutex::new(())),
                started: Instant::now(),
            },
            active_project: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn cancel(&self) {
        self.process_manager.cancel();
    }

    pub fn emit_terminal(&self, error: &SplatError) {
        self.events.terminal(error);
    }

    pub fn failure_context(&self) -> PipelineFailureContext {
        let failed_stage = *self
            .events
            .last_stage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let project = self
            .active_project
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        PipelineFailureContext {
            failed_stage,
            project_id: project.as_ref().map(|value| value.project_id),
            project_path: project.as_ref().map(|value| value.project_path.clone()),
            logs_directory: project.map(|value| value.logs_directory),
        }
    }

    pub async fn verify_pipeline_engines(
        &self,
    ) -> Result<crate::engines::ColmapAccelerationStatus> {
        let statuses = self.engines.check_all().await;
        for required in [
            EngineKind::Ffmpeg,
            EngineKind::Ffprobe,
            EngineKind::Colmap,
            EngineKind::Brush,
        ] {
            let status = statuses
                .iter()
                .find(|status| status.kind == required)
                .expect("all engine kinds returned");
            if !status.exists {
                return Err(SplatError::EngineMissing(status.path.display().to_string()));
            }
            if !status.can_start {
                return Err(SplatError::EngineStart {
                    engine: format!("{required:?}"),
                    detail: status.detail.clone(),
                });
            }
        }
        colmap::require_verified_cli(&self.engines.colmap)?;
        brush::require_verified_cli(&self.engines.brush)?;
        statuses
            .into_iter()
            .find(|status| status.kind == EngineKind::Colmap)
            .and_then(|status| status.acceleration)
            .ok_or_else(|| SplatError::UnsupportedEngine("无法确定 COLMAP 自动加速状态".into()))
    }

    pub async fn prepare_frames(
        &self,
        input: &Path,
        quality: Quality,
        output: &Path,
        masks: &Path,
        logs: Option<&Path>,
    ) -> Result<PreparedFrames> {
        self.events
            .stage(PipelineStage::ProbingVideo, 0.0, "正在读取视频信息");
        let video = probe_video(
            &self.engines.ffprobe,
            input,
            logs.map(|path| path.join("ffprobe.log")),
            &self.process_manager,
        )
        .await?;
        let probe_message = if video.has_alpha {
            format!(
                "视频 {:.1} 秒 · {:.2} FPS · {}×{} · 检测到 Alpha 通道（{}）",
                video.duration, video.fps, video.width, video.height, video.pixel_format
            )
        } else {
            format!(
                "视频 {:.1} 秒 · {:.2} FPS · {}×{}",
                video.duration, video.fps, video.width, video.height
            )
        };
        self.events
            .stage(PipelineStage::ProbingVideo, 1.0, probe_message);

        self.events
            .stage(PipelineStage::PlanningFrames, 0.0, "正在规划智能抽帧");
        let plan = SmartFrameSelection.create_plan(&video, &quality.preset());
        self.events.stage(
            PipelineStage::PlanningFrames,
            1.0,
            format!("预计提取 {} 帧", plan.estimated_frames),
        );

        self.events.stage(
            PipelineStage::ExtractingFrames,
            0.0,
            if video.has_alpha {
                "FFmpeg 正在同步提取透明 PNG 画面与 COLMAP Mask"
            } else {
                "FFmpeg 开始提取画面"
            },
        );
        let observer = self.process_observer(
            PipelineStage::ExtractingFrames,
            PipelineEngine::Ffmpeg,
            Some(plan.estimated_frames),
            ObserverMode::Ffmpeg,
        );
        let extraction = extract_uniform_frames(
            &self.engines.ffmpeg,
            input,
            output,
            masks,
            &plan,
            video.has_alpha,
            logs.map(|path| path.join("ffmpeg.log")),
            &self.process_manager,
            Some(observer),
        )
        .await?;
        self.events.stage(
            PipelineStage::ExtractingFrames,
            1.0,
            if extraction.has_alpha {
                format!(
                    "已提取 {} 张透明 PNG 和 {} 张 Mask",
                    extraction.frame_count, extraction.mask_count
                )
            } else {
                format!("已提取 {} 帧", extraction.frame_count)
            },
        );
        Ok(PreparedFrames {
            input_type: ProjectInputType::Video,
            video: Some(video),
            image_sequence: None,
            plan,
            extracted_frames: extraction.frame_count,
            image_format: extraction.image_format.as_str().into(),
            mask_count: extraction.mask_count,
            has_alpha: extraction.has_alpha,
        })
    }

    pub async fn prepare_images(
        &self,
        input: &Path,
        quality: Quality,
        output: &Path,
        masks: &Path,
    ) -> Result<PreparedFrames> {
        self.events
            .stage(PipelineStage::ProbingVideo, 0.0, "正在分析图片序列");
        let source = input.to_path_buf();
        let image_sequence =
            tokio::task::spawn_blocking(move || crate::video::analyze_image_sequence(&source))
                .await
                .map_err(|error| SplatError::Process(format!("图片序列分析任务失败：{error}")))??;
        self.events.stage(
            PipelineStage::ProbingVideo,
            1.0,
            format!(
                "图片序列 {} 张 · {}×{}{}",
                image_sequence.image_count,
                image_sequence.width,
                image_sequence.height,
                if image_sequence.has_alpha {
                    " · 检测到透明区域"
                } else {
                    ""
                }
            ),
        );
        let plan = crate::video::create_image_plan(&image_sequence, &quality.preset());
        self.events.stage(
            PipelineStage::PlanningFrames,
            1.0,
            format!("将处理全部 {} 张图片", image_sequence.image_count),
        );
        self.events.stage(
            PipelineStage::ExtractingFrames,
            0.0,
            if image_sequence.has_alpha {
                "正在准备原始图片并生成 COLMAP Alpha Mask"
            } else {
                "正在准备图片序列"
            },
        );
        let source = input.to_path_buf();
        let frames = output.to_path_buf();
        let mask_root = masks.to_path_buf();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_image_sequence(&source, &frames, &mask_root)
        })
        .await
        .map_err(|error| SplatError::Process(format!("图片序列准备任务失败：{error}")))??;
        self.events.stage(
            PipelineStage::ExtractingFrames,
            1.0,
            if prepared.has_alpha {
                format!(
                    "已准备 {} 张图片和 {} 张 Mask",
                    prepared.image_count, prepared.mask_count
                )
            } else {
                format!("已准备 {} 张图片", prepared.image_count)
            },
        );
        Ok(PreparedFrames {
            input_type: ProjectInputType::Images,
            video: None,
            image_sequence: Some(image_sequence),
            plan,
            extracted_frames: prepared.image_count,
            image_format: "images".into(),
            mask_count: prepared.mask_count,
            has_alpha: prepared.has_alpha,
        })
    }

    pub async fn generate(
        &self,
        input: &Path,
        quality: Quality,
        projects_root: &Path,
    ) -> Result<PipelineResult> {
        self.generate_with_manager(
            input,
            quality,
            ProjectManager::with_root(projects_root.to_path_buf()),
        )
        .await
    }

    pub async fn generate_for_diagnostics(
        &self,
        input: &Path,
        quality: Quality,
        projects_root: &Path,
    ) -> Result<PipelineResult> {
        self.generate_with_manager(
            input,
            quality,
            ProjectManager::for_diagnostics(projects_root.to_path_buf()),
        )
        .await
    }

    async fn generate_with_manager(
        &self,
        input: &Path,
        quality: Quality,
        project_manager: ProjectManager,
    ) -> Result<PipelineResult> {
        let acceleration = self.verify_pipeline_engines().await?;
        self.events.acceleration(acceleration.clone());
        let (paths, mut metadata) = project_manager.create(input, quality).await?;
        let state = PipelineStateFile::created_for(quality, metadata.input_type);
        self.execute_project(project_manager, paths, &mut metadata, state, &acceleration)
            .await
    }

    /// Creates a derived project from an already completed project and fresh reshoot media.
    /// The source project is read-only: its frames and final.ply are never moved or replaced.
    pub async fn generate_reshoot(
        &self,
        source_project_id: uuid::Uuid,
        reshoot_input: &Path,
        quality: Quality,
        projects_root: &Path,
        plan: ReshootPlan,
    ) -> Result<PipelineResult> {
        plan.validate()?;
        let ReshootPlan {
            regions,
            guidance,
            guidance_images,
        } = plan;
        let acceleration = self.verify_pipeline_engines().await?;
        self.events.acceleration(acceleration.clone());
        let (source_root, source_metadata) =
            catalog::load_registered_project(source_project_id).await?;
        if source_metadata.status != ProjectStatus::Completed
            || !source_root.join("final.ply").is_file()
        {
            return Err(SplatError::Process(
                "只能为已完成且包含 final.ply 的项目创建高清补拍".into(),
            ));
        }
        let source_frames = reshoot_source_frames(&source_root).await?;
        if !source_frames.is_dir() {
            return Err(SplatError::Process(
                "原项目缺少可复用的输入画面，无法融合补拍素材".into(),
            ));
        }
        let source_masks = source_root.join("work").join("masks");
        if source_masks.is_dir()
            && tokio::fs::read_dir(&source_masks)
                .await?
                .next_entry()
                .await?
                .is_some()
        {
            return Err(SplatError::Process(
                "原项目含透明 Mask，当前高清补拍不支持混合透明素材".into(),
            ));
        }

        let project_manager = ProjectManager::with_root(projects_root.to_path_buf());
        let (paths, mut metadata) = project_manager.create(reshoot_input, quality).await?;
        let stored_reshoot_source = metadata.source_path.clone();
        let temporary = paths.work.join("reshoot-frames");
        let temporary_masks = paths.work.join("reshoot-masks");
        let prepared_reshoot = if reshoot_input.is_dir() {
            self.prepare_images(reshoot_input, quality, &temporary, &temporary_masks)
                .await?
        } else {
            self.prepare_frames(
                reshoot_input,
                quality,
                &temporary,
                &temporary_masks,
                Some(&paths.logs),
            )
            .await?
        };
        if prepared_reshoot.has_alpha {
            return Err(SplatError::Process(
                "高清补拍暂不支持透明素材；请导出不含 Alpha 的 JPG/PNG 或 MP4/MOV".into(),
            ));
        }
        reset_directory(&paths.frames).await?;
        copy_merged_frames(&source_frames, &temporary, &paths.frames).await?;
        let original_frame_count = count_image_files(&source_frames).await?;
        let merged_count = original_frame_count + prepared_reshoot.extracted_frames;
        metadata.name = format!("{}_高清补拍", source_metadata.name);
        // Keep the copied reshoot input as the project source. The merged frames are a
        // checkpoint; if it is lost, resume can reconstruct it from provenance.
        metadata.source_path = stored_reshoot_source.clone();
        metadata.input_type = ProjectInputType::Images;
        let guidance_images =
            write_reshoot_guidance_images(&paths.project, &guidance_images).await?;
        metadata.reshoot = Some(ReshootProvenance {
            source_project_id,
            source_project_path: source_root.clone(),
            source_final_ply: source_root.join("final.ply"),
            reshoot_source_path: stored_reshoot_source,
            regions,
            guidance,
            guidance_images,
            original_frame_count,
            reshoot_frame_count: prepared_reshoot.extracted_frames,
        });
        project_manager
            .write_metadata(&paths.metadata, &metadata)
            .await?;
        let mut state = PipelineStateFile::created_for(quality, ProjectInputType::Images);
        state.stage = PipelineStage::ExtractingFrames;
        state.image_sequence = Some(ImageSequenceInfo {
            image_count: merged_count,
            width: 0,
            height: 0,
            has_alpha: false,
            requires_large_sequence_confirmation: merged_count
                > crate::video::LARGE_SEQUENCE_WARNING_COUNT,
        });
        state.frames = Some(FrameState {
            retention_ratio: 1.0,
            sampling_fps: 0.0,
            estimated_frames: merged_count,
            extracted_frames: Some(merged_count),
            image_format: Some("merged".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        project_manager.write_state(&paths.state, &state).await?;
        self.execute_project(project_manager, paths, &mut metadata, state, &acceleration)
            .await
    }

    pub async fn resume(&self, project_id: uuid::Uuid) -> Result<PipelineResult> {
        let acceleration = self.verify_pipeline_engines().await?;
        self.events.acceleration(acceleration.clone());
        let (project, mut metadata) = catalog::load_registered_project(project_id).await?;
        if catalog::project_is_durably_completed(&project, &metadata).await {
            return Err(SplatError::Process("该项目已经完成，无需继续".into()));
        }
        let source_available = if metadata.reshoot.is_some() {
            // A derived reshoot can retain either a video file or an image folder.
            metadata.source_path.exists()
        } else {
            match metadata.input_type {
                ProjectInputType::Video => metadata.source_path.is_file(),
                ProjectInputType::Images => metadata.source_path.is_dir(),
            }
        };
        if !source_available {
            return Err(SplatError::Process("项目源素材缺失，无法继续".into()));
        }
        let paths = ProjectPaths::existing(project_id, project.clone());
        let project_manager = ProjectManager::with_root(
            project
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| project.clone()),
        );
        let state = project_manager.read_state(&paths.state).await?;
        if state.preset != metadata.quality {
            return Err(SplatError::Process(
                "项目档位与检查点不一致，无法安全继续".into(),
            ));
        }
        self.execute_project(project_manager, paths, &mut metadata, state, &acceleration)
            .await
    }

    async fn execute_project(
        &self,
        project_manager: ProjectManager,
        paths: ProjectPaths,
        metadata: &mut ProjectMetadata,
        state: PipelineStateFile,
        acceleration: &crate::engines::ColmapAccelerationStatus,
    ) -> Result<PipelineResult> {
        *self
            .active_project
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ActiveProjectContext {
            project_id: paths.id,
            project_path: paths.project.clone(),
            logs_directory: paths.logs.clone(),
        });
        let started = Instant::now();
        let previous_duration = metadata.duration_ms.unwrap_or(0);
        metadata.status = ProjectStatus::Running;
        metadata.started_at = Some(Utc::now());
        metadata.completed_at = None;
        metadata.failure_message = None;
        project_manager
            .write_metadata(&paths.metadata, metadata)
            .await?;
        let result = self
            .run_project(&project_manager, &paths, metadata, state, acceleration)
            .await;

        if let Err(error) = &result {
            let cancelled = matches!(error, SplatError::Cancelled);
            metadata.status = if cancelled {
                ProjectStatus::Cancelled
            } else {
                ProjectStatus::Failed
            };
            metadata.completed_at = Some(Utc::now());
            metadata.duration_ms =
                Some(previous_duration.saturating_add(started.elapsed().as_millis() as u64));
            metadata.failure_message = Some(error.to_string());
            let _ = project_manager
                .write_metadata(&paths.metadata, metadata)
                .await;
            let mut state = project_manager
                .read_state(&paths.state)
                .await
                .unwrap_or_else(|_| {
                    PipelineStateFile::created_for(metadata.quality, metadata.input_type)
                });
            state = mark_state_terminal(state, cancelled);
            let _ = project_manager.write_state(&paths.state, &state).await;
        }
        result
    }

    async fn run_project(
        &self,
        project_manager: &ProjectManager,
        paths: &ProjectPaths,
        metadata: &mut ProjectMetadata,
        mut state: PipelineStateFile,
        acceleration: &crate::engines::ColmapAccelerationStatus,
    ) -> Result<PipelineResult> {
        let quality = metadata.quality;
        if state.input_type != metadata.input_type {
            return Err(SplatError::Process(
                "项目输入类型与检查点不一致，无法安全继续".into(),
            ));
        }
        recover_interrupted_publish(paths, &state).await?;
        normalize_checkpoints(paths, &mut state).await?;
        project_manager.write_state(&paths.state, &state).await?;
        let prepared = if let Some(prepared) =
            prepared_frames_from_checkpoint(paths, &state).await?
        {
            self.events.stage(
                PipelineStage::ExtractingFrames,
                1.0,
                format!("已复用 {} 帧检查点", prepared.extracted_frames),
            );
            prepared
        } else {
            reset_directory(&paths.colmap).await?;
            reset_directory(&paths.brush).await?;
            let prepared = if let Some(provenance) = metadata.reshoot.as_ref() {
                // The same frame set the first attempt merged, so a resumed
                // reshoot keeps the same matching cost instead of silently
                // falling back to the larger raw set.
                let source_frames = reshoot_source_frames(&provenance.source_project_path).await?;
                if !source_frames.is_dir() {
                    return Err(SplatError::Process(
                        "原项目输入画面已缺失，无法继续高清补拍".into(),
                    ));
                }
                reset_directory(&paths.frames).await?;
                reset_directory(&paths.masks).await?;
                let reshoot_frames = paths.work.join("reshoot-recovery-frames");
                let reshoot_masks = paths.work.join("reshoot-recovery-masks");
                reset_directory(&reshoot_frames).await?;
                reset_directory(&reshoot_masks).await?;
                let recovered_reshoot = if metadata.source_path.is_dir() {
                    self.prepare_images(
                        &metadata.source_path,
                        quality,
                        &reshoot_frames,
                        &reshoot_masks,
                    )
                    .await?
                } else {
                    self.prepare_frames(
                        &metadata.source_path,
                        quality,
                        &reshoot_frames,
                        &reshoot_masks,
                        Some(&paths.logs),
                    )
                    .await?
                };
                if recovered_reshoot.has_alpha {
                    return Err(SplatError::Process("高清补拍恢复不支持透明素材".into()));
                }
                copy_merged_frames(&source_frames, &reshoot_frames, &paths.frames).await?;
                let extracted_frames = count_image_files(&paths.frames).await?;
                PreparedFrames {
                    input_type: ProjectInputType::Images,
                    video: None,
                    image_sequence: Some(ImageSequenceInfo {
                        image_count: extracted_frames,
                        width: 0,
                        height: 0,
                        has_alpha: false,
                        requires_large_sequence_confirmation: false,
                    }),
                    plan: FramePlan {
                        retention_ratio: 1.0,
                        sampling_fps: 0.0,
                        estimated_frames: extracted_frames,
                    },
                    extracted_frames,
                    image_format: "merged".into(),
                    mask_count: 0,
                    has_alpha: false,
                }
            } else {
                reset_directory(&paths.frames).await?;
                reset_directory(&paths.masks).await?;
                match metadata.input_type {
                    ProjectInputType::Video => {
                        self.prepare_frames(
                            &metadata.source_path,
                            quality,
                            &paths.frames,
                            &paths.masks,
                            Some(&paths.logs),
                        )
                        .await?
                    }
                    ProjectInputType::Images => {
                        self.prepare_images(
                            &metadata.source_path,
                            quality,
                            &paths.frames,
                            &paths.masks,
                        )
                        .await?
                    }
                }
            };
            state.input_type = prepared.input_type;
            state.video = prepared.video.clone();
            state.image_sequence = prepared.image_sequence.clone();
            let mut frames = FrameState::from(&prepared.plan);
            frames.extracted_frames = Some(prepared.extracted_frames);
            frames.image_format = Some(prepared.image_format.clone());
            frames.mask_count = Some(prepared.mask_count);
            frames.has_alpha = prepared.has_alpha;
            state.frames = Some(frames);
            state.features_complete = false;
            state.matching_complete = false;
            state.reconstruction_complete = false;
            state.brush_complete = false;
            state.stage = PipelineStage::ExtractingFrames;
            project_manager.write_state(&paths.state, &state).await?;
            prepared
        };
        let source_duration_seconds = prepared.video.as_ref().map(|video| video.duration);
        let filter_result = ensure_filter_checkpoint(paths, &mut state, &prepared).await?;
        project_manager.write_state(&paths.state, &state).await?;
        if let Some(result) = filter_result {
            self.events.stage(
                PipelineStage::ExtractingFrames,
                1.0,
                format!(
                    "已智能筛选 {} / {} 帧（模糊 {} · 曝光 {} · 冗余 {} · 兜底 {}）",
                    result.kept_frames,
                    result.total_frames,
                    result.rejected_blur,
                    result.rejected_exposure,
                    result.rejected_redundant,
                    result.forced_keeps
                ),
            );
            if result.forced_keeps * 10 > result.total_frames {
                self.events.send(
                    PipelineStage::ExtractingFrames,
                    Some(PipelineEngine::System),
                    EventKind::Log,
                    EventLevel::Warning,
                    Some(1.0),
                    false,
                    "智能筛选兜底帧占比超过 10%，请检查素材质量或筛选参数",
                    Some(result.forced_keeps as u64),
                    Some(result.total_frames as u64),
                    Some("frames"),
                );
            }
            project_manager.write_state(&paths.state, &state).await?;
        }
        let (frame_input, _) = pipeline_frame_inputs(paths, &state);
        let database = paths.colmap.join("database.db");
        let sparse = paths.colmap.join("sparse");
        let colmap_log = paths.logs.join("colmap.log");
        // COLMAP's bundled bitmap loader cannot reliably open non-ASCII absolute
        // paths on Windows. The process working directory is work/colmap, so this
        // ASCII-only relative path preserves Unicode/UNC project roots without
        // moving any project data outside the project directory.
        let (colmap_images, colmap_mask_root) = colmap_input_paths(&state);
        let colmap_masks = prepared.has_alpha.then_some(colmap_mask_root);

        let backend_label = if acceleration.use_gpu() { "GPU" } else { "CPU" };
        let gpu_index = acceleration.gpu_index();
        let preset = quality.preset();
        let extraction_tuning = colmap::FeatureExtractionTuning {
            max_image_size: preset.feature_max_image_size,
            max_num_features: preset.feature_max_num_features,
        };
        let matching_tuning = colmap::SequentialMatchingTuning {
            overlap: preset.sequential_overlap,
        };
        if state.features_complete {
            self.events.stage(
                PipelineStage::ExtractingFeatures,
                1.0,
                "已复用特征提取检查点",
            );
        } else {
            reset_directory(&paths.colmap).await?;
            self.events.stage(
                PipelineStage::ExtractingFeatures,
                0.0,
                format!(
                    "COLMAP 正在使用 {backend_label} 提取特征（max_image_size={}, max_num_features={}）",
                    extraction_tuning.max_image_size, extraction_tuning.max_num_features
                ),
            );
            colmap::extract_features(
                &self.engines.colmap,
                &database,
                colmap_images,
                colmap_masks,
                colmap_log.clone(),
                &self.process_manager,
                Some(
                    self.process_observer(
                        PipelineStage::ExtractingFeatures,
                        PipelineEngine::Colmap,
                        Some(
                            state
                                .frames
                                .as_ref()
                                .and_then(|frames| frames.filtered_frames)
                                .unwrap_or(prepared.extracted_frames),
                        ),
                        ObserverMode::BracketProgress,
                    ),
                ),
                gpu_index,
                extraction_tuning,
            )
            .await?;
            state.stage = PipelineStage::ExtractingFeatures;
            state.features_complete = true;
            project_manager.write_state(&paths.state, &state).await?;
            self.events.stage(
                PipelineStage::ExtractingFeatures,
                1.0,
                format!("{backend_label} 特征提取完成"),
            );
        }

        if state.matching_complete {
            self.events.stage(
                PipelineStage::Matching,
                1.0,
                if prepared.input_type == ProjectInputType::Images {
                    "已复用穷举匹配检查点"
                } else {
                    "已复用顺序匹配检查点"
                },
            );
        } else {
            self.events.stage(
                PipelineStage::Matching,
                0.0,
                if prepared.input_type == ProjectInputType::Images {
                    format!("COLMAP 正在进行 {backend_label} 穷举匹配")
                } else {
                    format!(
                        "COLMAP 正在进行 {backend_label} 顺序匹配（overlap={}, quadratic_overlap=1）",
                        matching_tuning.overlap
                    )
                },
            );
            let observer = Some(
                self.process_observer(
                    PipelineStage::Matching,
                    PipelineEngine::Colmap,
                    Some(
                        state
                            .frames
                            .as_ref()
                            .and_then(|frames| frames.filtered_frames)
                            .unwrap_or(prepared.extracted_frames),
                    ),
                    ObserverMode::BracketProgress,
                ),
            );
            if prepared.input_type == ProjectInputType::Images {
                colmap::match_exhaustive(
                    &self.engines.colmap,
                    &database,
                    colmap_log.clone(),
                    &self.process_manager,
                    observer,
                    gpu_index,
                )
                .await?;
            } else {
                colmap::match_sequential(
                    &self.engines.colmap,
                    &database,
                    colmap_log.clone(),
                    &self.process_manager,
                    observer,
                    gpu_index,
                    matching_tuning,
                )
                .await?;
            }
            state.stage = PipelineStage::Matching;
            state.matching_complete = true;
            project_manager.write_state(&paths.state, &state).await?;
            self.events.stage(
                PipelineStage::Matching,
                1.0,
                if prepared.input_type == ProjectInputType::Images {
                    "穷举匹配完成"
                } else {
                    "顺序匹配完成"
                },
            );
        }

        if state.reconstruction_complete {
            self.events
                .stage(PipelineStage::Reconstructing, 1.0, "已复用相机重建检查点");
        } else {
            let frame_count = state
                .frames
                .as_ref()
                .and_then(|frames| frames.filtered_frames)
                .unwrap_or(prepared.extracted_frames);
            let preference = quality.preset().mapper_backend;
            let global_available =
                colmap::supports_global_mapper(&self.engines.colmap, &self.process_manager).await;
            let preferred_backend = preference.backend(global_available);
            let mut selected_backend = preferred_backend;
            let started = Instant::now();
            reset_directory(&sparse).await?;
            self.events.stage(
                PipelineStage::Reconstructing,
                0.0,
                format!(
                    "正在使用 {} 重建相机轨迹",
                    mapper_backend_label(preferred_backend)
                ),
            );
            if preferred_backend == colmap::MapperBackend::Global {
                // The global mapper needs focal-length priors. FFmpeg-extracted
                // frames carry no EXIF, so the cameras start without priors and
                // COLMAP itself recommends calibrating the view graph first. The
                // step is best-effort: without it the mapper still runs, just with
                // the warning this prevents.
                let calibrated =
                    colmap::cli_capabilities(&self.engines.colmap, &self.process_manager)
                        .await
                        .map(|capabilities| capabilities.has_view_graph_calibrator())
                        .unwrap_or(false);
                if needs_view_graph_calibration(preferred_backend, calibrated) {
                    match colmap::calibrate_view_graph(
                        &self.engines.colmap,
                        &database,
                        paths.logs.join("colmap_view_graph.log"),
                        &self.process_manager,
                        None,
                    )
                    .await
                    {
                        Ok(()) => self.events.stage(
                            PipelineStage::Reconstructing,
                            0.05,
                            "已完成视图图标定，全局重建获得焦距先验",
                        ),
                        Err(error) => self.events.send(
                            PipelineStage::Reconstructing,
                            Some(PipelineEngine::Colmap),
                            EventKind::Log,
                            EventLevel::Warning,
                            Some(0.05),
                            false,
                            format!("视图图标定未完成，继续以原参数重建：{error}"),
                            None,
                            None,
                            None,
                        ),
                    }
                }
                let global_result = colmap::map_with_backend(
                    colmap::MapperBackend::Global,
                    &self.engines.colmap,
                    &database,
                    colmap_images,
                    &sparse,
                    colmap_log.clone(),
                    &self.process_manager,
                    Some(self.process_observer(
                        PipelineStage::Reconstructing,
                        PipelineEngine::Colmap,
                        Some(frame_count),
                        ObserverMode::Mapper,
                    )),
                )
                .await;
                let global_succeeded = global_result.is_ok();
                let global_quality = if global_result.is_ok() {
                    best_sparse_model(frame_input, &sparse).await.ok()
                } else {
                    None
                }
                .filter(|(_, report)| {
                    report.registered_ratio >= GLOBAL_MAPPER_MIN_REGISTERED_RATIO
                });
                if global_quality.is_none() {
                    let reason = if !global_succeeded {
                        "global_mapper 执行失败"
                    } else {
                        "global_mapper 输出无效或注册率低于 60%"
                    };
                    self.events.send(
                        PipelineStage::Reconstructing,
                        Some(PipelineEngine::Colmap),
                        EventKind::Log,
                        EventLevel::Warning,
                        Some(0.2),
                        false,
                        format!("Global Mapper 未达到要求，回退 Incremental：{reason}"),
                        None,
                        None,
                        None,
                    );
                    reset_directory(&sparse).await?;
                    selected_backend = colmap::MapperBackend::Incremental;
                    colmap::map_with_backend(
                        selected_backend,
                        &self.engines.colmap,
                        &database,
                        colmap_images,
                        &sparse,
                        colmap_log.clone(),
                        &self.process_manager,
                        Some(self.process_observer(
                            PipelineStage::Reconstructing,
                            PipelineEngine::Colmap,
                            Some(frame_count),
                            ObserverMode::Mapper,
                        )),
                    )
                    .await?;
                }
            } else {
                colmap::map_with_backend(
                    selected_backend,
                    &self.engines.colmap,
                    &database,
                    colmap_images,
                    &sparse,
                    colmap_log.clone(),
                    &self.process_manager,
                    Some(self.process_observer(
                        PipelineStage::Reconstructing,
                        PipelineEngine::Colmap,
                        Some(frame_count),
                        ObserverMode::Mapper,
                    )),
                )
                .await?;
            }
            let (_, final_report) = best_sparse_model(frame_input, &sparse).await?;
            ensure_trainable_reconstruction(&final_report)?;
            state.stage = PipelineStage::Reconstructing;
            state.reconstruction_complete = true;
            state.mapper_backend = Some(selected_backend);
            project_manager.write_state(&paths.state, &state).await?;
            self.events.stage(
                PipelineStage::Reconstructing,
                1.0,
                format!(
                    "{} 重建完成，耗时 {} 秒，注册率 {:.1}%",
                    mapper_backend_label(selected_backend),
                    started.elapsed().as_secs(),
                    final_report.registered_ratio * 100.0
                ),
            );
        }

        self.events.stage(
            PipelineStage::ValidatingReconstruction,
            0.0,
            "正在核验注册率和三维点",
        );
        let (model, report) = best_sparse_model(frame_input, &sparse).await?;
        let warning = (report.quality == ReconstructionQuality::Warning).then(|| {
            format!(
                "注册率 {:.1}%：低于 80%，将继续训练，但结果质量可能受影响",
                report.registered_ratio * 100.0
            )
        });
        self.events.stage(
            PipelineStage::ValidatingReconstruction,
            1.0,
            format!(
                "注册 {}/{} 张 · 三维点 {}",
                report.registered_images, report.input_images, report.points_3d
            ),
        );

        let preset = quality.preset();
        let candidate = if state.brush_complete {
            self.events.stage(
                PipelineStage::TrainingSplats,
                1.0,
                "已复用 Brush 训练检查点",
            );
            brush_candidate(&paths.brush)
                .ok_or_else(|| SplatError::Process("Brush 检查点文件缺失，无法继续发布".into()))?
        } else {
            reset_directory(&paths.brush).await?;
            let dataset = prepare_brush_dataset(&paths.brush, frame_input, &model).await?;
            let runtime_samples = catalog::runtime_samples().await;
            let mapper_backend = state
                .mapper_backend
                .unwrap_or_else(|| quality.preset().mapper_backend.backend(true));
            let estimated_brush_duration_ms = match (&prepared.video, &prepared.image_sequence) {
                (Some(video), _) => estimate_calibrated_brush_stage_ms(
                    video,
                    &prepared.plan,
                    quality,
                    &runtime_samples,
                    mapper_backend,
                ),
                (_, Some(images)) => estimate_calibrated_brush_stage_ms_for_images(
                    images.image_count,
                    &prepared.plan,
                    quality,
                    &runtime_samples,
                    mapper_backend,
                ),
                _ => return Err(SplatError::Process("项目输入信息不完整".into())),
            };
            self.events.send(
                PipelineStage::TrainingSplats,
                Some(PipelineEngine::Brush),
                EventKind::Stage,
                EventLevel::Info,
                None,
                true,
                format!(
                    "Brush 训练开始（使用可用图形后端）· {} iterations · 最大分辨率 {}{} · 预计约 {}",
                    preset.brush_iterations,
                    preset.brush_max_resolution,
                    brush_tuning_note(preset.brush_tuning),
                    format_duration(estimated_brush_duration_ms)
                ),
                Some(0),
                Some(preset.brush_iterations as u64),
                Some("iterations"),
            );
            let candidate = brush::train(
                &self.engines.brush,
                &dataset,
                &paths.brush,
                preset,
                paths.logs.join("brush.log"),
                &self.process_manager,
                Some(self.process_observer(
                    PipelineStage::TrainingSplats,
                    PipelineEngine::Brush,
                    Some(preset.brush_iterations as u64),
                    ObserverMode::Brush {
                        estimated_duration_ms: estimated_brush_duration_ms,
                    },
                )),
            )
            .await?;
            state.stage = PipelineStage::TrainingSplats;
            state.brush_complete = true;
            project_manager.write_state(&paths.state, &state).await?;
            self.events
                .stage(PipelineStage::TrainingSplats, 1.0, "Brush 训练完成");
            candidate
        };

        self.events
            .stage(PipelineStage::Exporting, 0.0, "正在校验并发布 final.ply");
        let ply = inspect_gaussian_ply(&candidate)?;
        let final_ply = paths.project.join("final.ply");
        atomic_replace_file(&candidate, &final_ply).await?;
        state.stage = PipelineStage::Completed;
        project_manager.write_state(&paths.state, &state).await?;

        let completed_at = Utc::now();
        let duration_ms = metadata.duration_ms.unwrap_or(0).saturating_add(
            metadata
                .started_at
                .map(|started| (completed_at - started).num_milliseconds().max(0) as u64)
                .unwrap_or(0),
        );
        metadata.status = ProjectStatus::Completed;
        metadata.completed_at = Some(completed_at);
        metadata.duration_ms = Some(duration_ms);
        metadata.output = Some(ProjectOutput {
            final_ply: final_ply.clone(),
            file_size: ply.file_size,
            splat_count: ply.splat_count,
            input_images: report.input_images,
            registered_images: report.registered_images,
            registered_ratio: report.registered_ratio,
            points_3d: report.points_3d,
        });
        project_manager
            .write_metadata(&paths.metadata, metadata)
            .await?;

        self.events.stage(
            PipelineStage::Exporting,
            1.0,
            format!("已发布 {} 个 Splat", ply.splat_count),
        );
        self.events
            .stage(PipelineStage::Completed, 1.0, "全部处理完成");
        Ok(PipelineResult {
            project_id: paths.id.to_string(),
            project_path: paths.project.clone(),
            final_ply,
            file_size: ply.file_size,
            splat_count: ply.splat_count,
            input_images: report.input_images,
            registered_images: report.registered_images,
            registered_ratio: report.registered_ratio,
            points_3d: report.points_3d,
            duration_ms,
            completed_at,
            warning,
            logs_directory: paths.logs.clone(),
            source_duration_seconds,
        })
    }

    fn process_observer(
        &self,
        stage: PipelineStage,
        engine: PipelineEngine,
        expected_total: Option<u64>,
        mode: ObserverMode,
    ) -> ProcessObserver {
        let events = self.events.clone();
        let mapper_count = Arc::new(AtomicU64::new(0));
        let brush_progress_basis_points = Arc::new(AtomicU64::new(0));
        Arc::new(move |update| match update {
            ProcessUpdate::Started { process_id } => events.send(
                stage,
                Some(engine),
                EventKind::Log,
                EventLevel::Info,
                None,
                true,
                format!("进程已启动 · PID {process_id}"),
                None,
                expected_total,
                None,
            ),
            ProcessUpdate::Heartbeat { elapsed_ms } => {
                if let ObserverMode::Brush {
                    estimated_duration_ms,
                } = mode
                {
                    let progress = estimated_brush_progress(elapsed_ms, estimated_duration_ms);
                    brush_progress_basis_points
                        .store((progress * 10_000.0).round() as u64, Ordering::Relaxed);
                    events.send(
                        stage,
                        Some(engine),
                        EventKind::Heartbeat,
                        EventLevel::Info,
                        Some(progress),
                        false,
                        brush_progress_message(elapsed_ms, estimated_duration_ms, progress),
                        None,
                        expected_total,
                        Some("estimated_progress"),
                    );
                }
            }
            ProcessUpdate::Line { stream: _, line } => {
                if line.is_empty() {
                    return;
                }
                let parsed = match mode {
                    ObserverMode::Ffmpeg => {
                        parse_ffmpeg_frame(&line).map(|current| MapperProgress::Registered {
                            current,
                            total: expected_total,
                            message: format!("FFmpeg 已输出 {current} 帧"),
                        })
                    }
                    ObserverMode::BracketProgress => {
                        parse_bracket_progress(&line).map(|(current, total)| {
                            MapperProgress::Registered {
                                current,
                                total: Some(total),
                                message: friendly_engine_line(&line),
                            }
                        })
                    }
                    ObserverMode::Mapper => {
                        parse_mapper_progress(&line, &mapper_count, expected_total)
                    }
                    ObserverMode::Brush { .. } => None,
                };
                if let Some(parsed) = parsed {
                    let (progress, message, current, total, unit) = match parsed {
                        MapperProgress::Registered {
                            current,
                            total,
                            message,
                        } => (
                            total
                                .filter(|value| *value > 0)
                                .map(|value| current as f32 / value as f32),
                            message,
                            Some(current),
                            total,
                            Some("张"),
                        ),
                        MapperProgress::Stage { fraction, message } => (
                            Some(fraction.clamp(0.0, 1.0)),
                            message,
                            None,
                            expected_total,
                            None,
                        ),
                    };
                    events.send(
                        stage,
                        Some(engine),
                        EventKind::Progress,
                        EventLevel::Info,
                        progress,
                        progress.is_none(),
                        message,
                        current,
                        total,
                        unit,
                    );
                } else if matches!(mode, ObserverMode::Brush { .. }) {
                    let progress =
                        brush_progress_basis_points.load(Ordering::Relaxed) as f32 / 10_000.0;
                    events.send(
                        stage,
                        Some(engine),
                        EventKind::Log,
                        EventLevel::Info,
                        Some(progress),
                        progress == 0.0,
                        friendly_engine_line(&line),
                        None,
                        expected_total,
                        Some("estimated_progress"),
                    );
                } else if is_useful_line(&line) {
                    events.send(
                        stage,
                        Some(engine),
                        EventKind::Log,
                        EventLevel::Info,
                        None,
                        true,
                        friendly_engine_line(&line),
                        None,
                        expected_total,
                        None,
                    );
                }
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ObserverMode {
    Ffmpeg,
    BracketProgress,
    Mapper,
    Brush { estimated_duration_ms: u64 },
}

/// Rejects a reconstruction that is too degenerate to train on.
///
/// COLMAP reports "nothing could be reconstructed" as a successful exit, and a
/// failed run still leaves a structurally valid sparse directory behind, so the
/// model itself has to decide whether the run is usable. Without this gate the
/// pipeline continues into Brush training on a handful of registered images and
/// spends minutes producing noise from an empty point cloud.
///
/// The bar is the same ratio that already triggers the backend fallback, so a
/// run only fails when neither backend reached the quality the pipeline demands.
fn ensure_trainable_reconstruction(report: &ReconstructionReport) -> Result<()> {
    if report.registered_ratio >= GLOBAL_MAPPER_MIN_REGISTERED_RATIO {
        return Ok(());
    }
    Err(SplatError::Process(format!(
        "重建注册率仅 {:.1}%（{} / {} 张图像），低于 {}% 的可训练下限：两个重建后端都未达标。\
请检查素材清晰度与重叠度，或改用更充分的匹配参数后重试。",
        report.registered_ratio * 100.0,
        report.registered_images,
        report.input_images,
        (GLOBAL_MAPPER_MIN_REGISTERED_RATIO * 100.0).round(),
    )))
}

fn estimated_brush_progress(elapsed_ms: u64, estimated_duration_ms: u64) -> f32 {
    const MAX_PROGRESS_BEFORE_COMPLETION: f64 = 0.95;
    if estimated_duration_ms == 0 {
        return 0.0;
    }
    ((elapsed_ms as f64 / estimated_duration_ms as f64) * MAX_PROGRESS_BEFORE_COMPLETION)
        .clamp(0.0, MAX_PROGRESS_BEFORE_COMPLETION) as f32
}

/// Brush reports no progress of its own, so the bar can only follow a duration
/// estimate and must stop short of 100% until the process exits. Once the run
/// passes that estimate the bar stops moving, which reads as a hang — say so
/// explicitly instead of leaving the user staring at a frozen 95%.
fn brush_progress_message(elapsed_ms: u64, estimated_duration_ms: u64, progress: f32) -> String {
    if estimated_duration_ms > 0 && elapsed_ms > estimated_duration_ms {
        return format!(
            "Brush 训练中 · 已超过预估时间 · 已用时 {}（训练仍在继续，可查看日志确认）",
            format_duration(elapsed_ms)
        );
    }
    format!(
        "Brush 训练中 · 估算进度 {:.0}% · 已用时 {}",
        progress * 100.0,
        format_duration(elapsed_ms)
    )
}

fn parse_ffmpeg_frame(line: &str) -> Option<u64> {
    line.strip_prefix("frame=")?.trim().parse().ok()
}

fn parse_bracket_progress(line: &str) -> Option<(u64, u64)> {
    let open = line.find('[')?;
    let close = line[open + 1..].find(']')? + open + 1;
    let value = &line[open + 1..close];
    let (current, total) = value.split_once('/')?;
    Some((current.trim().parse().ok()?, total.trim().parse().ok()?))
}

/// Progress fractions for the global mapper's published stages.
///
/// The global pipeline never prints a registration counter, so progress is
/// derived from the stage headings it does print. The fractions reflect where the
/// time goes on real footage: on this hardware the positioning solve dominates,
/// while rotation averaging and track establishment are near-instant.
const GLOBAL_MAPPER_STAGES: &[(&str, f32, &str)] = &[
    (
        "=== Running rotation averaging ===",
        0.05,
        "全局重建：旋转平均",
    ),
    (
        "=== Running track establishment ===",
        0.15,
        "全局重建：建立轨迹",
    ),
    (
        "=== Running global positioning ===",
        0.30,
        "全局重建：全局定位（最耗时）",
    ),
    (
        "=== Running iterative bundle adjustment ===",
        0.65,
        "全局重建：整体光束法平差",
    ),
    (
        "=== Running iterative retriangulation and refinement ===",
        0.80,
        "全局重建：重三角化与精化",
    ),
];

/// One parsed mapper progress signal.
///
/// The two backends report progress in fundamentally different ways, so the two
/// shapes are kept apart instead of being flattened into a count pair.
enum MapperProgress {
    /// The incremental mapper registers images one by one.
    Registered {
        current: u64,
        total: Option<u64>,
        message: String,
    },
    /// The global mapper only announces which stage it entered.
    Stage { fraction: f32, message: String },
}

fn parse_mapper_progress(
    line: &str,
    counter: &AtomicU64,
    expected_total: Option<u64>,
) -> Option<MapperProgress> {
    // The incremental mapper never announces its stages, and the global mapper
    // announces stage banners instead of counting registered images, so both
    // shapes have to be understood here. Stage banners win because they are the
    // only signal the global mapper produces.
    if let Some((_, fraction, label)) = GLOBAL_MAPPER_STAGES
        .iter()
        .find(|(marker, _, _)| line.contains(marker))
    {
        return Some(MapperProgress::Stage {
            fraction: *fraction,
            message: (*label).to_owned(),
        });
    }
    let reported_count = value_after(line, "num_reg_frames=")
        .or_else(|| value_after(line, "num_reg_frames ="))
        .and_then(|value| value.parse::<u64>().ok());
    let lower = line.to_ascii_lowercase();
    if lower.contains("retriangulation") || lower.contains("global bundle adjustment") {
        if let Some(current) = reported_count {
            counter.fetch_max(current, Ordering::Relaxed);
        }
        let current = counter.load(Ordering::Relaxed);
        if current > 0 {
            return Some(MapperProgress::Registered {
                current,
                total: expected_total,
                message: friendly_engine_line(line),
            });
        }
        return None;
    }
    if let Some(current) = reported_count {
        counter.fetch_max(current, Ordering::Relaxed);
        let current = counter.load(Ordering::Relaxed);
        return Some(MapperProgress::Registered {
            current,
            total: expected_total,
            message: format!("已注册 {current} 张图像"),
        });
    }
    if line.contains("Registering image #") {
        let current = counter.fetch_add(1, Ordering::Relaxed) + 1;
        return Some(MapperProgress::Registered {
            current,
            total: expected_total,
            message: format!("正在注册第 {current} 张图像"),
        });
    }
    None
}

fn value_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let value = line.split_once(marker)?.1;
    Some(
        value
            .split_whitespace()
            .next()?
            .trim_matches(|ch: char| !ch.is_ascii_digit()),
    )
}

fn is_useful_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "bundle", "register", "triang", "elapsed", "warning", "error", "writing", "loading",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn friendly_engine_line(line: &str) -> String {
    const MAX: usize = 360;
    let mut value = line.trim().to_string();
    if value.chars().count() > MAX {
        value = value.chars().take(MAX).collect::<String>() + "…";
    }
    value
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

fn checkpoint_stage(state: &PipelineStateFile) -> PipelineStage {
    if state.brush_complete {
        PipelineStage::TrainingSplats
    } else if state.reconstruction_complete {
        PipelineStage::Reconstructing
    } else if state.matching_complete {
        PipelineStage::Matching
    } else if state.features_complete {
        PipelineStage::ExtractingFeatures
    } else if state
        .frames
        .as_ref()
        .and_then(|frames| frames.extracted_frames)
        .is_some_and(|count| count > 0)
    {
        PipelineStage::ExtractingFrames
    } else {
        PipelineStage::Created
    }
}

/// Everything the user selected in the preview for one reshoot run.
#[derive(Debug, Clone, Default)]
pub struct ReshootPlan {
    pub regions: Vec<crate::project::GaussianCrop>,
    pub guidance: Vec<String>,
    /// PNG data URLs: the circled region plus arrows for the shooting positions.
    pub guidance_images: Vec<String>,
}

impl ReshootPlan {
    fn validate(&self) -> Result<()> {
        if self.regions.is_empty() {
            return Err(SplatError::Process("请至少圈选一个需要补拍的区域".into()));
        }
        ensure_distinct_reshoot_regions(&self.regions)?;
        if self.guidance.len() != self.regions.len()
            || self.guidance_images.len() != self.regions.len()
        {
            return Err(SplatError::Process(
                "补拍区域与补拍指引数量不一致，请重新圈选区域".into(),
            ));
        }
        Ok(())
    }
}

/// Two selections closer than this describe the same spot, not two regions.
const RESHOOT_REGION_PRECISION: f64 = 1e3;

fn reshoot_region_key(region: &crate::project::GaussianCrop) -> String {
    let round = |value: f64| (value * RESHOOT_REGION_PRECISION).round() as i64;
    match region {
        crate::project::GaussianCrop::Sphere { center, radius } => format!(
            "sphere:{}:{}:{}:{}",
            round(center[0]),
            round(center[1]),
            round(center[2]),
            round(*radius)
        ),
        crate::project::GaussianCrop::Box { center, size } => format!(
            "box:{}:{}:{}:{}:{}:{}",
            round(center[0]),
            round(center[1]),
            round(center[2]),
            round(size[0]),
            round(size[1]),
            round(size[2])
        ),
    }
}

/// The reshoot list must describe distinct areas: a repeated selection would ask
/// the shooter for the same footage twice and skew the merged frame set.
fn ensure_distinct_reshoot_regions(regions: &[crate::project::GaussianCrop]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for region in regions {
        if !seen.insert(reshoot_region_key(region)) {
            return Err(SplatError::Process(
                "补拍清单中存在重复区域，请移除重复项或重新圈选不同区域".into(),
            ));
        }
    }
    Ok(())
}

/// Persist the annotated guidance images inside the derived project so the
/// shooting plan stays traceable next to the frames it belongs to.
async fn write_reshoot_guidance_images(
    project_root: &Path,
    images: &[String],
) -> Result<Vec<PathBuf>> {
    const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let directory = project_root.join("reshoot-guidance");
    let mut written = Vec::new();
    for (index, image) in images.iter().enumerate() {
        if image.trim().is_empty() {
            continue;
        }
        let payload = image
            .split_once(',')
            .filter(|(header, _)| header.starts_with("data:image/png"))
            .map(|(_, payload)| payload)
            .ok_or_else(|| SplatError::Process("补拍指引图格式无效，请重新生成".into()))?;
        let bytes = decode_base64(payload)
            .ok_or_else(|| SplatError::Process("补拍指引图无法解码，请重新生成".into()))?;
        if !bytes.starts_with(&PNG_MAGIC) {
            return Err(SplatError::Process("补拍指引图不是有效的 PNG".into()));
        }
        tokio::fs::create_dir_all(&directory).await?;
        let path = directory.join(format!("region-{:02}.png", index + 1));
        tokio::fs::write(&path, &bytes).await?;
        written.push(path);
    }
    Ok(written)
}

/// Minimal standard-alphabet base64 decoder; the repository ships no base64 crate.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

async fn copy_filtered_masks(names: &[String], masks: &Path, output: &Path) -> Result<()> {
    for frame_name in names {
        let mask_name = format!("{frame_name}.png");
        let source = masks.join(&mask_name);
        if !source.is_file() {
            return Err(SplatError::Process(format!(
                "筛选帧缺少对应 Alpha Mask：{mask_name}"
            )));
        }
        tokio::fs::copy(source, output.join(mask_name)).await?;
    }
    Ok(())
}

async fn filtered_masks_match_frames(frames: &Path, masks: &Path) -> Result<bool> {
    let frames = frames.to_path_buf();
    let masks = masks.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if !frames.is_dir() || !masks.is_dir() {
            return Ok(false);
        }
        let frame_names = crate::video::list_images(&frames)?
            .into_iter()
            .filter_map(|path| path.file_name().map(|name| name.to_os_string()))
            .collect::<Vec<_>>();
        for frame_name in &frame_names {
            let mut mask_name = frame_name.clone();
            mask_name.push(".png");
            if !masks.join(mask_name).is_file() {
                return Ok(false);
            }
        }
        let mask_count = std::fs::read_dir(&masks)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .count();
        Ok::<bool, SplatError>(mask_count == frame_names.len())
    })
    .await
    .map_err(|error| SplatError::Process(format!("无法校验筛选 Mask：{error}")))?
}

/// Frames a reshoot project merges from its source project.
///
/// The smart filter already dropped the frames the original reconstruction never
/// used, and a reshoot project matches exhaustively because its new photos are
/// not temporally adjacent to the video frames. Merging the raw set back in would
/// therefore square the matching cost over frames that never contributed to the
/// source model, so the filtered set wins whenever the source produced one. Image
/// sequence sources are never filtered and fall back to their raw frames.
async fn reshoot_source_frames(project_root: &Path) -> Result<PathBuf> {
    let work = project_root.join("work");
    let filtered = work.join("frames_filtered");
    if count_image_files(&filtered).await.unwrap_or(0) > 0 {
        return Ok(filtered);
    }
    Ok(work.join("frames"))
}

async fn count_image_files(directory: &Path) -> Result<u64> {
    let directory = directory.to_path_buf();
    tokio::task::spawn_blocking(move || {
        Ok::<u64, SplatError>(crate::video::list_images(&directory)?.len() as u64)
    })
    .await
    .map_err(|error| SplatError::Process(format!("无法统计输入画面：{error}")))?
}

/// Copy original and reshoot frames into one stable, contiguous image sequence.
/// Source material remains untouched; the derived project exclusively owns `destination`.
async fn copy_merged_frames(original: &Path, reshoot: &Path, destination: &Path) -> Result<()> {
    let original = original.to_path_buf();
    let reshoot = reshoot.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut sources = crate::video::list_images(&original)?;
        sources.extend(crate::video::list_images(&reshoot)?);
        if sources.len() < 2 {
            return Err(SplatError::Process("融合后至少需要 2 张有效画面".into()));
        }
        std::fs::create_dir_all(&destination)?;
        for (index, source) in sources.iter().enumerate() {
            let extension = source
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("jpg")
                .to_ascii_lowercase();
            let target = destination.join(format!("frame_{index:06}.{extension}"));
            std::fs::copy(source, target)?;
        }
        Ok(())
    })
    .await
    .map_err(|error| SplatError::Process(format!("融合输入画面失败：{error}")))?
}

/// Keeps the filter's audit files out of the directory COLMAP scans.
///
/// `frames_filtered` is passed to COLMAP as `--image_path`; anything that is not
/// an image makes the reader log a parse error and inflates the file count it
/// reports progress against. The CLI path already moves these files next to the
/// raw frames, so the pipeline does the same.
async fn relocate_filter_reports(filtered: &Path, frames: &Path) -> Result<()> {
    for name in [
        "metadata.csv",
        "filter_summary.json",
        "filter_forced_keep.log",
    ] {
        let source = filtered.join(name);
        if !source.is_file() {
            continue;
        }
        let target = frames.join(name);
        if let Err(error) = tokio::fs::rename(&source, &target).await {
            return Err(SplatError::Process(format!(
                "无法移出过滤报告 {name}：{error}"
            )));
        }
    }
    Ok(())
}

fn mark_state_terminal(mut state: PipelineStateFile, cancelled: bool) -> PipelineStateFile {
    state.stage = if cancelled {
        PipelineStage::Cancelled
    } else {
        PipelineStage::Failed
    };
    state
}

async fn normalize_checkpoints(paths: &ProjectPaths, state: &mut PipelineStateFile) -> Result<()> {
    let legacy_overwritten_filter = state.input_type == ProjectInputType::Video
        && state.preset.preset().enable_smart_filter
        && state.frames.as_ref().is_some_and(|frames| {
            frames.filtered_frames.is_none() && frames.filter_config_hash.is_none()
        })
        && paths.frames.join("filter_summary.json").is_file();
    let frames_complete = !legacy_overwritten_filter
        && prepared_frames_from_checkpoint(paths, state)
            .await?
            .is_some();
    if !frames_complete {
        state.video = None;
        state.image_sequence = None;
        state.frames = None;
    }
    let filter_complete = frames_complete && filter_checkpoint_complete(paths, state).await?;
    state.filter_complete = filter_complete;

    let database_complete = tokio::fs::metadata(paths.colmap.join("database.db"))
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
    state.features_complete = filter_complete && state.features_complete && database_complete;
    state.matching_complete = state.features_complete && state.matching_complete;
    let mapper_matches_preference = match state.preset.preset().mapper_backend {
        crate::presets::MapperPreference::PreferGlobal => true,
        crate::presets::MapperPreference::ForceIncremental => {
            state.mapper_backend.is_none()
                || state.mapper_backend == Some(crate::engines::MapperBackend::Incremental)
        }
    };
    state.reconstruction_complete = state.matching_complete
        && state.reconstruction_complete
        && mapper_matches_preference
        && best_sparse_model(
            if smart_filter_enabled(state) {
                &paths.frames_filtered
            } else {
                &paths.frames
            },
            &paths.colmap.join("sparse"),
        )
        .await
        .is_ok();
    state.brush_complete = state.reconstruction_complete
        && state.brush_complete
        && brush_candidate(&paths.brush)
            .and_then(|path| inspect_gaussian_ply(&path).ok())
            .is_some();
    state.stage = checkpoint_stage(state);
    Ok(())
}

/// Describes the Brush tuning actually in effect, so the console shows which
/// training parameters ran instead of leaving them implicit in the preset.
fn brush_tuning_note(tuning: crate::presets::BrushTuning) -> String {
    let mut parts = Vec::new();
    if let Some(degree) = tuning.sh_degree {
        parts.push(format!("SH 阶数 {degree}"));
    }
    if let Some(stop) = tuning.growth_stop_iter {
        parts.push(format!("致密化止于 {stop} 步"));
    }
    if let Some(every) = tuning.refine_every {
        parts.push(format!("细化间隔 {every}"));
    }
    if let Some(max) = tuning.max_splats {
        parts.push(format!("高斯上限 {max}"));
    }
    if tuning.single_export {
        parts.push("仅最终导出".to_owned());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    }
}

fn smart_filter_enabled(state: &PipelineStateFile) -> bool {
    state.input_type == ProjectInputType::Video && state.preset.preset().enable_smart_filter
}

/// Whether to calibrate the view graph before mapping.
///
/// Only the global mapper consumes focal-length priors; the incremental mapper
/// initialises and refines intrinsics on its own, so it must not pay for a step
/// that rewrites the shared database.
fn needs_view_graph_calibration(backend: colmap::MapperBackend, supports_calibrator: bool) -> bool {
    backend == colmap::MapperBackend::Global && supports_calibrator
}

fn pipeline_frame_inputs<'a>(
    paths: &'a ProjectPaths,
    state: &PipelineStateFile,
) -> (&'a Path, &'a Path) {
    if smart_filter_enabled(state) {
        (&paths.frames_filtered, &paths.masks_filtered)
    } else {
        (&paths.frames, &paths.masks)
    }
}

fn colmap_input_paths(state: &PipelineStateFile) -> (&'static Path, &'static Path) {
    if smart_filter_enabled(state) {
        (
            Path::new("../frames_filtered"),
            Path::new("../masks_filtered"),
        )
    } else {
        (Path::new("../frames"), Path::new("../masks"))
    }
}

async fn filter_checkpoint_complete(
    paths: &ProjectPaths,
    state: &PipelineStateFile,
) -> Result<bool> {
    let Some(frames) = state.frames.as_ref() else {
        return Ok(false);
    };
    if !smart_filter_enabled(state) {
        return Ok(true);
    }
    let Some(source_frames) = frames.extracted_frames.filter(|count| *count > 0) else {
        return Ok(false);
    };
    let Some(filtered_frames) = frames.filtered_frames.filter(|count| *count > 0) else {
        return Ok(false);
    };
    let expected_hash =
        crate::video::filter_config_hash(&state.preset.preset().smart_filter_config, source_frames);
    if frames.filter_config_hash.as_deref() != Some(expected_hash.as_str()) {
        return Ok(false);
    }
    let actual = count_image_files(&paths.frames_filtered).await?;
    if actual != filtered_frames {
        return Ok(false);
    }
    if frames.has_alpha
        && !filtered_masks_match_frames(&paths.frames_filtered, &paths.masks_filtered).await?
    {
        return Ok(false);
    }
    Ok(true)
}

async fn ensure_filter_checkpoint(
    paths: &ProjectPaths,
    state: &mut PipelineStateFile,
    prepared: &PreparedFrames,
) -> Result<Option<crate::video::FilterOutcome>> {
    if !smart_filter_enabled(state) {
        if let Some(frames) = state.frames.as_mut() {
            frames.filtered_frames = None;
            frames.filter_config_hash = None;
        }
        state.filter_complete = true;
        return Ok(None);
    }
    if filter_checkpoint_complete(paths, state).await? {
        state.filter_complete = true;
        return Ok(None);
    }
    state.filter_complete = false;
    reset_directory(&paths.frames_filtered).await?;
    if prepared.has_alpha {
        reset_directory(&paths.masks_filtered).await?;
    }
    let config = state.preset.preset().smart_filter_config;
    let input = paths.frames.clone();
    let output = paths.frames_filtered.clone();
    let masks = paths.masks.clone();
    let sampling_fps = prepared.plan.sampling_fps;
    let has_alpha = prepared.has_alpha;
    let outcome = tokio::task::spawn_blocking(move || {
        if has_alpha {
            filter_frames_with_masks_at_fps(&input, &output, &masks, &config, sampling_fps)
        } else {
            crate::video::filter_frames_at_fps(&input, &output, &config, sampling_fps)
        }
    })
    .await
    .map_err(|error| SplatError::Process(format!("智能抽帧过滤任务失败：{error}")))?
    .map_err(|error| SplatError::Process(format!("智能抽帧过滤失败：{error}")))?;
    if prepared.has_alpha {
        copy_filtered_masks(
            &outcome.kept_file_names,
            &paths.masks,
            &paths.masks_filtered,
        )
        .await?;
    }
    // The filter writes its report next to the images, but this directory is
    // handed to COLMAP as --image_path, which tries to read every entry and logs
    // BITMAP_ERROR for each non-image it finds. Keep the report with the raw
    // frames instead, exactly as the CLI path does.
    relocate_filter_reports(&paths.frames_filtered, &paths.frames).await?;
    if let Some(frames) = state.frames.as_mut() {
        frames.filtered_frames = Some(outcome.kept_frames as u64);
        frames.filter_config_hash = Some(crate::video::filter_config_hash(
            &config,
            prepared.extracted_frames,
        ));
    }
    state.filter_complete = true;
    Ok(Some(outcome))
}

async fn prepared_frames_from_checkpoint(
    paths: &ProjectPaths,
    state: &PipelineStateFile,
) -> Result<Option<PreparedFrames>> {
    let Some(frames) = state.frames.as_ref() else {
        return Ok(None);
    };
    let Some(extracted_frames) = frames.extracted_frames.filter(|count| *count > 0) else {
        return Ok(None);
    };
    let plan = FramePlan {
        retention_ratio: frames.retention_ratio,
        sampling_fps: frames.sampling_fps,
        estimated_frames: frames.estimated_frames,
    };
    match state.input_type {
        ProjectInputType::Video => {
            let Some(video) = state.video.clone() else {
                return Ok(None);
            };
            let has_alpha = frames.has_alpha || video.has_alpha;
            let Ok(extraction) = validate_extraction(&paths.frames, &paths.masks, has_alpha).await
            else {
                return Ok(None);
            };
            if extraction.frame_count != extracted_frames
                || frames
                    .image_format
                    .as_deref()
                    .is_some_and(|format| format != extraction.image_format.as_str())
                || frames
                    .mask_count
                    .is_some_and(|count| count != extraction.mask_count)
            {
                return Ok(None);
            }
            // 抽帧参数变了就必须重抽：帧数相同不代表帧序列相同（同样数量可以由不同的
            // 抽帧率与取整路径得到），而复用旧帧会让 timestamp_ms 按旧帧率被解释。
            // 之前这里只比对帧数，档位里的抽帧率/保留比例改动可能被静默复用。
            let current = SmartFrameSelection.create_plan(&video, &state.preset.preset());
            if (current.sampling_fps - frames.sampling_fps).abs() > f64::EPSILON
                || (current.retention_ratio - frames.retention_ratio).abs() > f64::EPSILON
            {
                return Ok(None);
            }
            Ok(Some(PreparedFrames {
                input_type: ProjectInputType::Video,
                video: Some(video),
                image_sequence: None,
                plan,
                extracted_frames,
                image_format: extraction.image_format.as_str().into(),
                mask_count: extraction.mask_count,
                has_alpha: extraction.has_alpha,
            }))
        }
        ProjectInputType::Images => {
            let Some(image_sequence) = state.image_sequence.clone() else {
                return Ok(None);
            };
            let frames_dir = paths.frames.clone();
            let masks_dir = paths.masks.clone();
            let Ok(prepared) = tokio::task::spawn_blocking(move || {
                validate_prepared_image_sequence(
                    &frames_dir,
                    &masks_dir,
                    extracted_frames,
                    image_sequence.has_alpha,
                )
                .map(|prepared| (prepared, image_sequence))
            })
            .await
            .map_err(|error| SplatError::Process(format!("图片检查点校验失败：{error}")))?
            else {
                return Ok(None);
            };
            let (prepared, image_sequence) = prepared;
            if frames
                .mask_count
                .is_some_and(|count| count != prepared.mask_count)
            {
                return Ok(None);
            }
            Ok(Some(PreparedFrames {
                input_type: ProjectInputType::Images,
                video: None,
                image_sequence: Some(image_sequence),
                plan,
                extracted_frames,
                image_format: "images".into(),
                mask_count: prepared.mask_count,
                has_alpha: prepared.has_alpha,
            }))
        }
    }
}

fn brush_candidate(root: &Path) -> Option<PathBuf> {
    [root.join("final.ply.tmp"), root.join("final.ply.tmp.ply")]
        .into_iter()
        .find(|path| path.is_file())
}

async fn recover_interrupted_publish(
    paths: &ProjectPaths,
    state: &PipelineStateFile,
) -> Result<()> {
    if state.stage == PipelineStage::Completed
        || !state.brush_complete
        || brush_candidate(&paths.brush).is_some()
    {
        return Ok(());
    }
    let orphan = paths.project.join("final.ply");
    if !orphan.is_file() {
        return Ok(());
    }
    let inspect_path = orphan.clone();
    if tokio::task::spawn_blocking(move || inspect_gaussian_ply(&inspect_path))
        .await
        .map_err(|error| SplatError::Process(format!("PLY 恢复校验任务失败：{error}")))?
        .is_err()
    {
        return Ok(());
    }
    tokio::fs::create_dir_all(&paths.brush).await?;
    atomic_replace_file(&orphan, &paths.brush.join("final.ply.tmp")).await
}

async fn reset_directory(path: &Path) -> Result<()> {
    if path.exists() {
        tokio::fs::remove_dir_all(path).await?;
    }
    tokio::fs::create_dir_all(path).await?;
    Ok(())
}

async fn best_sparse_model(
    frames: &Path,
    sparse: &Path,
) -> Result<(PathBuf, ReconstructionReport)> {
    let frames = frames.to_path_buf();
    let sparse = sparse.to_path_buf();
    tokio::task::spawn_blocking(move || best_sparse_model_blocking(&frames, &sparse))
        .await
        .map_err(|error| SplatError::Process(format!("稀疏模型校验任务失败：{error}")))?
}

fn best_sparse_model_blocking(
    frames: &Path,
    sparse: &Path,
) -> Result<(PathBuf, ReconstructionReport)> {
    let mut best: Option<(PathBuf, ReconstructionReport)> = None;
    for entry in std::fs::read_dir(sparse)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(report) = ReconstructionValidator::validate(frames, &path) {
            if best
                .as_ref()
                .is_none_or(|(_, current)| report.registered_images > current.registered_images)
            {
                best = Some((path, report));
            }
        }
    }
    best.ok_or_else(|| SplatError::Process("COLMAP 未生成完整的稀疏模型".into()))
}

async fn prepare_brush_dataset(root: &Path, frames: &Path, model: &Path) -> Result<PathBuf> {
    let dataset = root.join("dataset");
    let images = dataset.join("images");
    let sparse = dataset.join("sparse").join("0");
    tokio::fs::create_dir_all(&images).await?;
    tokio::fs::create_dir_all(&sparse).await?;
    let mut entries = tokio::fs::read_dir(frames).await?;
    while let Some(entry) = entries.next_entry().await? {
        let source = entry.path();
        if !source.is_file() {
            continue;
        }
        let destination = images.join(entry.file_name());
        if tokio::fs::hard_link(&source, &destination).await.is_err() {
            tokio::fs::copy(&source, &destination).await?;
        }
    }
    for name in ["cameras.bin", "images.bin", "points3D.bin"] {
        tokio::fs::copy(model.join(name), sparse.join(name)).await?;
    }
    Ok(dataset)
}

pub fn default_engine_paths(engine_root: Option<PathBuf>) -> EnginePaths {
    engine_root
        .map(EnginePaths::from_root)
        .unwrap_or_else(|| EnginePaths::discover(None))
}

#[cfg(test)]
mod tests {
    use super::{copy_merged_frames, count_image_files};
    use std::path::Path;

    #[tokio::test]
    async fn merged_frames_are_contiguous_and_sources_remain_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let reshoot = root.path().join("reshoot");
        let destination = root.path().join("merged");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&reshoot).unwrap();
        std::fs::write(original.join("frame_9.jpg"), b"original").unwrap();
        std::fs::write(reshoot.join("hi_1.png"), b"reshoot").unwrap();
        copy_merged_frames(
            Path::new(&original),
            Path::new(&reshoot),
            Path::new(&destination),
        )
        .await
        .unwrap();
        assert_eq!(count_image_files(Path::new(&original)).await.unwrap(), 1);
        assert_eq!(count_image_files(Path::new(&reshoot)).await.unwrap(), 1);
        assert!(destination.join("frame_000000.jpg").is_file());
        assert!(destination.join("frame_000001.png").is_file());
        assert!(!original.join("frame_000000.jpg").exists());
    }

    use super::*;

    #[test]
    fn pipeline_paths_select_filtered_video_and_raw_images() {
        let root = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), root.path().to_path_buf());
        let video = PipelineStateFile::created_for(Quality::Balanced, ProjectInputType::Video);
        assert_eq!(
            pipeline_frame_inputs(&paths, &video).0,
            paths.frames_filtered
        );
        assert_eq!(
            colmap_input_paths(&video).0,
            Path::new("../frames_filtered")
        );

        let images = PipelineStateFile::created_for(Quality::Balanced, ProjectInputType::Images);
        assert_eq!(pipeline_frame_inputs(&paths, &images).0, paths.frames);
        assert_eq!(colmap_input_paths(&images).0, Path::new("../frames"));
    }

    #[tokio::test]
    async fn brush_dataset_copies_only_the_selected_frame_directory() {
        let root = tempfile::tempdir().unwrap();
        let frames = root.path().join("frames_filtered");
        let model = root.path().join("model");
        let brush = root.path().join("brush");
        tokio::fs::create_dir_all(&frames).await.unwrap();
        tokio::fs::create_dir_all(&model).await.unwrap();
        tokio::fs::write(frames.join("kept.jpg"), b"kept")
            .await
            .unwrap();
        for name in ["cameras.bin", "images.bin", "points3D.bin"] {
            tokio::fs::write(model.join(name), b"model").await.unwrap();
        }
        let dataset = prepare_brush_dataset(&brush, &frames, &model)
            .await
            .unwrap();
        assert!(dataset.join("images").join("kept.jpg").is_file());
        assert!(dataset
            .join("sparse")
            .join("0")
            .join("cameras.bin")
            .is_file());
    }

    fn sphere_region(center: [f64; 3], radius: f64) -> crate::project::GaussianCrop {
        crate::project::GaussianCrop::Sphere { center, radius }
    }

    #[test]
    fn rejects_a_repeated_reshoot_region() {
        let regions = vec![
            sphere_region([1.0, 2.0, 3.0], 0.5),
            sphere_region([1.0, 2.0, 3.0], 0.5),
        ];
        let error = ensure_distinct_reshoot_regions(&regions).unwrap_err();
        assert!(error.to_string().contains("重复区域"));
    }

    #[test]
    fn accepts_distinct_reshoot_regions() {
        let regions = vec![
            sphere_region([1.0, 2.0, 3.0], 0.5),
            sphere_region([1.0, 2.0, 3.4], 0.5),
            crate::project::GaussianCrop::Box {
                center: [1.0, 2.0, 3.0],
                size: [1.0, 1.0, 1.0],
            },
        ];
        assert!(ensure_distinct_reshoot_regions(&regions).is_ok());
    }

    #[test]
    fn decodes_png_data_urls_and_rejects_other_payloads() {
        // "iVBORw0KGgo=" is the base64 form of the eight PNG signature bytes.
        let signature = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        assert_eq!(decode_base64("iVBORw0KGgo=").unwrap(), signature);
        assert!(decode_base64("####").is_none());
        assert!(decode_base64("aGVsbG8=").is_some());
    }

    #[tokio::test]
    async fn writes_guidance_images_into_the_derived_project() {
        let root = tempfile::tempdir().unwrap();
        let signature = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let images = vec![
            "data:image/png;base64,iVBORw0KGgo=".to_string(),
            String::new(),
        ];
        let written = write_reshoot_guidance_images(root.path(), &images)
            .await
            .unwrap();

        assert_eq!(written.len(), 1);
        assert_eq!(
            written[0],
            root.path().join("reshoot-guidance").join("region-01.png")
        );
        assert_eq!(std::fs::read(&written[0]).unwrap(), signature);
        assert!(!root
            .path()
            .join("reshoot-guidance")
            .join("region-02.png")
            .exists());
    }

    #[tokio::test]
    async fn rejects_a_guidance_image_that_is_not_png() {
        let root = tempfile::tempdir().unwrap();
        let images = vec!["data:image/png;base64,aGVsbG8=".to_string()];
        let error = write_reshoot_guidance_images(root.path(), &images)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("PNG"));
        assert!(!root.path().join("reshoot-guidance").exists());
    }

    #[test]
    fn parses_ffmpeg_progress() {
        assert_eq!(parse_ffmpeg_frame("frame=127"), Some(127));
        assert_eq!(parse_ffmpeg_frame("progress=continue"), None);
    }

    #[test]
    fn reports_the_latest_durable_checkpoint_instead_of_terminal_status() {
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.stage = PipelineStage::Cancelled;
        assert_eq!(checkpoint_stage(&state), PipelineStage::Created);

        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 100,
            extracted_frames: Some(100),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        state.features_complete = true;
        state.matching_complete = true;
        assert_eq!(checkpoint_stage(&state), PipelineStage::Matching);

        state.reconstruction_complete = true;
        state.brush_complete = true;
        assert_eq!(checkpoint_stage(&state), PipelineStage::TrainingSplats);
    }

    #[test]
    fn terminal_state_preserves_every_checkpoint() {
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 100,
            extracted_frames: Some(100),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        state.features_complete = true;
        state.matching_complete = true;
        state.reconstruction_complete = true;
        state.brush_complete = true;

        let failed = mark_state_terminal(state.clone(), false);
        let cancelled = mark_state_terminal(state, true);
        for terminal in [failed, cancelled] {
            assert_eq!(
                terminal.frames.as_ref().unwrap().extracted_frames,
                Some(100)
            );
            assert!(terminal.features_complete);
            assert!(terminal.matching_complete);
            assert!(terminal.reconstruction_complete);
            assert!(terminal.brush_complete);
        }
    }

    #[tokio::test]
    async fn frame_checkpoint_requires_every_recorded_frame() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        tokio::fs::write(paths.frames.join("frame_000001.jpg"), b"one")
            .await
            .unwrap();
        tokio::fs::write(paths.frames.join("frame_000002.jpg"), b"two")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.video = Some(VideoInfo {
            duration: 1.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 30,
            codec: "h264".into(),
            rotation: 0,
            pixel_format: "yuv420p".into(),
            has_alpha: false,
        });
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 2,
            extracted_frames: Some(2),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });

        assert!(prepared_frames_from_checkpoint(&paths, &state)
            .await
            .unwrap()
            .is_some());
        tokio::fs::remove_file(paths.frames.join("frame_000002.jpg"))
            .await
            .unwrap();
        assert!(prepared_frames_from_checkpoint(&paths, &state)
            .await
            .unwrap()
            .is_none());
    }

    /// 抽帧参数变了就必须重抽：帧数够也救不了——同样数量可以由不同抽帧率与取整路径得到，
    /// 复用旧帧会让 `timestamp_ms` 按旧帧率被解释。这是 S1.3 参数化之后必须堵住的洞。
    #[tokio::test]
    async fn frame_checkpoint_is_invalidated_when_the_frame_plan_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        tokio::fs::write(paths.frames.join("frame_000001.jpg"), b"one")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.video = Some(VideoInfo {
            duration: 1.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 30,
            codec: "h264".into(),
            rotation: 0,
            pixel_format: "yuv420p".into(),
            has_alpha: false,
        });
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            // Balanced@30fps 解析出的候选密度是 22.5；这里伪装成 15 模拟"档位改过抽帧率"。
            sampling_fps: 15.0,
            estimated_frames: 1,
            extracted_frames: Some(1),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        assert!(
            prepared_frames_from_checkpoint(&paths, &state)
                .await
                .unwrap()
                .is_none(),
            "抽帧率与当前档位不符时不得复用旧帧"
        );

        // 对齐到当前档位解析出的值后，断点重新可用。
        if let Some(frames) = state.frames.as_mut() {
            frames.sampling_fps = 22.5;
        }
        assert!(prepared_frames_from_checkpoint(&paths, &state)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn interrupted_publish_restores_a_valid_orphan_as_a_brush_checkpoint() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        let valid = b"ply\nformat binary_little_endian 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nproperty float f_dc_0\nproperty float opacity\nproperty float scale_0\nproperty float rot_0\nend_header\n";
        tokio::fs::write(paths.project.join("final.ply"), valid)
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.brush_complete = true;

        recover_interrupted_publish(&paths, &state).await.unwrap();

        assert!(!paths.project.join("final.ply").exists());
        assert!(paths.brush.join("final.ply.tmp").is_file());
    }

    #[tokio::test]
    async fn image_sequence_checkpoint_requires_every_image_and_mask() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        tokio::fs::create_dir_all(&paths.masks).await.unwrap();
        for name in ["frame_000001.png", "frame_000002.jpg"] {
            tokio::fs::write(paths.frames.join(name), b"image")
                .await
                .unwrap();
            tokio::fs::write(paths.masks.join(format!("{name}.png")), b"mask")
                .await
                .unwrap();
        }
        let mut state = PipelineStateFile::created_for(Quality::Balanced, ProjectInputType::Images);
        state.image_sequence = Some(ImageSequenceInfo {
            image_count: 2,
            width: 1920,
            height: 1080,
            has_alpha: true,
            requires_large_sequence_confirmation: false,
        });
        state.frames = Some(FrameState {
            retention_ratio: 1.0,
            sampling_fps: 0.0,
            estimated_frames: 2,
            extracted_frames: Some(2),
            image_format: Some("images".into()),
            mask_count: Some(2),
            has_alpha: true,
            filtered_frames: None,
            filter_config_hash: None,
        });

        assert!(prepared_frames_from_checkpoint(&paths, &state)
            .await
            .unwrap()
            .is_some());
        tokio::fs::remove_file(paths.masks.join("frame_000002.jpg.png"))
            .await
            .unwrap();
        assert!(prepared_frames_from_checkpoint(&paths, &state)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn transparent_frame_checkpoint_requires_matching_masks() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        tokio::fs::create_dir_all(&paths.masks).await.unwrap();
        tokio::fs::write(paths.frames.join("frame_000001.png"), b"rgba")
            .await
            .unwrap();
        tokio::fs::write(paths.masks.join("frame_000001.png.png"), b"mask")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.video = Some(VideoInfo {
            duration: 1.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 30,
            codec: "prores".into(),
            rotation: 0,
            pixel_format: "yuva444p10le".into(),
            has_alpha: true,
        });
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 1,
            extracted_frames: Some(1),
            image_format: Some("png".into()),
            mask_count: Some(1),
            has_alpha: true,
            filtered_frames: None,
            filter_config_hash: None,
        });

        let prepared = prepared_frames_from_checkpoint(&paths, &state)
            .await
            .unwrap()
            .unwrap();
        assert!(prepared.has_alpha);
        assert_eq!(prepared.mask_count, 1);

        tokio::fs::remove_file(paths.masks.join("frame_000001.png.png"))
            .await
            .unwrap();
        assert!(prepared_frames_from_checkpoint(&paths, &state)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn empty_colmap_database_downgrades_the_feature_checkpoint() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        tokio::fs::create_dir_all(&paths.colmap).await.unwrap();
        tokio::fs::write(paths.frames.join("frame_000001.jpg"), b"jpeg")
            .await
            .unwrap();
        tokio::fs::write(paths.colmap.join("database.db"), b"")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.video = Some(VideoInfo {
            duration: 1.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 30,
            codec: "h264".into(),
            rotation: 0,
            pixel_format: "yuv420p".into(),
            has_alpha: false,
        });
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 1,
            extracted_frames: Some(1),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        state.features_complete = true;
        state.matching_complete = true;

        normalize_checkpoints(&paths, &mut state).await.unwrap();

        assert!(!state.features_complete);
        assert!(!state.matching_complete);
        assert_eq!(state.stage, PipelineStage::ExtractingFrames);
    }

    #[test]
    fn brush_tuning_note_reports_only_active_knobs() {
        use crate::presets::BrushTuning;
        // Untuned presets add nothing to the console line.
        assert_eq!(brush_tuning_note(BrushTuning::brush_defaults()), "");
        let note = brush_tuning_note(BrushTuning::high_detail());
        assert!(note.contains("SH 阶数 2"), "{note}");
        assert!(note.contains("致密化止于 12000 步"), "{note}");
        assert!(note.contains("仅最终导出"), "{note}");
        // Knobs that stay unset must not be advertised as if they were applied.
        assert!(!note.contains("细化间隔"), "{note}");
        assert!(!note.contains("高斯上限"), "{note}");
    }

    #[tokio::test]
    async fn reshoot_prefers_the_filtered_frames_of_its_source() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let raw = root.join("work").join("frames");
        tokio::fs::create_dir_all(&raw).await.unwrap();
        for index in 0..4 {
            tokio::fs::write(raw.join(format!("frame_{index:06}.jpg")), b"raw")
                .await
                .unwrap();
        }

        // Without a filtered set, an image-sequence source has to fall back to
        // its raw frames.
        assert_eq!(reshoot_source_frames(root).await.unwrap(), raw);

        // With one, the reshoot merges only what the source reconstruction used,
        // because the derived project matches exhaustively and merging the raw
        // set back in would square that cost over frames that never contributed.
        let filtered = root.join("work").join("frames_filtered");
        tokio::fs::create_dir_all(&filtered).await.unwrap();
        tokio::fs::write(filtered.join("frame_000001.jpg"), b"kept")
            .await
            .unwrap();
        assert_eq!(reshoot_source_frames(root).await.unwrap(), filtered);

        // An empty filtered directory must not win over usable raw frames.
        tokio::fs::remove_file(filtered.join("frame_000001.jpg"))
            .await
            .unwrap();
        assert_eq!(reshoot_source_frames(root).await.unwrap(), raw);
    }

    #[tokio::test]
    async fn filter_reports_leave_the_colmap_image_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let filtered = temporary.path().join("frames_filtered");
        let frames = temporary.path().join("frames");
        tokio::fs::create_dir_all(&filtered).await.unwrap();
        tokio::fs::create_dir_all(&frames).await.unwrap();
        for name in [
            "metadata.csv",
            "filter_summary.json",
            "filter_forced_keep.log",
        ] {
            tokio::fs::write(filtered.join(name), b"report")
                .await
                .unwrap();
        }
        tokio::fs::write(filtered.join("frame_000001.jpg"), b"image")
            .await
            .unwrap();

        relocate_filter_reports(&filtered, &frames).await.unwrap();

        // COLMAP's --image_path must contain images only.
        let remaining = std::fs::read_dir(&filtered)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(remaining, vec!["frame_000001.jpg".to_owned()]);
        for name in [
            "metadata.csv",
            "filter_summary.json",
            "filter_forced_keep.log",
        ] {
            assert!(frames.join(name).is_file(), "{name} should move to frames");
        }

        // Re-running must stay harmless when the reports are already elsewhere.
        relocate_filter_reports(&filtered, &frames).await.unwrap();
    }

    #[test]
    fn degenerate_reconstructions_never_reach_training() {
        let report = |registered: u64, input: u64| ReconstructionReport {
            input_images: input,
            registered_images: registered,
            registered_ratio: registered as f64 / input as f64,
            points_3d: registered * 100,
            quality: ReconstructionQuality::Warning,
        };

        // The real failure this guards: a 367-image run that registered 2 images
        // still produced a sparse directory, and Brush then trained on it four
        // times. COLMAP exits 0 in that case, so the ratio is the only signal.
        let degenerate = report(2, 367);
        let error = ensure_trainable_reconstruction(&degenerate).unwrap_err();
        assert!(error.to_string().contains("注册率"), "{error}");

        // No images at all is the same verdict.
        assert!(ensure_trainable_reconstruction(&report(0, 367)).is_err());
        // The boundary itself is trainable, and so is anything above it.
        assert!(ensure_trainable_reconstruction(&report(60, 100)).is_ok());
        assert!(ensure_trainable_reconstruction(&report(59, 100)).is_err());
        assert!(ensure_trainable_reconstruction(&report(295, 295)).is_ok());
    }

    #[test]
    fn brush_progress_message_names_an_overrun_estimate() {
        // Inside the estimate the bar carries the percentage.
        let within = brush_progress_message(30_000, 144_000, 0.20);
        assert!(within.contains("估算进度 20%"), "{within}");

        // Past the estimate the bar is pinned at 95%, so the message has to say
        // the run is over budget rather than looking frozen.
        let overrun = brush_progress_message(300_000, 144_000, 0.95);
        assert!(overrun.contains("已超过预估时间"), "{overrun}");
        assert!(!overrun.contains("估算进度"), "{overrun}");

        // An unknown estimate must not be reported as an overrun.
        let unknown = brush_progress_message(300_000, 0, 0.0);
        assert!(!unknown.contains("已超过预估时间"), "{unknown}");
    }

    #[test]
    fn view_graph_calibration_only_prepares_the_global_mapper() {
        // The calibrator rewrites the shared database, so it must never run as
        // part of an incremental reconstruction that does not consume priors.
        assert!(needs_view_graph_calibration(
            colmap::MapperBackend::Global,
            true
        ));
        assert!(!needs_view_graph_calibration(
            colmap::MapperBackend::Incremental,
            true
        ));
        // A build without the subcommand must not attempt the step at all.
        assert!(!needs_view_graph_calibration(
            colmap::MapperBackend::Global,
            false
        ));
    }

    #[tokio::test]
    async fn image_inputs_treat_filter_completion_as_raw_frame_completion() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        let mut state = PipelineStateFile::created_for(Quality::Balanced, ProjectInputType::Images);
        state.frames = Some(FrameState {
            retention_ratio: 1.0,
            sampling_fps: 0.0,
            estimated_frames: 1,
            extracted_frames: Some(1),
            image_format: Some("images".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        assert!(filter_checkpoint_complete(&paths, &state).await.unwrap());
    }

    #[tokio::test]
    async fn filtering_preserves_raw_frames_and_extracted_count() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        for index in 0..4 {
            let image = image::GrayImage::from_fn(32, 32, |x, y| {
                image::Luma([((x * 13 + y * 17 + index * 29) % 255) as u8])
            });
            image
                .save(paths.frames.join(format!("frame_{index:06}.png")))
                .unwrap();
        }
        let mut state = PipelineStateFile::created(Quality::Balanced);
        let plan = FramePlan {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 4,
        };
        state.frames = Some(FrameState {
            retention_ratio: plan.retention_ratio,
            sampling_fps: plan.sampling_fps,
            estimated_frames: plan.estimated_frames,
            extracted_frames: Some(4),
            image_format: Some("png".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        let prepared = PreparedFrames {
            input_type: ProjectInputType::Video,
            video: None,
            image_sequence: None,
            plan,
            extracted_frames: 4,
            image_format: "png".into(),
            mask_count: 0,
            has_alpha: false,
        };
        ensure_filter_checkpoint(&paths, &mut state, &prepared)
            .await
            .unwrap();
        assert_eq!(count_image_files(&paths.frames).await.unwrap(), 4);
        assert_eq!(state.frames.as_ref().unwrap().extracted_frames, Some(4));
        assert!(state.frames.as_ref().unwrap().filtered_frames.is_some());
    }

    #[tokio::test]
    async fn filter_checkpoint_requires_matching_hash_and_directory_count() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames_filtered)
            .await
            .unwrap();
        tokio::fs::write(paths.frames_filtered.join("frame_000001.jpg"), b"one")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        let config = state.preset.preset().smart_filter_config;
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 2,
            extracted_frames: Some(2),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: Some(1),
            filter_config_hash: Some(crate::video::filter_config_hash(&config, 2)),
        });
        assert!(filter_checkpoint_complete(&paths, &state).await.unwrap());
        state.frames.as_mut().unwrap().filter_config_hash =
            Some(crate::video::filter_config_hash(&config, 3));
        assert!(!filter_checkpoint_complete(&paths, &state).await.unwrap());
        state.frames.as_mut().unwrap().filter_config_hash =
            Some(crate::video::filter_config_hash(&config, 2));
        tokio::fs::write(paths.frames_filtered.join("frame_000002.jpg"), b"two")
            .await
            .unwrap();
        assert!(!filter_checkpoint_complete(&paths, &state).await.unwrap());
    }

    #[tokio::test]
    async fn invalid_filter_checkpoint_invalidates_features_without_invalidating_raw_frames() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        tokio::fs::create_dir_all(&paths.colmap).await.unwrap();
        tokio::fs::write(paths.frames.join("frame_000001.jpg"), b"jpeg")
            .await
            .unwrap();
        tokio::fs::write(paths.colmap.join("database.db"), b"database")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.video = Some(VideoInfo {
            duration: 1.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 30,
            codec: "h264".into(),
            rotation: 0,
            pixel_format: "yuv420p".into(),
            has_alpha: false,
        });
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 1,
            extracted_frames: Some(1),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: Some(1),
            filter_config_hash: Some("stale".into()),
        });
        state.features_complete = true;
        state.matching_complete = true;
        normalize_checkpoints(&paths, &mut state).await.unwrap();
        assert!(state.frames.is_some());
        assert!(!state.filter_complete);
        assert!(!state.features_complete);
        assert!(!state.matching_complete);
    }

    #[tokio::test]
    async fn alpha_filter_checkpoint_requires_exact_mask_pairing() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames_filtered)
            .await
            .unwrap();
        tokio::fs::create_dir_all(&paths.masks_filtered)
            .await
            .unwrap();
        tokio::fs::write(paths.frames_filtered.join("frame_000001.png"), b"frame")
            .await
            .unwrap();
        tokio::fs::write(paths.masks_filtered.join("frame_999999.png.png"), b"mask")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        let config = state.preset.preset().smart_filter_config;
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 1,
            extracted_frames: Some(1),
            image_format: Some("png".into()),
            mask_count: Some(1),
            has_alpha: true,
            filtered_frames: Some(1),
            filter_config_hash: Some(crate::video::filter_config_hash(&config, 1)),
        });
        assert!(!filter_checkpoint_complete(&paths, &state).await.unwrap());
    }

    #[tokio::test]
    async fn legacy_overwritten_filter_checkpoint_forces_raw_reextraction() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::existing(uuid::Uuid::nil(), temporary.path().to_path_buf());
        tokio::fs::create_dir_all(&paths.frames).await.unwrap();
        tokio::fs::create_dir_all(&paths.colmap).await.unwrap();
        tokio::fs::write(paths.frames.join("frame_000001.jpg"), b"filtered")
            .await
            .unwrap();
        tokio::fs::write(paths.frames.join("filter_summary.json"), b"{}")
            .await
            .unwrap();
        tokio::fs::write(paths.colmap.join("database.db"), b"database")
            .await
            .unwrap();
        let mut state = PipelineStateFile::created(Quality::Balanced);
        state.video = Some(VideoInfo {
            duration: 1.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 30,
            codec: "h264".into(),
            rotation: 0,
            pixel_format: "yuv420p".into(),
            has_alpha: false,
        });
        state.frames = Some(FrameState {
            retention_ratio: 0.5,
            sampling_fps: 22.5,
            estimated_frames: 1,
            extracted_frames: Some(1),
            image_format: Some("jpeg".into()),
            mask_count: Some(0),
            has_alpha: false,
            filtered_frames: None,
            filter_config_hash: None,
        });
        state.features_complete = true;
        normalize_checkpoints(&paths, &mut state).await.unwrap();
        assert!(state.frames.is_none());
        assert!(!state.filter_complete);
        assert!(!state.features_complete);
    }

    #[test]
    fn parses_colmap_file_progress() {
        assert_eq!(
            parse_bracket_progress("Processed file [23/533]"),
            Some((23, 533))
        );
        assert_eq!(
            parse_bracket_progress("Processing image [4/10]"),
            Some((4, 10))
        );
    }

    #[test]
    fn parses_mapper_registration() {
        let counter = AtomicU64::new(0);
        let parsed = parse_mapper_progress(
            "Registering image #90 (num_reg_frames=86)",
            &counter,
            Some(100),
        )
        .unwrap();
        match parsed {
            MapperProgress::Registered { current, .. } => assert_eq!(current, 86),
            MapperProgress::Stage { .. } => panic!("expected a registration count"),
        }
        assert_eq!(counter.load(Ordering::Relaxed), 86);
    }

    #[test]
    fn parses_global_mapper_stages_as_progress() {
        // The global mapper never counts registered images, so the Reconstructing
        // stage would otherwise report no progress at all on the default backend.
        let counter = AtomicU64::new(0);
        let expected = [
            "I20260913 03:56:56.504001 32524 global_mapper.cc:465] === Running rotation averaging ===",
            "I20260913 03:56:56.504001 32524 global_mapper.cc:477] === Running track establishment ===",
            "I20260913 03:56:56.504001 32524 global_mapper.cc:487] === Running global positioning ===",
            "I20260913 03:56:56.504001 32524 global_mapper.cc:502] === Running iterative bundle adjustment ===",
            "I20260913 03:56:56.504001 32524 global_mapper.cc:519] === Running iterative retriangulation and refinement ===",
        ];
        let mut previous = -1.0_f32;
        for line in expected {
            let Some(MapperProgress::Stage { fraction, .. }) =
                parse_mapper_progress(line, &counter, Some(295))
            else {
                panic!("global mapper stage line was not recognised: {line}");
            };
            assert!(fraction > previous, "stage progress must advance: {line}");
            previous = fraction;
        }
        assert!(
            previous < 1.0,
            "the stage walk must leave room for completion"
        );
        // Unrelated chatter must not be mistaken for a stage.
        assert!(parse_mapper_progress("Loading images...", &counter, None).is_none());
    }

    #[test]
    fn mapper_refinement_keeps_the_latest_registered_count() {
        let counter = AtomicU64::new(0);
        parse_mapper_progress(
            "Registering image #90 (num_reg_frames=86)",
            &counter,
            Some(100),
        )
        .unwrap();

        let retriangulation = parse_mapper_progress(
            "Retriangulation and Global bundle adjustment",
            &counter,
            Some(100),
        )
        .unwrap();
        match retriangulation {
            MapperProgress::Registered {
                current,
                total,
                message,
            } => {
                assert_eq!(current, 86);
                assert_eq!(total, Some(100));
                assert_eq!(message, "Retriangulation and Global bundle adjustment");
            }
            MapperProgress::Stage { .. } => panic!("expected a registration count"),
        }

        let bundle_adjustment =
            parse_mapper_progress("Global bundle adjustment", &counter, Some(100)).unwrap();
        match bundle_adjustment {
            MapperProgress::Registered { current, total, .. } => {
                assert_eq!(current, 86);
                assert_eq!(total, Some(100));
            }
            MapperProgress::Stage { .. } => panic!("expected a registration count"),
        }
    }

    #[test]
    fn mapper_refinement_without_a_registration_count_stays_indeterminate() {
        let counter = AtomicU64::new(0);
        assert!(parse_mapper_progress(
            "Retriangulation and Global bundle adjustment",
            &counter,
            Some(100),
        )
        .is_none());
    }

    #[test]
    fn mapper_registration_count_only_moves_forward() {
        let counter = AtomicU64::new(0);
        parse_mapper_progress("num_reg_frames=86", &counter, Some(100)).unwrap();
        parse_mapper_progress("num_reg_frames=91", &counter, Some(100)).unwrap();
        parse_mapper_progress("num_reg_frames=89", &counter, Some(100)).unwrap();

        let value = parse_mapper_progress(
            "Retriangulation and Global bundle adjustment",
            &counter,
            Some(100),
        )
        .unwrap();
        match value {
            MapperProgress::Registered { current, .. } => assert_eq!(current, 91),
            MapperProgress::Stage { .. } => panic!("expected a registration count"),
        }
    }

    #[test]
    fn brush_estimated_progress_advances_and_stops_at_ninety_five_percent() {
        assert_eq!(estimated_brush_progress(0, 100_000), 0.0);
        assert!((estimated_brush_progress(50_000, 100_000) - 0.475).abs() < f32::EPSILON);
        assert!((estimated_brush_progress(100_000, 100_000) - 0.95).abs() < f32::EPSILON);
        assert!((estimated_brush_progress(500_000, 100_000) - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn brush_estimated_progress_handles_an_invalid_duration() {
        assert_eq!(estimated_brush_progress(10_000, 0), 0.0);
    }

    #[test]
    fn event_sequence_is_strictly_increasing() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let sink = EventSink {
            emit: Arc::new(move |event| captured.lock().unwrap().push(event.sequence)),
            sequence: Arc::new(AtomicU64::new(0)),
            last_progress_milli_percent: Arc::new(AtomicU64::new(0)),
            last_stage: Arc::new(std::sync::Mutex::new(None)),
            dispatch: Arc::new(std::sync::Mutex::new(())),
            started: Instant::now(),
        };
        sink.stage(PipelineStage::Created, 0.0, "created");
        sink.stage(PipelineStage::ProbingVideo, 0.0, "probing");
        assert_eq!(*events.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn terminal_event_keeps_the_last_real_progress_and_clears_stage_progress() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let sink = EventSink {
            emit: Arc::new(move |event| captured.lock().unwrap().push(event)),
            sequence: Arc::new(AtomicU64::new(0)),
            last_progress_milli_percent: Arc::new(AtomicU64::new(0)),
            last_stage: Arc::new(std::sync::Mutex::new(None)),
            dispatch: Arc::new(std::sync::Mutex::new(())),
            started: Instant::now(),
        };
        sink.stage(PipelineStage::TrainingSplats, 0.5, "training");
        sink.terminal(&SplatError::Process("boom".into()));

        let events = events.lock().unwrap();
        assert_eq!(events[0].progress, 79.0);
        assert_eq!(events[1].progress, 79.0);
        assert_eq!(events[1].stage_progress, None);
        assert_eq!(events[1].stage, PipelineStage::Failed);
        assert_eq!(
            *sink.last_stage.lock().unwrap(),
            Some(PipelineStage::TrainingSplats)
        );
    }
}
