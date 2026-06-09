use super::*;
use crate::TurnContext;
use crate::config::Config;
use crate::session::turn_context::TurnEnvironment;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnAgentGitWorktreeArgs {
    pub(crate) branch: String,
    pub(crate) base: Option<String>,
    pub(crate) path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub(crate) struct SpawnAgentWorkspaceResult {
    pub(crate) path: String,
    pub(crate) branch: Option<String>,
    pub(crate) base: Option<String>,
    pub(crate) created: bool,
}

pub(crate) struct SpawnWorkspaceOutcome {
    pub(crate) workspace: Option<SpawnAgentWorkspaceResult>,
    pub(crate) environments: Vec<TurnEnvironmentSelection>,
}

pub(crate) async fn apply_spawn_workspace(
    config: &mut Config,
    turn: &TurnContext,
    workspace_path: Option<&Path>,
    git_worktree: Option<&SpawnAgentGitWorktreeArgs>,
) -> Result<SpawnWorkspaceOutcome, FunctionCallError> {
    if workspace_path.is_some() && git_worktree.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "Provide either workspace_path or git_worktree, but not both".to_string(),
        ));
    }

    let Some(workspace_target) =
        resolve_workspace_target(turn, workspace_path, git_worktree).await?
    else {
        return Ok(SpawnWorkspaceOutcome {
            workspace: None,
            environments: turn.environments.to_selections(),
        });
    };

    config.cwd = workspace_target.path.clone();
    config.workspace_roots = vec![workspace_target.path.clone()];
    config
        .permissions
        .set_workspace_roots(vec![workspace_target.path.clone()]);

    let environments = turn
        .environments
        .to_selections()
        .into_iter()
        .map(|selection| TurnEnvironmentSelection {
            cwd: workspace_target.path.clone(),
            ..selection
        })
        .collect();

    Ok(SpawnWorkspaceOutcome {
        workspace: Some(SpawnAgentWorkspaceResult {
            path: workspace_target.path.as_path().display().to_string(),
            branch: workspace_target.branch,
            base: workspace_target.base,
            created: workspace_target.created,
        }),
        environments,
    })
}

fn selected_turn_cwd(turn: &TurnContext) -> &AbsolutePathBuf {
    turn.environments
        .primary()
        .map_or(&turn.config.cwd, TurnEnvironment::cwd)
}

struct WorkspaceTarget {
    path: AbsolutePathBuf,
    branch: Option<String>,
    base: Option<String>,
    created: bool,
}

async fn resolve_workspace_target(
    turn: &TurnContext,
    workspace_path: Option<&Path>,
    git_worktree: Option<&SpawnAgentGitWorktreeArgs>,
) -> Result<Option<WorkspaceTarget>, FunctionCallError> {
    if let Some(workspace_path) = workspace_path {
        let path = resolve_spawn_path(workspace_path, turn)?;
        validate_existing_workspace_path(&path)?;
        validate_spawn_workspace_write(turn, &path)?;
        return Ok(Some(WorkspaceTarget {
            path,
            branch: None,
            base: None,
            created: false,
        }));
    }

    let Some(git_worktree) = git_worktree else {
        return Ok(None);
    };

    let branch = git_worktree.branch.trim();
    if branch.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "git_worktree.branch must not be empty".to_string(),
        ));
    }
    let base = git_worktree
        .base
        .as_deref()
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .unwrap_or("HEAD")
        .to_string();

    let selected_cwd = selected_turn_cwd(turn);
    let repo_root = git_stdout(selected_cwd.as_path(), &["rev-parse", "--show-toplevel"]).await?;
    let repo_root =
        AbsolutePathBuf::from_absolute_path(Path::new(repo_root.trim())).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "git rev-parse returned an invalid repository root: {err}"
            ))
        })?;
    git_status(
        repo_root.as_path(),
        &["check-ref-format", "--branch", branch],
    )
    .await?;

    let path = match git_worktree.path.as_deref() {
        Some(path) => resolve_spawn_path(path, turn)?,
        None => default_worktree_path(repo_root.as_path(), branch)?,
    };
    validate_spawn_workspace_write(turn, &path)?;
    validate_new_worktree_path(&path)?;
    git_status(repo_root.as_path(), {
        vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(branch),
            path.as_path().as_os_str().to_os_string(),
            OsString::from(&base),
        ]
    })
    .await?;

    Ok(Some(WorkspaceTarget {
        path,
        branch: Some(branch.to_string()),
        base: Some(base),
        created: true,
    }))
}

