use super::*;
use scv_protocol::{
    DaemonCommand, ProjectResponse, ProjectRunStatus, ProjectStatus, ProjectTaskStatus,
};
use tempfile::TempDir;

fn ledger() -> (TempDir, TempDir, ProjectLedger) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let ledger = ProjectLedger::open(&Layout::new(home.path())).unwrap();
    (home, workspace, ledger)
}

fn project_id(response: ProjectResponse) -> String {
    match response {
        ProjectResponse::Created { project } => project.id,
        _ => panic!("expected created response"),
    }
}

fn task_id(response: ProjectResponse) -> String {
    match response {
        ProjectResponse::TaskAdded { task_id, .. } => task_id,
        _ => panic!("expected task response"),
    }
}

fn run_id(response: ProjectResponse) -> String {
    match response {
        ProjectResponse::RunStarted { run_id, .. } => run_id,
        _ => panic!("expected run response"),
    }
}

#[test]
fn project_events_replay_and_dependencies_advance() {
    let (home, workspace, mut ledger) = ledger();
    let project = project_id(
        ledger
            .execute(DaemonCommand::ProjectCreate {
                name: "demo".into(),
                workspace: workspace.path().display().to_string(),
            })
            .unwrap(),
    );
    let first = task_id(
        ledger
            .execute(DaemonCommand::ProjectTaskAdd {
                project: project.clone(),
                title: "first".into(),
                depends_on: vec![],
                max_retries: 0,
            })
            .unwrap(),
    );
    let second = task_id(
        ledger
            .execute(DaemonCommand::ProjectTaskAdd {
                project: project.clone(),
                title: "second".into(),
                depends_on: vec![first.clone()],
                max_retries: 0,
            })
            .unwrap(),
    );
    let run = run_id(
        ledger
            .execute(DaemonCommand::ProjectRunStart {
                project: project.clone(),
                task: first.clone(),
                agent: "fake".into(),
            })
            .unwrap(),
    );
    ledger
        .execute(DaemonCommand::ProjectRunFinish {
            project: project.clone(),
            run,
            status: ProjectRunStatus::Succeeded,
        })
        .unwrap();
    let ProjectResponse::Tasks { tasks, .. } = ledger
        .execute(DaemonCommand::ProjectTasks {
            project: project.clone(),
        })
        .unwrap()
    else {
        panic!("expected tasks")
    };
    assert_eq!(
        tasks.iter().find(|task| task.id == second).unwrap().status,
        ProjectTaskStatus::Ready
    );

    let mut reopened = ProjectLedger::open(&Layout::new(home.path())).unwrap();
    let ProjectResponse::Events { events, .. } = reopened
        .execute(DaemonCommand::ProjectEvents {
            project,
            after: None,
        })
        .unwrap()
    else {
        panic!("expected events")
    };
    assert!(events.len() >= 5);
    assert!(
        events
            .windows(2)
            .all(|window| window[0].sequence < window[1].sequence)
    );
}

#[test]
fn failed_run_retries_until_the_budget_is_exhausted() {
    let (_home, workspace, mut ledger) = ledger();
    let project = project_id(
        ledger
            .execute(DaemonCommand::ProjectCreate {
                name: "retry".into(),
                workspace: workspace.path().display().to_string(),
            })
            .unwrap(),
    );
    let task = task_id(
        ledger
            .execute(DaemonCommand::ProjectTaskAdd {
                project: project.clone(),
                title: "retry me".into(),
                depends_on: vec![],
                max_retries: 1,
            })
            .unwrap(),
    );
    let run = run_id(
        ledger
            .execute(DaemonCommand::ProjectRunStart {
                project: project.clone(),
                task: task.clone(),
                agent: "fake".into(),
            })
            .unwrap(),
    );
    ledger
        .execute(DaemonCommand::ProjectRunFinish {
            project: project.clone(),
            run,
            status: ProjectRunStatus::Failed,
        })
        .unwrap();
    let ProjectResponse::Status { project: summary } = ledger
        .execute(DaemonCommand::ProjectStatus {
            project: project.clone(),
        })
        .unwrap()
    else {
        panic!("expected status")
    };
    assert_eq!(summary.status, ProjectStatus::Active);

    let run = run_id(
        ledger
            .execute(DaemonCommand::ProjectRunStart {
                project: project.clone(),
                task,
                agent: "fake".into(),
            })
            .unwrap(),
    );
    ledger
        .execute(DaemonCommand::ProjectRunFinish {
            project: project.clone(),
            run,
            status: ProjectRunStatus::Failed,
        })
        .unwrap();
    let ProjectResponse::Status { project: summary } = ledger
        .execute(DaemonCommand::ProjectStatus { project })
        .unwrap()
    else {
        panic!("expected status")
    };
    assert_eq!(summary.status, ProjectStatus::Failed);
}

#[test]
fn compaction_keeps_state_and_reports_the_omitted_event_boundary() {
    let (home, workspace, mut ledger) = ledger();
    let project = project_id(
        ledger
            .execute(DaemonCommand::ProjectCreate {
                name: "compact".into(),
                workspace: workspace.path().display().to_string(),
            })
            .unwrap(),
    );
    for index in 0..511 {
        ledger
            .execute(DaemonCommand::ProjectTaskAdd {
                project: project.clone(),
                title: format!("task-{index}"),
                depends_on: vec![],
                max_retries: 0,
            })
            .unwrap();
    }
    assert!(home.path().join("state/projects/snapshot.json").is_file());
    let ProjectResponse::Events {
        events,
        compacted_before,
        ..
    } = ledger
        .execute(DaemonCommand::ProjectEvents {
            project: project.clone(),
            after: None,
        })
        .unwrap()
    else {
        panic!("expected events")
    };
    assert!(events.is_empty());
    assert_eq!(compacted_before, Some(512));

    let mut reopened = ProjectLedger::open(&Layout::new(home.path())).unwrap();
    let ProjectResponse::Status { project: summary } = reopened
        .execute(DaemonCommand::ProjectStatus { project })
        .unwrap()
    else {
        panic!("expected status")
    };
    assert_eq!(summary.task_count, 511);
}

#[test]
fn newline_terminated_corruption_fails_closed_but_a_partial_tail_is_recovered() {
    let home = tempfile::tempdir().unwrap();
    let path = Layout::new(home.path()).projects().join("events.jsonl");
    let _ = ProjectLedger::open(&Layout::new(home.path())).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not-json\n").unwrap();
    assert!(ProjectLedger::open(&Layout::new(home.path())).is_err());
    std::fs::write(&path, b"not-json").unwrap();
    assert!(ProjectLedger::open(&Layout::new(home.path())).is_ok());
}

#[cfg(unix)]
#[test]
fn ledger_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let (home, workspace, mut ledger) = ledger();
    ledger
        .execute(DaemonCommand::ProjectCreate {
            name: "private".into(),
            workspace: workspace.path().display().to_string(),
        })
        .unwrap();
    let directory = home.path().join("state/projects");
    let file_mode = std::fs::metadata(directory.join("events.jsonl"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let directory_mode = std::fs::metadata(directory).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600);
    assert_eq!(directory_mode, 0o700);
}
