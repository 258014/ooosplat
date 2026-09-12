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

/// Builds the Brush training command line.
///
/// Only flags that the shipped Brush 0.3.0 binary really exposes are emitted;
/// see `docs/brush_help.txt` (produced by `scripts/probe_brush_help.ps1`) for the
/// authoritative list. Optional knobs are omitted entirely when unset so Brush
/// keeps its own default instead of receiving an empty value.
fn training_args(preset: QualityPreset, output_directory: &Path, dataset: &Path) -> Vec<OsString> {
    // A single export at the end of training: intermediate exports copy the whole
    // splat set from device to host, which costs real time and, with a fixed
    // export name, only overwrites the same file.
    let mut args = vec![
        OsString::from("--total-steps"),
        preset.brush_iterations.to_string().into(),
        OsString::from("--max-resolution"),
        preset.brush_max_resolution.to_string().into(),
        OsString::from("--export-every"),
        preset.brush_iterations.to_string().into(),
        OsString::from("--export-path"),
        output_directory.into(),
        OsString::from("--export-name"),
        OsString::from("final.ply.tmp"),
        OsString::from("--sh-degree"),
        preset.brush_sh_degree.to_string().into(),
    ];
    if let Some(growth_stop_iter) = preset.brush_growth_stop_iter {
        args.push(OsString::from("--growth-stop-iter"));
        args.push(growth_stop_iter.to_string().into());
    }
    if let Some(refine_every) = preset.brush_refine_every {
        args.push(OsString::from("--refine-every"));
        args.push(refine_every.to_string().into());
    }
    if let Some(max_splats) = preset.brush_max_splats {
        args.push(OsString::from("--max-splats"));
        args.push(max_splats.to_string().into());
    }
    // The dataset stays last: a leading positional argument would be parsed as
    // the source path instead of a flag.
    args.push(dataset.into());
    args
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
    let output = manager
        .run(ProcessSpec {
            executable: executable.to_path_buf(),
            args: training_args(preset, output_directory, dataset),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::Quality;

    fn args(preset: QualityPreset) -> Vec<String> {
        training_args(preset, Path::new("out"), Path::new("dataset/dense"))
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn value_of(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    }

    #[test]
    fn sh_degree_is_forwarded_for_every_preset() {
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            let preset = quality.preset();
            let args = args(preset);
            assert_eq!(
                value_of(&args, "--sh-degree").as_deref(),
                Some(preset.brush_sh_degree.to_string().as_str())
            );
        }
    }

    #[test]
    fn unset_optional_knobs_are_omitted_entirely() {
        let args = args(Quality::High.preset());
        assert!(!args.iter().any(|arg| arg == "--growth-stop-iter"));
        assert!(!args.iter().any(|arg| arg == "--refine-every"));
        assert!(!args.iter().any(|arg| arg == "--max-splats"));
    }

    #[test]
    fn set_optional_knobs_carry_their_configured_value() {
        let args = args(Quality::Balanced.preset());
        assert_eq!(
            value_of(&args, "--growth-stop-iter").as_deref(),
            Some("9000")
        );
        assert!(!args.iter().any(|arg| arg == "--refine-every"));
        assert!(!args.iter().any(|arg| arg == "--max-splats"));
    }

    #[test]
    fn export_every_matches_total_steps() {
        for quality in [Quality::Fast, Quality::Balanced, Quality::High] {
            let preset = quality.preset();
            let args = args(preset);
            assert_eq!(
                value_of(&args, "--total-steps"),
                value_of(&args, "--export-every")
            );
        }
    }

    #[test]
    fn export_name_and_path_are_stable() {
        let args = args(Quality::Balanced.preset());
        assert_eq!(
            value_of(&args, "--export-name").as_deref(),
            Some("final.ply.tmp")
        );
        assert_eq!(value_of(&args, "--export-path").as_deref(), Some("out"));
    }

    #[test]
    fn the_dataset_stays_the_last_argument() {
        let args = args(Quality::Fast.preset());
        assert_eq!(args.last().map(String::as_str), Some("dataset/dense"));
    }
}
