//! Durable owner-created project ledger and its reducer.
//!
//! The ledger is deliberately evidence based: only explicit control calls and
//! observed run heartbeats/terminal states become events. Agent prose is never
//! used to infer progress, approvals, or deployment state.

use anyhow::{Context, Result, bail};
use scv_client::Layout;
use scv_protocol::{
    ProjectEvent, ProjectPhase, ProjectReport, ProjectResponse, ProjectRun, ProjectRunStatus,
    ProjectStatus, ProjectSummary, ProjectTask, ProjectTaskStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_NAME_BYTES: usize = 256;
const MAX_TITLE_BYTES: usize = 1024;
const COMPACTION_EVENT_LIMIT: usize = 512;
const MAX_EVENT_LOG_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const ORCHESTRATOR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const HEARTBEAT_TIMEOUT_SECONDS: u64 = 5 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectRecord {
    id: String,
    name: String,
    workspace: String,
    phase: ProjectPhase,
    status: ProjectStatus,
    created_unix_seconds: u64,
    updated_unix_seconds: u64,
    tasks: BTreeMap<String, ProjectTask>,
    runs: BTreeMap<String, ProjectRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectSnapshot {
    next_sequence: u64,
    projects: BTreeMap<String, ProjectRecord>,
}

/// The in-memory reducer backed by an append-only JSONL event log and periodic
/// private snapshots.
#[derive(Debug, Clone)]
pub(crate) struct ProjectLedger {
    path: PathBuf,
    snapshot_path: PathBuf,
    compacted_before: Option<u64>,
    next_sequence: u64,
    events: Vec<ProjectEvent>,
    projects: BTreeMap<String, ProjectRecord>,
}

impl ProjectLedger {
    pub(crate) fn open(layout: &Layout) -> Result<Self> {
        let directory = layout.projects();
        let path = directory.join("events.jsonl");
        let snapshot_path = directory.join("snapshot.json");
        match fs::symlink_metadata(&directory) {
            Ok(_) => secure_directory(&directory)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    snapshot_path,
                    compacted_before: None,
                    next_sequence: 1,
                    events: Vec::new(),
                    projects: BTreeMap::new(),
                });
            }
            Err(error) => return Err(error).context("inspect project ledger directory"),
        }
        if path.exists() {
            secure_file(&path)?;
        }
        let (next_sequence, projects, compacted_before) = if snapshot_path.exists() {
            secure_file(&snapshot_path)?;
            let mut snapshot_file = open_readonly(&snapshot_path)?;
            let mut snapshot_text = String::new();
            snapshot_file
                .read_to_string(&mut snapshot_text)
                .context("read project ledger snapshot")?;
            let snapshot: ProjectSnapshot =
                serde_json::from_str(&snapshot_text).context("decode project ledger snapshot")?;
            (
                snapshot.next_sequence,
                snapshot.projects,
                Some(snapshot.next_sequence.saturating_sub(1)),
            )
        } else {
            (1, BTreeMap::new(), None)
        };
        let mut text = String::new();
        if path.exists() {
            if fs::metadata(&path).context("stat project ledger")?.len() > MAX_EVENT_LOG_BYTES {
                bail!("project ledger event log exceeds its 64 MiB recovery limit")
            }
            let mut file = open_ledger(&path, false).context("open project ledger")?;
            file.read_to_string(&mut text)
                .context("read project ledger")?;
        }
        let mut ledger = Self {
            path,
            snapshot_path,
            compacted_before,
            next_sequence,
            events: Vec::new(),
            projects,
        };
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line.trim().is_empty() {
                continue;
            }
            let event = match serde_json::from_str::<ProjectEvent>(line) {
                Ok(event) => event,
                Err(error) if !text.ends_with('\n') && lines.peek().is_none() => {
                    tracing::warn!("ignoring truncated project ledger tail: {error}");
                    break;
                }
                Err(error) => return Err(error).context("decode project ledger event"),
            };
            if event.sequence < ledger.next_sequence {
                continue;
            }
            if event.sequence != ledger.next_sequence {
                bail!(
                    "project ledger sequence gap: expected {}, found {}",
                    ledger.next_sequence,
                    event.sequence
                )
            }
            ledger.next_sequence = ledger.next_sequence.saturating_add(1);
            ledger.apply(&event)?;
            ledger.events.push(event);
        }
        Ok(ledger)
    }

    pub(crate) fn execute(
        &mut self,
        command: scv_protocol::DaemonCommand,
    ) -> Result<ProjectResponse> {
        use scv_protocol::DaemonCommand;
        match command {
            DaemonCommand::ProjectCreate { name, workspace } => self.create(name, workspace),
            DaemonCommand::ProjectStatus { project } => Ok(ProjectResponse::Status {
                project: self.summary(&project)?,
            }),
            DaemonCommand::ProjectEvents { project, after } => {
                let id = self.resolve_id(&project)?;
                Ok(ProjectResponse::Events {
                    project_id: id.clone(),
                    compacted_before: self.compacted_before,
                    events: self
                        .events
                        .iter()
                        .filter(|event| {
                            event.project_id == id && after.is_none_or(|n| event.sequence > n)
                        })
                        .cloned()
                        .collect(),
                })
            }
            DaemonCommand::ProjectTasks { project } => {
                let id = self.resolve_id(&project)?;
                Ok(ProjectResponse::Tasks {
                    project_id: id.clone(),
                    tasks: self
                        .projects
                        .get(&id)
                        .expect("resolved project")
                        .tasks
                        .values()
                        .cloned()
                        .collect(),
                })
            }
            DaemonCommand::ProjectReport { project } => self.report(&project),
            DaemonCommand::ProjectTaskAdd {
                project,
                title,
                depends_on,
                max_retries,
            } => self.add_task(&project, title, depends_on, max_retries),
            DaemonCommand::ProjectTaskUpdate {
                project,
                task,
                status,
                progress,
            } => self.update_task(&project, &task, status, progress),
            DaemonCommand::ProjectRunStart {
                project,
                task,
                agent,
            } => self.run_start(&project, &task, agent),
            DaemonCommand::ProjectRunProgress {
                project,
                run,
                progress,
            } => self.run_progress(&project, &run, progress),
            DaemonCommand::ProjectRunFinish {
                project,
                run,
                status,
            } => self.run_finish(&project, &run, status),
            DaemonCommand::ProjectHeartbeat { project, task, run } => {
                self.heartbeat(&project, task.as_deref(), run.as_deref())
            }
            _ => bail!("not a project command"),
        }
    }

    pub(crate) fn has_projects(&self) -> bool {
        !self.projects.is_empty()
    }

    /// Mark runs that have stopped reporting as stale. This is deliberately
    /// evidence based and never starts or kills an agent process.
    pub(crate) fn reconcile_stale(&mut self, now: u64) -> Result<()> {
        let stale_runs: Vec<(String, String)> = self
            .projects
            .values()
            .flat_map(|project| {
                project
                    .runs
                    .values()
                    .filter(|run| {
                        run.status == ProjectRunStatus::Running
                            && now.saturating_sub(run.updated_unix_seconds)
                                > HEARTBEAT_TIMEOUT_SECONDS
                    })
                    .map(|run| (project.id.clone(), run.id.clone()))
            })
            .collect();
        for (project, run) in stale_runs {
            self.commit(project, "run.stale", json!({"run": run}))?;
        }
        Ok(())
    }

    fn create(&mut self, name: String, workspace: String) -> Result<ProjectResponse> {
        validate_text(&name, MAX_NAME_BYTES, "project name")?;
        let workspace_path = Path::new(&workspace);
        if !workspace_path.is_absolute() || !workspace_path.is_dir() {
            bail!("project workspace must be an existing absolute directory")
        }
        if self.projects.values().any(|project| project.name == name) {
            bail!("a project with that name already exists")
        }
        let now = now();
        let id = Uuid::new_v4().to_string();
        self.commit(
            id.clone(),
            "project.created",
            json!({"name": name, "workspace": workspace, "created_unix_seconds": now}),
        )?;
        Ok(ProjectResponse::Created {
            project: self.summary(&id)?,
        })
    }

    fn add_task(
        &mut self,
        project: &str,
        title: String,
        depends_on: Vec<String>,
        max_retries: u32,
    ) -> Result<ProjectResponse> {
        validate_text(&title, MAX_TITLE_BYTES, "task title")?;
        let id = self.resolve_id(project)?;
        if depends_on
            .iter()
            .any(|dependency| depends_on.iter().filter(|item| *item == dependency).count() > 1)
        {
            bail!("task dependencies must be unique")
        }
        for dependency in &depends_on {
            if !self
                .projects
                .get(&id)
                .expect("resolved project")
                .tasks
                .contains_key(dependency)
            {
                bail!("task dependency does not exist: {dependency}")
            }
        }
        let task = Uuid::new_v4().to_string();
        self.commit(id.clone(), "task.created", json!({"task": task, "title": title, "depends_on": depends_on, "max_retries": max_retries}))?;
        Ok(ProjectResponse::TaskAdded {
            project: self.summary(&id)?,
            task_id: task,
        })
    }

    fn update_task(
        &mut self,
        project: &str,
        task: &str,
        status: ProjectTaskStatus,
        progress: Option<String>,
    ) -> Result<ProjectResponse> {
        let id = self.resolve_id(project)?;
        let current = self
            .projects
            .get(&id)
            .expect("resolved project")
            .tasks
            .get(task)
            .ok_or_else(|| anyhow::anyhow!("unknown project task"))?
            .status;
        if status == ProjectTaskStatus::Running && current != ProjectTaskStatus::Running {
            bail!("start a project run before marking a task running")
        }
        if matches!(
            current,
            ProjectTaskStatus::Done | ProjectTaskStatus::Failed | ProjectTaskStatus::Stale
        ) && status != current
        {
            bail!("project task is terminal")
        }
        if let Some(progress) = &progress {
            validate_text(progress, MAX_TITLE_BYTES, "task progress")?;
        }
        self.commit(
            id.clone(),
            "task.updated",
            json!({"task": task, "status": status, "progress": progress}),
        )?;
        Ok(ProjectResponse::Updated {
            project: self.summary(&id)?,
        })
    }

    fn run_start(&mut self, project: &str, task: &str, agent: String) -> Result<ProjectResponse> {
        validate_text(&agent, 128, "agent name")?;
        let id = self.resolve_id(project)?;
        let record = self.projects.get(&id).expect("resolved project");
        let task_record = record
            .tasks
            .get(task)
            .ok_or_else(|| anyhow::anyhow!("unknown project task"))?;
        if task_record.status != ProjectTaskStatus::Ready {
            bail!("project task is not ready")
        }
        if record
            .runs
            .values()
            .any(|run| run.task_id == task && run.status == ProjectRunStatus::Running)
        {
            bail!("project task already has a running run")
        }
        let run = Uuid::new_v4().to_string();
        self.commit(
            id.clone(),
            "run.started",
            json!({"run": run, "task": task, "agent": agent, "started_unix_seconds": now()}),
        )?;
        Ok(ProjectResponse::RunStarted {
            project: self.summary(&id)?,
            run_id: run,
        })
    }

    fn run_progress(
        &mut self,
        project: &str,
        run: &str,
        progress: String,
    ) -> Result<ProjectResponse> {
        validate_text(&progress, MAX_TITLE_BYTES, "run progress")?;
        let id = self.resolve_id(project)?;
        let run_record = self
            .projects
            .get(&id)
            .expect("resolved project")
            .runs
            .get(run)
            .ok_or_else(|| anyhow::anyhow!("unknown project run"))?;
        if run_record.status != ProjectRunStatus::Running {
            bail!("project run is not running")
        }
        self.commit(
            id.clone(),
            "run.progress",
            json!({"run": run, "progress": progress, "timestamp_unix_seconds": now()}),
        )?;
        Ok(ProjectResponse::Updated {
            project: self.summary(&id)?,
        })
    }

    fn run_finish(
        &mut self,
        project: &str,
        run: &str,
        status: ProjectRunStatus,
    ) -> Result<ProjectResponse> {
        let id = self.resolve_id(project)?;
        let run_record = self
            .projects
            .get(&id)
            .expect("resolved project")
            .runs
            .get(run)
            .ok_or_else(|| anyhow::anyhow!("unknown project run"))?;
        if run_record.status != ProjectRunStatus::Running {
            bail!("project run is not running")
        }
        if status == ProjectRunStatus::Running {
            bail!("a running run must use progress or heartbeat")
        }
        self.commit(
            id.clone(),
            "run.finished",
            json!({"run": run, "status": status, "timestamp_unix_seconds": now()}),
        )?;
        Ok(ProjectResponse::Updated {
            project: self.summary(&id)?,
        })
    }

    fn heartbeat(
        &mut self,
        project: &str,
        task: Option<&str>,
        run: Option<&str>,
    ) -> Result<ProjectResponse> {
        let id = self.resolve_id(project)?;
        let record = self.projects.get(&id).expect("resolved project");
        if let Some(task) = task
            && !record.tasks.contains_key(task)
        {
            bail!("unknown project task")
        }
        if let Some(run) = run
            && !record.runs.contains_key(run)
        {
            bail!("unknown project run")
        }
        self.commit(
            id.clone(),
            "heartbeat",
            json!({"task": task, "run": run, "timestamp_unix_seconds": now()}),
        )?;
        Ok(ProjectResponse::Updated {
            project: self.summary(&id)?,
        })
    }

    fn report(&self, project: &str) -> Result<ProjectResponse> {
        let id = self.resolve_id(project)?;
        let record = self.projects.get(&id).expect("resolved project");
        let summary = self.summary(&id)?;
        let mut evidence = Vec::new();
        if let Some(last) = self
            .events
            .iter()
            .rev()
            .find(|event| event.project_id == id)
        {
            evidence.push(format!("last event #{}: {}", last.sequence, last.kind));
        }
        for task in record
            .tasks
            .values()
            .filter(|task| task.status == ProjectTaskStatus::Stale)
        {
            evidence.push(format!(
                "task {} is stale because its heartbeat is old or absent",
                task.id
            ));
        }
        Ok(ProjectResponse::Report {
            report: ProjectReport {
                project: summary,
                tasks: record.tasks.values().cloned().collect(),
                runs: record.runs.values().cloned().collect(),
                generated_unix_seconds: now(),
                evidence,
            },
        })
    }

    fn summary(&self, project: &str) -> Result<ProjectSummary> {
        let id = self.resolve_id(project)?;
        let record = self.projects.get(&id).expect("resolved project");
        Ok(ProjectSummary {
            id: record.id.clone(),
            name: record.name.clone(),
            workspace: record.workspace.clone(),
            phase: record.phase,
            status: record.status,
            created_unix_seconds: record.created_unix_seconds,
            updated_unix_seconds: record.updated_unix_seconds,
            task_count: record.tasks.len() as u64,
            completed_tasks: record
                .tasks
                .values()
                .filter(|task| task.status == ProjectTaskStatus::Done)
                .count() as u64,
            last_heartbeat_unix_seconds: self
                .events
                .iter()
                .rev()
                .find(|event| {
                    event.project_id == id
                        && matches!(
                            event.kind.as_str(),
                            "heartbeat" | "run.progress" | "run.started"
                        )
                })
                .and_then(|event| event.data.get("timestamp_unix_seconds"))
                .and_then(Value::as_u64),
        })
    }

    fn resolve_id(&self, value: &str) -> Result<String> {
        if self.projects.contains_key(value) {
            return Ok(value.to_owned());
        }
        self.projects
            .values()
            .find(|project| project.name == value)
            .map(|project| project.id.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown project: {value}"))
    }

    fn commit(&mut self, project_id: String, kind: &str, data: Value) -> Result<()> {
        self.ensure_storage()?;
        let event = ProjectEvent {
            sequence: self.next_sequence,
            project_id,
            kind: kind.to_owned(),
            timestamp_unix_seconds: now(),
            data,
        };
        let mut next = self.clone();
        next.apply(&event)?;
        let mut line = serde_json::to_vec(&event).context("encode project event")?;
        line.push(b'\n');
        let mut file = open_ledger(&self.path, false).context("open project ledger for append")?;
        let original_len = file.metadata().context("stat project ledger")?.len();
        if let Err(error) = file.write_all(&line) {
            let _ = file.set_len(original_len);
            return Err(error).context("append project ledger event");
        }
        if let Err(error) = file.sync_data() {
            let _ = file.set_len(original_len);
            return Err(error).context("sync project ledger event");
        }
        next.next_sequence = next.next_sequence.saturating_add(1);
        next.events.push(event);
        *self = next;
        self.compact_if_needed()?;
        Ok(())
    }

    fn ensure_storage(&self) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("project ledger has no parent directory")?;
        fs::create_dir_all(directory).context("create project ledger directory")?;
        secure_directory(directory)?;
        if !self.path.exists() {
            open_ledger(&self.path, true).context("create project ledger")?;
        }
        secure_file(&self.path)
    }

    fn compact_if_needed(&mut self) -> Result<()> {
        if self.events.len() < COMPACTION_EVENT_LIMIT {
            return Ok(());
        }
        let snapshot = ProjectSnapshot {
            next_sequence: self.next_sequence,
            projects: self.projects.clone(),
        };
        let bytes = serde_json::to_vec(&snapshot).context("encode project ledger snapshot")?;
        let temporary = self
            .snapshot_path
            .with_extension(format!("tmp-{}", Uuid::new_v4()));
        {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .context("create project ledger snapshot")?;
            file.write_all(&bytes)
                .context("write project ledger snapshot")?;
            file.sync_data().context("sync project ledger snapshot")?;
        }
        secure_file(&temporary)?;
        fs::rename(&temporary, &self.snapshot_path).context("install project ledger snapshot")?;
        sync_directory(self.snapshot_path.parent().expect("snapshot has a parent"))?;
        let file = open_ledger(&self.path, false)?;
        file.set_len(0)
            .context("compact project ledger event log")?;
        file.sync_data()
            .context("sync compacted project ledger event log")?;
        self.compacted_before = Some(self.next_sequence.saturating_sub(1));
        self.events.clear();
        Ok(())
    }

    fn apply(&mut self, event: &ProjectEvent) -> Result<()> {
        let data = &event.data;
        match event.kind.as_str() {
            "project.created" => {
                let name = data
                    .get("name")
                    .and_then(Value::as_str)
                    .context("project.created name")?;
                let workspace = data
                    .get("workspace")
                    .and_then(Value::as_str)
                    .context("project.created workspace")?;
                let created = data
                    .get("created_unix_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(event.timestamp_unix_seconds);
                self.projects.insert(
                    event.project_id.clone(),
                    ProjectRecord {
                        id: event.project_id.clone(),
                        name: name.into(),
                        workspace: workspace.into(),
                        phase: ProjectPhase::Discovery,
                        status: ProjectStatus::Active,
                        created_unix_seconds: created,
                        updated_unix_seconds: event.timestamp_unix_seconds,
                        tasks: BTreeMap::new(),
                        runs: BTreeMap::new(),
                    },
                );
            }
            "task.created" => {
                let project = self
                    .projects
                    .get_mut(&event.project_id)
                    .context("task for unknown project")?;
                let task = data
                    .get("task")
                    .and_then(Value::as_str)
                    .context("task.created id")?;
                let depends_on: Vec<String> = serde_json::from_value(
                    data.get("depends_on").cloned().unwrap_or_else(|| json!([])),
                )
                .context("task dependencies")?;
                project.tasks.insert(
                    task.into(),
                    ProjectTask {
                        id: task.into(),
                        title: data
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        status: if depends_on.is_empty() {
                            ProjectTaskStatus::Ready
                        } else {
                            ProjectTaskStatus::Pending
                        },
                        depends_on,
                        retries: 0,
                        max_retries: data.get("max_retries").and_then(Value::as_u64).unwrap_or(0)
                            as u32,
                        run_id: None,
                        progress: None,
                        last_heartbeat_unix_seconds: None,
                    },
                );
                project.updated_unix_seconds = event.timestamp_unix_seconds;
            }
            "task.updated" => {
                let project = self
                    .projects
                    .get_mut(&event.project_id)
                    .context("task for unknown project")?;
                let task_id = data
                    .get("task")
                    .and_then(Value::as_str)
                    .context("task.updated id")?;
                let task = project
                    .tasks
                    .get_mut(task_id)
                    .context("task.updated unknown task")?;
                task.status =
                    serde_json::from_value(data.get("status").cloned().context("task status")?)
                        .context("task status value")?;
                task.progress = data
                    .get("progress")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                project.updated_unix_seconds = event.timestamp_unix_seconds;
            }
            "run.started" => {
                let project = self
                    .projects
                    .get_mut(&event.project_id)
                    .context("run for unknown project")?;
                let run = data
                    .get("run")
                    .and_then(Value::as_str)
                    .context("run.started id")?;
                let task_id = data
                    .get("task")
                    .and_then(Value::as_str)
                    .context("run.started task")?;
                let task = project.tasks.get_mut(task_id).context("run task unknown")?;
                task.status = ProjectTaskStatus::Running;
                task.run_id = Some(run.into());
                task.last_heartbeat_unix_seconds = Some(event.timestamp_unix_seconds);
                let attempt = task.retries.saturating_add(1);
                project.runs.insert(
                    run.into(),
                    ProjectRun {
                        id: run.into(),
                        task_id: task_id.into(),
                        agent: data
                            .get("agent")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        status: ProjectRunStatus::Running,
                        attempt,
                        started_unix_seconds: data
                            .get("started_unix_seconds")
                            .and_then(Value::as_u64)
                            .unwrap_or(event.timestamp_unix_seconds),
                        updated_unix_seconds: event.timestamp_unix_seconds,
                        progress: None,
                    },
                );
                project.updated_unix_seconds = event.timestamp_unix_seconds;
            }
            "run.progress" => {
                let project = self
                    .projects
                    .get_mut(&event.project_id)
                    .context("run for unknown project")?;
                let run_id = data
                    .get("run")
                    .and_then(Value::as_str)
                    .context("run.progress id")?;
                let run = project
                    .runs
                    .get_mut(run_id)
                    .context("run.progress unknown run")?;
                run.progress = data
                    .get("progress")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                run.updated_unix_seconds = event.timestamp_unix_seconds;
                if let Some(task) = project.tasks.get_mut(&run.task_id) {
                    task.progress = run.progress.clone();
                    task.last_heartbeat_unix_seconds = Some(event.timestamp_unix_seconds);
                }
                project.updated_unix_seconds = event.timestamp_unix_seconds;
            }
            "run.finished" => {
                let project = self
                    .projects
                    .get_mut(&event.project_id)
                    .context("run for unknown project")?;
                let run_id = data
                    .get("run")
                    .and_then(Value::as_str)
                    .context("run.finished id")?;
                let status: ProjectRunStatus =
                    serde_json::from_value(data.get("status").cloned().context("run status")?)
                        .context("run status value")?;
                let run = project
                    .runs
                    .get_mut(run_id)
                    .context("run.finished unknown run")?;
                run.status = status;
                run.updated_unix_seconds = event.timestamp_unix_seconds;
                if let Some(task) = project.tasks.get_mut(&run.task_id) {
                    task.status = if status == ProjectRunStatus::Succeeded {
                        ProjectTaskStatus::Done
                    } else if status == ProjectRunStatus::Failed {
                        task.retries = task.retries.saturating_add(1);
                        if task.retries > task.max_retries {
                            ProjectTaskStatus::Failed
                        } else {
                            ProjectTaskStatus::Ready
                        }
                    } else if status == ProjectRunStatus::Stale {
                        ProjectTaskStatus::Stale
                    } else {
                        ProjectTaskStatus::Ready
                    };
                    task.run_id = None;
                }
                project.updated_unix_seconds = event.timestamp_unix_seconds;
            }
            "run.stale" => {
                let project = self
                    .projects
                    .get_mut(&event.project_id)
                    .context("stale run for unknown project")?;
                let run_id = data
                    .get("run")
                    .and_then(Value::as_str)
                    .context("run.stale id")?;
                let run = project
                    .runs
                    .get_mut(run_id)
                    .context("run.stale unknown run")?;
                run.status = ProjectRunStatus::Stale;
                run.updated_unix_seconds = event.timestamp_unix_seconds;
                if let Some(task) = project.tasks.get_mut(&run.task_id) {
                    task.status = ProjectTaskStatus::Stale;
                    task.run_id = None;
                }
                project.updated_unix_seconds = event.timestamp_unix_seconds;
            }
            "heartbeat" => {
                let project = self
                    .projects
                    .get_mut(&event.project_id)
                    .context("heartbeat for unknown project")?;
                if let Some(task_id) = data.get("task").and_then(Value::as_str)
                    && let Some(task) = project.tasks.get_mut(task_id)
                {
                    task.last_heartbeat_unix_seconds = Some(event.timestamp_unix_seconds);
                }
                if let Some(run_id) = data.get("run").and_then(Value::as_str)
                    && let Some(run) = project.runs.get_mut(run_id)
                {
                    run.updated_unix_seconds = event.timestamp_unix_seconds;
                }
                project.updated_unix_seconds = event.timestamp_unix_seconds;
            }
            kind => bail!("unknown project ledger event: {kind}"),
        }
        self.refresh_project(&event.project_id, event.timestamp_unix_seconds);
        Ok(())
    }

    fn refresh_project(&mut self, project_id: &str, timestamp: u64) {
        let Some(project) = self.projects.get_mut(project_id) else {
            return;
        };
        let done: std::collections::BTreeSet<String> = project
            .tasks
            .values()
            .filter(|task| task.status == ProjectTaskStatus::Done)
            .map(|task| task.id.clone())
            .collect();
        let failed: std::collections::BTreeSet<String> = project
            .tasks
            .values()
            .filter(|task| {
                matches!(
                    task.status,
                    ProjectTaskStatus::Failed | ProjectTaskStatus::Stale
                )
            })
            .map(|task| task.id.clone())
            .collect();
        for task in project.tasks.values_mut() {
            if task.status == ProjectTaskStatus::Pending {
                if task
                    .depends_on
                    .iter()
                    .any(|dependency| failed.contains(dependency))
                {
                    task.status = ProjectTaskStatus::Blocked;
                } else if task
                    .depends_on
                    .iter()
                    .all(|dependency| done.contains(dependency))
                {
                    task.status = ProjectTaskStatus::Ready;
                }
            }
        }
        let all_done = !project.tasks.is_empty()
            && project
                .tasks
                .values()
                .all(|task| task.status == ProjectTaskStatus::Done);
        let any_failed = project.tasks.values().any(|task| {
            matches!(
                task.status,
                ProjectTaskStatus::Failed | ProjectTaskStatus::Stale
            )
        });
        let any_running = project
            .tasks
            .values()
            .any(|task| task.status == ProjectTaskStatus::Running);
        let any_ready = project
            .tasks
            .values()
            .any(|task| task.status == ProjectTaskStatus::Ready);
        project.status = if all_done {
            ProjectStatus::Completed
        } else if any_failed {
            ProjectStatus::Failed
        } else if !any_running && !any_ready && !project.tasks.is_empty() {
            ProjectStatus::Blocked
        } else {
            ProjectStatus::Active
        };
        project.phase = if all_done {
            ProjectPhase::Reporting
        } else if any_running {
            ProjectPhase::Implementation
        } else if any_failed {
            ProjectPhase::Verification
        } else if project.tasks.is_empty() {
            ProjectPhase::Discovery
        } else {
            ProjectPhase::Planning
        };
        project.updated_unix_seconds = project.updated_unix_seconds.max(timestamp);
    }
}

fn validate_text(value: &str, max: usize, label: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        bail!("invalid {label}")
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn secure_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("inspect project ledger directory")?;
    if !metadata.file_type().is_dir() {
        bail!("project ledger path is not a directory")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn secure_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("inspect project ledger")?;
    if !metadata.file_type().is_file() {
        bail!("project ledger is not a regular file")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn open_ledger(path: &Path, create: bool) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).append(!create);
    if create {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(Into::into)
}

fn open_readonly(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(Into::into)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .context("open project ledger directory")?
            .sync_all()
            .context("sync project ledger directory")?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests;
