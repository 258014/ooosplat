use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{
    error::{Result, SplatError},
    presets::QualityPreset,
    process::{ProcessManager, ProcessObserver, ProcessSpec},
};

pub fn require_verified_cli(executable: &Path) -> Result<()> {
    if executable.is_file() {
        Ok(())
    } else {
        Err(SplatError::EngineMissing(executable.display().to_string()))
    }
}

pub async fn train(
    executable: &Path,
    dataset: &Path,
    output_directory: &Path,
    preset: QualityPreset,
    log_path: PathBuf,
    manager: &ProcessManager,
    observer: Option<ProcessObserver>,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(output_directory).await?;
    let candidate = output_directory.join("final.ply.tmp");
    if candidate.exists() {
        tokio::fs::remove_file(&candidate).await?;
    }
    // Export intermediate snapshots during training so the app can offer a live
    // preview of the splats as they refine (the final export still happens when
    // the trainer reaches the requested step count).
    let export_every = ((preset.brush_iterations as u64) / 10).clamp(500, 2_000);
    let output = manager
        .run(ProcessSpec {
            executable: executable.to_path_buf(),
            args: vec![
                OsString::from("--total-steps"),
                preset.brush_iterations.to_string().into(),
                OsString::from("--max-resolution"),
                preset.brush_max_resolution.to_string().into(),
                OsString::from("--export-every"),
                export_every.to_string().into(),
                OsString::from("--export-path"),
                output_directory.into(),
                OsString::from("--export-name"),
                OsString::from("final.ply.tmp"),
                dataset.into(),
            ],
            working_directory: Some(output_directory.to_path_buf()),
            log_path: Some(log_path),
            observer,
        })
        .await?;
    if !output.success {
        return Err(SplatError::Process(format!(
            "Brush 退出码 {:?}",
            output.exit_code
        )));
    }
    let candidate = if candidate.is_file() {
        candidate
    } else {
        let alternate = output_directory.join("final.ply.tmp.ply");
        if alternate.is_file() {
            alternate
        } else {
            candidate
        }
    };
    if !candidate.is_file() {
        return Err(SplatError::Process(format!(
            "Brush 未生成预期文件：{}",
            candidate.display()
        )));
    }
    Ok(candidate)
}