fn resolve_spawn_path(
    path: &Path,
    turn: &TurnContext,
) -> Result<AbsolutePathBuf, FunctionCallError> {
    let selected_cwd = selected_turn_cwd(turn);
    let resolved = AbsolutePathBuf::resolve_path_against_base(path, selected_cwd.as_path());
    Ok(resolved)
}

fn validate_existing_workspace_path(path: &AbsolutePathBuf) -> Result<(), FunctionCallError> {
    let metadata = std::fs::metadata(path.as_path()).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "workspace_path `{}` is not accessible: {err}",
            path.as_path().display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(FunctionCallError::RespondToModel(format!(
            "workspace_path `{}` is not a directory",
            path.as_path().display()
        )));
    }
    Ok(())
}

fn validate_new_worktree_path(path: &AbsolutePathBuf) -> Result<(), FunctionCallError> {
    if path.as_path().exists() {
        return Err(FunctionCallError::RespondToModel(format!(
            "git_worktree path `{}` already exists",
            path.as_path().display()
        )));
    }
    Ok(())
}

fn validate_spawn_workspace_write(
    turn: &TurnContext,
    path: &AbsolutePathBuf,
) -> Result<(), FunctionCallError> {
    let policy = turn.file_system_sandbox_policy();
    let selected_cwd = selected_turn_cwd(turn);
    let write_target = if path.as_path().exists() {
        path.as_path()
    } else {
        path.as_path().parent().unwrap_or_else(|| path.as_path())
    };
    if policy.can_write_path_with_cwd(write_target, selected_cwd.as_path()) {
        return Ok(());
    }

    Err(FunctionCallError::RespondToModel(format!(
        "spawn_agent workspace `{}` is outside the parent writable workspace roots",
        path.as_path().display()
    )))
}

fn default_worktree_path(
    repo_root: &Path,
    branch: &str,
) -> Result<AbsolutePathBuf, FunctionCallError> {
    let repo_name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("repo");
    let parent = repo_root.parent().ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "cannot derive a sibling worktree path for repository `{}`",
            repo_root.display()
        ))
    })?;
    AbsolutePathBuf::from_absolute_path(
        parent.join(format!("{repo_name}.{}", sanitize_path_component(branch))),
    )
    .map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to derive git_worktree path for branch `{branch}`: {err}"
        ))
    })
}

fn sanitize_path_component(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('-');
        }
    }
    sanitized.trim_matches('-').to_string()
}

async fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String, FunctionCallError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to run git {}: {err}",
                args.join(" ")
            ))
        })?;
    if !output.status.success() {
        return Err(git_error(args, &output));
    }
    String::from_utf8(output.stdout).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "git {} returned non-UTF8 stdout: {err}",
            args.join(" ")
        ))
    })
}

async fn git_status<I, S>(cwd: &Path, args: I) -> Result<(), FunctionCallError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let args: Vec<std::ffi::OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let output = Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to run git {}: {err}",
                display_args(&args)
            ))
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(git_error_os(&args, &output))
}

fn git_error(args: &[&str], output: &std::process::Output) -> FunctionCallError {
    let args: Vec<std::ffi::OsString> = args.iter().map(std::ffi::OsString::from).collect();
    git_error_os(&args, output)
}

fn git_error_os(args: &[std::ffi::OsString], output: &std::process::Output) -> FunctionCallError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    FunctionCallError::RespondToModel(format!("git {} failed: {detail}", display_args(args)))
}

fn display_args(args: &[std::ffi::OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
