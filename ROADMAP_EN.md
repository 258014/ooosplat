# OOOSplat Roadmap

[中文](ROADMAP.md) | [English](ROADMAP_EN.md)

This roadmap describes OOOSplat's product direction and implementation priorities. P0–P3 indicate relative priority; they are not release numbers and do not guarantee delivery dates.

Current version: **0.4.0**. This release focuses on multiple input types, recoverable generation pipelines, and non-destructive Gaussian editing.

## Product Principles

- **One-click workflow**: Continue reducing engine configuration and manual steps so users can turn source media directly into usable Gaussian Splatting results.
- **Local first**: Reconstruction, training, preview, and export should primarily use the user's own hardware without depending on cloud processing services.
- **Safe and non-destructive**: Keep source media and project data local by default, preserve original results during editing and export, and make file locations and processing state easy to trace.

## Priority Levels

- **P0 · Near-term focus**: Core experience, stability, speed, and output quality.
- **P1 · High priority**: Evaluation infrastructure, guidance, and reconstruction diagnostics.
- **P2 · Platform and output expansion**: New export capabilities and production-ready platform delivery.
- **P3 · Longer-term exploration**: Additional capture methods and use cases.

## Planned Work

| Priority | Item | Goal | GitHub Issue |
| --- | --- | --- | --- |
| P0 | Large Gaussian preview stability and performance | Reduce memory peaks when loading and editing very large PLY files, and improve selection, rendering, and preview-exit stability. | To be created |
| P0 | Performance optimization | Reduce total time spent on image preparation, feature extraction, matching, reconstruction, and training while preserving result compatibility. | To be created |
| P0 | Generation quality optimization | Improve camera registration, geometric completeness, visual detail, and edge quality, with measurable strategies for correcting low-quality results. | To be created |
| P1 | Gaussian generation benchmark | Establish reproducible datasets, hardware profiles, quality metrics, and timing metrics to compare generation speed, resource use, and output quality across releases. | To be created |
| P1 | Video capture guidance UI | Provide guidance on orbit paths, movement speed, overlap, lighting, and common capture mistakes before generation begins. | To be created |
| P1 | In-app Chinese/English switching | Let users switch between Chinese and English in Settings, covering primary UI, interaction guidance, status messages, and error summaries while persisting the choice across restarts. | To be created |
| P1 | Reconstruction diagnostics and recovery guidance | Turn low registration rates, image-quality problems, and GPU backend errors into clear, actionable guidance. | To be created |
| P2 | Mesh export | Convert reconstruction results to common Mesh formats with documented texture, coordinate-system, and quality options. | To be created |
| P2 | macOS signing and notarization | Add official signing and notarization for Apple Silicon packages to improve first-run installation. | To be created |
| P3 | Panoramic video support | Explore a workflow for using panoramic video as input and producing usable Gaussian Splatting results. | To be created |

## Completed

| Status | Item | Delivery | GitHub Issue |
| --- | --- | --- | --- |
| Completed | Ubuntu 24.04 Alpha | Provides an x86_64 `.deb` desktop package and CLI using system FFmpeg/FFprobe/CPU COLMAP and a pinned Brush runtime bundled with the package. This does not imply support for other Linux distributions. | [#5 Add Linux support](https://github.com/ooolabdev/ooosplat/issues/5), delivered by [PR #10](https://github.com/ooolabdev/ooosplat/pull/10) |
| Completed | Apple Silicon macOS Alpha | Provides an application-bundled FFmpeg, FFprobe, CPU COLMAP, and Brush workflow for macOS 15+ arm64. | [#4 Add macOS support](https://github.com/ooolabdev/ooosplat/issues/4), delivered by [PR #8](https://github.com/ooolabdev/ooosplat/pull/8) |
| Completed | Embedded Gaussian Splat preview | Supports `.ply` loading, camera navigation, whole-model transforms, undo/redo, animation preview, and non-destructive Gaussian and portrait-video export. | [#3 Embedded viewer](https://github.com/ooolabdev/ooosplat/issues/3) |
| Completed | Automatic COLMAP CUDA acceleration | Detects NVIDIA drivers and Compute Capability, automatically enables GPU feature extraction and matching when supported, and falls back to CPU without interrupting the task. | [#6 Add CUDA-accelerated pipeline](https://github.com/ooolabdev/ooosplat/issues/6); related feedback in [#2](https://github.com/ooolabdev/ooosplat/issues/2) |
| Completed · 0.4.0 | Stage-level pipeline resume and time estimation | Interrupted tasks validate and reuse frame, mask, feature, matching, and sparse-reconstruction checkpoints. Missing or damaged stages safely fall back for rerun, with generation-time estimates. | [PR #27](https://github.com/ooolabdev/ooosplat/pull/27) |
| Completed · 0.4.0 | Automatic masks for transparent media | Detects transparent MOV and PNG media, preserves RGBA data for Brush, and generates matching COLMAP masks to exclude transparent backgrounds. | [PR #19](https://github.com/ooolabdev/ooosplat/pull/19) and follow-up work |
| Completed · 0.4.0 | Image-sequence input | Unifies video and image input. Image sequences use a shared camera, exhaustive matching, and the incremental Mapper, with automatic masks for transparent PNG files. | [PR #19](https://github.com/ooolabdev/ooosplat/pull/19) |
| Completed · 0.4.0 | Gaussian region editing | Adds rectangle, sphere, and box selection, non-destructive deletion, crop freezing, shared undo/redo, and export to `edit.ply`. | Implemented in 0.4.0; issue to be created |

## Tracking and Contributions

The actual feature scope, technical discussion, and implementation status are governed by the linked GitHub Issues. Contributions, use cases, and technical feedback are welcome in the corresponding issue.

Items marked “To be created” do not yet have a dedicated issue. Once one exists, this page should be updated with its permanent issue number and link. Priorities may change as requirements and implementation constraints evolve; a priority change does not mean a feature has been cancelled.
