use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENT_GUARD_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let dirs = ProjectDirs::from("dev", "agent-guard", "agent-guard")
        .context("could not resolve the agent-guard data directory")?;
    Ok(dirs.data_local_dir().to_path_buf())
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENT_GUARD_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    let dirs = ProjectDirs::from("dev", "agent-guard", "agent-guard")
        .context("could not resolve the agent-guard config directory")?;
    Ok(dirs.config_dir().to_path_buf())
}

pub fn policy_path(workspace: &Path, explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        return Ok(Some(path.to_path_buf()));
    }
    let project = workspace.join(".agent-guard/policy.yaml");
    if project.exists() {
        return Ok(Some(project));
    }
    let global = config_dir()?.join("policy.yaml");
    Ok(global.exists().then_some(global))
}
