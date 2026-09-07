pub mod catalog;
pub mod manager;
pub mod metadata;

pub use manager::{ProjectManager, ProjectPaths};
pub use metadata::{
    FrameState, GaussianCrop, GaussianEditing, GaussianTransform, PipelineStateFile,
    ProjectMetadata, ProjectOutput, ProjectStatus, PROJECT_APP_ID,
};
