//! CLI access to the daemon-owned durable project ledger.

use anyhow::{Result, bail};
use scv_client::Layout;
use scv_protocol::{DaemonCommand, ProjectRunStatus, ProjectTaskStatus};
use std::path::Path;

use super::{args::ProjectCommand, control};

pub(crate) async fn run(layout: &Layout, command: ProjectCommand, cwd: &Path) -> Result<()> {
    let command = match command {
        ProjectCommand::Create { name, workspace } => DaemonCommand::ProjectCreate {
            name,
            workspace: super::common::absolute_path(&workspace, cwd)
                .to_string_lossy()
                .into_owned(),
        },
        ProjectCommand::Status { project } => DaemonCommand::ProjectStatus { project },
        ProjectCommand::Events { project, after } => {
            DaemonCommand::ProjectEvents { project, after }
        }
        ProjectCommand::Tasks { project } => DaemonCommand::ProjectTasks { project },
        ProjectCommand::Report { project } => DaemonCommand::ProjectReport { project },
        ProjectCommand::AddTask {
            project,
            title,
            depends_on,
            max_retries,
        } => DaemonCommand::ProjectTaskAdd {
            project,
            title,
            depends_on,
            max_retries,
        },
        ProjectCommand::UpdateTask {
            project,
            task,
            status,
            progress,
        } => DaemonCommand::ProjectTaskUpdate {
            project,
            task,
            status: parse_task_status(&status)?,
            progress,
        },
        ProjectCommand::RunStart {
            project,
            task,
            agent,
        } => DaemonCommand::ProjectRunStart {
            project,
            task,
            agent,
        },
        ProjectCommand::RunProgress {
            project,
            run,
            progress,
        } => DaemonCommand::ProjectRunProgress {
            project,
            run,
            progress,
        },
        ProjectCommand::RunFinish {
            project,
            run,
            status,
        } => DaemonCommand::ProjectRunFinish {
            project,
            run,
            status: parse_run_status(&status)?,
        },
        ProjectCommand::Heartbeat { project, task, run } => {
            DaemonCommand::ProjectHeartbeat { project, task, run }
        }
    };
    let status = control(layout, command).await?;
    let Some(project) = status.project else {
        bail!("daemon returned no project response")
    };
    println!("{}", serde_json::to_string_pretty(&project)?);
    Ok(())
}

fn parse_task_status(value: &str) -> Result<ProjectTaskStatus> {
    match value {
        "pending" => Ok(ProjectTaskStatus::Pending),
        "ready" => Ok(ProjectTaskStatus::Ready),
        "running" => Ok(ProjectTaskStatus::Running),
        "blocked" => Ok(ProjectTaskStatus::Blocked),
        "done" => Ok(ProjectTaskStatus::Done),
        "failed" => Ok(ProjectTaskStatus::Failed),
        "stale" => Ok(ProjectTaskStatus::Stale),
        _ => bail!("unknown task status: {value}"),
    }
}

fn parse_run_status(value: &str) -> Result<ProjectRunStatus> {
    match value {
        "running" => Ok(ProjectRunStatus::Running),
        "succeeded" => Ok(ProjectRunStatus::Succeeded),
        "failed" => Ok(ProjectRunStatus::Failed),
        "cancelled" => Ok(ProjectRunStatus::Cancelled),
        "stale" => Ok(ProjectRunStatus::Stale),
        _ => bail!("unknown run status: {value}"),
    }
}
