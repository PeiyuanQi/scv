//! What SCV started, so it can list, stop, and clean up delegated agents.
//!
//! Every delegated process is tagged through its environment
//! (`SCV_PARENT=<instance>/<session>/<handle>`, chained through nested SCVs,
//! and `SCV_DELEGATION_DEPTH`) and recorded in
//! `$SCV_HOME/run/delegations/<handle>.json` while it runs. A record whose
//! owning SCV process died is an orphan: the daemon's reconciliation kills its
//! process group and anything still carrying its tag, then removes it.
//!
//! This is cooperative bookkeeping. Delegated agents run as the user, so one
//! that deliberately clears its environment or leaves its process group can
//! escape it; the tags and records exist to clean up accidental leaks.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Environment variable carrying the delegation chain.
pub const PARENT_VARIABLE: &str = "SCV_PARENT";
/// Environment variable carrying how deeply this process is delegated.
pub const DEPTH_VARIABLE: &str = "SCV_DELEGATION_DEPTH";
/// Grace between TERM and KILL when stopping delegated processes.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// Largest record file read.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// A zombie child younger than this may still be awaited by its spawner.
const ZOMBIE_MIN_AGE: Duration = Duration::from_secs(10);

/// Delegation depth of the current process: 0 unless an SCV started it.
pub fn current_depth() -> u32 {
    std::env::var(DEPTH_VARIABLE)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// A process, identified by PID plus start time so a reused PID never matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

impl ProcessIdentity {
    pub fn current() -> Option<Self> {
        Self::of(std::process::id())
    }

    pub fn of(pid: u32) -> Option<Self> {
        process_start_time(pid).map(|start_time| Self { pid, start_time })
    }

    /// Whether this exact process still runs. An exited process that its
    /// parent has not yet collected (a zombie) does not count.
    pub fn is_alive(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            linux::stat(self.pid)
                .is_some_and(|info| info.start_time == self.start_time && info.state != 'Z')
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::of(self.pid) == Some(*self)
        }
    }
}

/// One delegated run, as recorded on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRecord {
    pub handle: String,
    pub agent: String,
    pub instance: String,
    pub session: String,
    pub owner: ProcessIdentity,
    /// The agent process, which leads its own process group.
    pub process: ProcessIdentity,
    pub pgid: u32,
    pub cwd: PathBuf,
    pub started_unix: u64,
    /// Depth of the delegated process (the owner's depth plus one).
    pub depth: u32,
}

/// A record plus what SCV currently observes about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationEntry {
    pub record: DelegationRecord,
    /// The owning SCV process is gone; reconciliation will clean it up.
    pub orphaned: bool,
    /// Live processes in its group plus tagged processes outside it.
    pub processes: usize,
}

/// What a reconciliation pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Orphaned delegations whose processes were stopped.
    pub reaped: Vec<String>,
    /// Orphaned records whose processes had already exited.
    pub removed: usize,
}

#[derive(Debug, Default)]
struct Inner {
    active: HashMap<String, Arc<AtomicBool>>,
    reaped: u64,
}

/// The delegations one SCV process started, backed by the instance's records.
#[derive(Debug)]
pub struct DelegationRegistry {
    record_dir: PathBuf,
    instance: String,
    owner: Option<ProcessIdentity>,
    depth: u32,
    chain: Option<String>,
    inner: Mutex<Inner>,
}

/// A delegation about to start: its handle and the environment tagging it.
pub(crate) struct PendingDelegation {
    pub handle: String,
    pub environment: Vec<(OsString, OsString)>,
    agent: String,
    session: String,
    cwd: PathBuf,
}

impl DelegationRegistry {
    /// The registry for the SCV instance rooted at `instance_home`.
    pub fn new(instance_home: &Path) -> Self {
        let digest = Sha256::digest(instance_home.as_os_str().as_encoded_bytes());
        let instance = digest[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            record_dir: instance_home.join("run").join("delegations"),
            instance,
            owner: ProcessIdentity::current(),
            depth: current_depth(),
            chain: std::env::var(PARENT_VARIABLE)
                .ok()
                .filter(|value| !value.trim().is_empty()),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// This process's own delegation depth.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn record_dir(&self) -> &Path {
        &self.record_dir
    }

    /// Short identifier of the SCV instance, shared by all its processes.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Delegations this process stopped as orphans since it started.
    pub fn reaped_total(&self) -> u64 {
        self.inner.lock().expect("registry lock").reaped
    }

    pub(crate) fn begin(&self, agent: &str, session: &str, cwd: &Path) -> PendingDelegation {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let handle = format!("{agent}-{}", &suffix[..6]);
        let entry = format!("{}/{session}/{handle}", self.instance);
        let chain = match &self.chain {
            Some(chain) => format!("{chain};{entry}"),
            None => entry,
        };
        PendingDelegation {
            environment: vec![
                (PARENT_VARIABLE.into(), chain.into()),
                (DEPTH_VARIABLE.into(), (self.depth + 1).to_string().into()),
            ],
            handle,
            agent: agent.to_owned(),
            session: session.to_owned(),
            cwd: cwd.to_owned(),
        }
    }

    /// Record a spawned delegation. The returned guard removes the record and
    /// stops leftovers when the run ends, even if the run is abandoned.
    pub(crate) fn register(
        self: &Arc<Self>,
        pending: PendingDelegation,
        pid: u32,
    ) -> std::io::Result<DelegationGuard> {
        let killed = Arc::new(AtomicBool::new(false));
        let record = DelegationRecord {
            handle: pending.handle.clone(),
            agent: pending.agent,
            instance: self.instance.clone(),
            session: pending.session,
            owner: self.owner.unwrap_or(ProcessIdentity {
                pid: std::process::id(),
                start_time: 0,
            }),
            process: ProcessIdentity::of(pid).unwrap_or(ProcessIdentity { pid, start_time: 0 }),
            pgid: pid,
            cwd: pending.cwd,
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            depth: self.depth + 1,
        };
        self.inner
            .lock()
            .expect("registry lock")
            .active
            .insert(record.handle.clone(), Arc::clone(&killed));
        if let Err(error) = write_record(&self.record_dir, &record) {
            self.inner
                .lock()
                .expect("registry lock")
                .active
                .remove(&record.handle);
            return Err(error);
        }
        Ok(DelegationGuard {
            registry: Arc::clone(self),
            handle: record.handle,
            pgid: pid,
            killed,
            finished: false,
        })
    }

    /// Delegations of this instance that are still running. With
    /// `include_orphans`, also records whose owner died and await cleanup.
    pub fn list(&self, include_orphans: bool) -> Vec<DelegationEntry> {
        let table = ProcessTable::snapshot();
        let mut entries: Vec<_> = self
            .records()
            .into_iter()
            .filter_map(|record| {
                let orphaned = !self.owner_alive(&record);
                if orphaned && !include_orphans {
                    return None;
                }
                let processes = table.members(&record).len();
                Some(DelegationEntry {
                    record,
                    orphaned,
                    processes,
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            a.record
                .started_unix
                .cmp(&b.record.started_unix)
                .then_with(|| a.record.handle.cmp(&b.record.handle))
        });
        entries
    }

    /// Stop one delegation of this instance, whichever process owns it.
    pub async fn kill(&self, handle: &str) -> Result<(), String> {
        let record = self
            .records()
            .into_iter()
            .find(|record| record.handle == handle)
            .ok_or_else(|| format!("no running delegation {handle:?}"))?;
        let local = self
            .inner
            .lock()
            .expect("registry lock")
            .active
            .get(handle)
            .cloned();
        if let Some(killed) = &local {
            killed.store(true, Ordering::Release);
        }
        stop_delegation(&record).await;
        if local.is_none() && !self.owner_alive(&record) {
            remove_record(&self.record_dir, handle);
            self.inner.lock().expect("registry lock").reaped += 1;
        }
        Ok(())
    }

    /// Stop and remove every orphaned delegation of this instance.
    pub async fn reconcile(&self) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        for record in self.records() {
            if self.owner_alive(&record) {
                continue;
            }
            if stop_delegation(&record).await {
                report.reaped.push(record.handle.clone());
            } else {
                report.removed += 1;
            }
            remove_record(&self.record_dir, &record.handle);
        }
        self.inner.lock().expect("registry lock").reaped += report.reaped.len() as u64;
        report
    }

    /// Whether the process that owns `record` still runs it. A record this
    /// process owns counts only while its run is active here.
    fn owner_alive(&self, record: &DelegationRecord) -> bool {
        if Some(record.owner) == self.owner {
            return self
                .inner
                .lock()
                .expect("registry lock")
                .active
                .contains_key(&record.handle);
        }
        record.owner.is_alive()
    }

    fn records(&self) -> Vec<DelegationRecord> {
        let Ok(entries) = std::fs::read_dir(&self.record_dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| read_record(&entry.path()))
            .filter(|record| record.instance == self.instance)
            .collect()
    }

    fn finish_local(&self, handle: &str) {
        self.inner
            .lock()
            .expect("registry lock")
            .active
            .remove(handle);
        remove_record(&self.record_dir, handle);
    }
}

/// Keeps a delegation recorded while it runs.
pub(crate) struct DelegationGuard {
    registry: Arc<DelegationRegistry>,
    handle: String,
    pgid: u32,
    killed: Arc<AtomicBool>,
    finished: bool,
}

impl DelegationGuard {
    #[cfg(test)]
    pub(crate) fn handle(&self) -> &str {
        &self.handle
    }

    /// Whether `scv agents kill` stopped this run.
    pub(crate) fn was_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    /// The run ended: stop anything still tagged with it, then forget it.
    pub(crate) async fn finish(mut self) {
        self.finished = true;
        stop_tagged(&self.handle).await;
        self.registry.finish_local(&self.handle);
    }
}

impl Drop for DelegationGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The run was abandoned mid-flight: kill its group now and sweep
        // tagged leftovers in the background.
        signal_group(self.pgid, libc::SIGKILL);
        self.registry.finish_local(&self.handle);
        let handle = self.handle.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { stop_tagged(&handle).await });
        } else {
            for identity in tagged_processes(&handle) {
                signal(identity.pid, libc::SIGKILL);
            }
        }
    }
}

/// Stop a delegation's process group and tagged processes: TERM, then KILL
/// after a short grace. Returns whether anything was still running.
async fn stop_delegation(record: &DelegationRecord) -> bool {
    let mut stopped = false;
    // The group ID is the leader's PID, which the kernel does not reuse while
    // the group has members. A live leader with a different start time means
    // the PID was reused, so the group is not ours.
    let leader = ProcessIdentity::of(record.process.pid);
    let group_is_ours = record.pgid == record.process.pid
        && match leader {
            Some(leader) => leader == record.process,
            None => group_exists(record.pgid),
        };
    if group_is_ours && group_exists(record.pgid) {
        stopped = true;
        signal_group(record.pgid, libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        while group_exists(record.pgid) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        signal_group(record.pgid, libc::SIGKILL);
    }
    stopped | stop_tagged(&record.handle).await
}

/// TERM, then KILL, every process tagged with `handle`. Returns whether any was found.
async fn stop_tagged(handle: &str) -> bool {
    let tagged = tagged_processes(handle);
    if tagged.is_empty() {
        return false;
    }
    for identity in &tagged {
        signal(identity.pid, libc::SIGTERM);
    }
    let deadline = tokio::time::Instant::now() + STOP_GRACE;
    while tagged.iter().any(ProcessIdentity::is_alive) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for identity in tagged.iter().filter(|identity| identity.is_alive()) {
        signal(identity.pid, libc::SIGKILL);
    }
    true
}

/// Processes whose `SCV_PARENT` chain names `handle`.
fn tagged_processes(handle: &str) -> Vec<ProcessIdentity> {
    let own = std::process::id();
    ProcessTable::snapshot()
        .tagged
        .into_iter()
        .filter(|(identity, chain)| identity.pid != own && chain_names(chain, handle))
        .map(|(identity, _)| identity)
        .collect()
}

fn chain_names(chain: &str, handle: &str) -> bool {
    chain
        .split(';')
        .any(|entry| entry.rsplit('/').next() == Some(handle))
}

fn signal(pid: u32, signal: i32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 0
    {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

fn signal_group(pgid: u32, signal: i32) {
    // Never address group 0 or 1 (this process's own group, or init's).
    if let Ok(pgid) = i32::try_from(pgid)
        && pgid > 1
    {
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

/// Whether the process group still has a running member (zombies excluded on Linux).
fn group_exists(pgid: u32) -> bool {
    let Ok(group) = i32::try_from(pgid) else {
        return false;
    };
    if group <= 1 {
        return false;
    }
    let result = unsafe { libc::kill(-group, 0) };
    let signalable =
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    #[cfg(target_os = "linux")]
    {
        signalable
            && linux::all_stats()
                .iter()
                .any(|info| info.pgid == pgid && info.state != 'Z')
    }
    #[cfg(not(target_os = "linux"))]
    {
        signalable
    }
}

fn write_record(dir: &Path, record: &DelegationRecord) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    std::fs::create_dir_all(dir)?;
    if let Some(run) = dir.parent() {
        std::fs::set_permissions(run, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    let temporary = dir.join(format!(".{}.json.tmp", record.handle));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, dir.join(format!("{}.json", record.handle)))
}

fn read_record(path: &Path) -> Option<DelegationRecord> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, MAX_RECORD_BYTES), &mut bytes)
        .ok()?;
    let record: DelegationRecord = serde_json::from_slice(&bytes).ok()?;
    // Only a record named after its own handle is trusted.
    (path.file_stem().and_then(|stem| stem.to_str()) == Some(record.handle.as_str()))
        .then_some(record)
}

fn remove_record(dir: &Path, handle: &str) {
    let _ = std::fs::remove_file(dir.join(format!("{handle}.json")));
}

/// Make this process the reaper of orphaned descendants (Linux), so processes
/// a delegated agent leaves behind stay in SCV's process tree.
pub fn become_child_subreaper() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

static SPAWNED: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

/// Note a child this process spawned and will wait for itself.
pub(crate) fn track_spawned(pid: u32) {
    SPAWNED
        .lock()
        .expect("spawned lock")
        .get_or_insert_with(HashSet::new)
        .insert(pid);
}

pub(crate) fn untrack_spawned(pid: u32) {
    if let Some(spawned) = SPAWNED.lock().expect("spawned lock").as_mut() {
        spawned.remove(&pid);
    }
}

/// Collect exited orphans reparented to this subreaper. Children SCV spawned
/// itself are left to their own waiters.
pub fn reap_orphaned_zombies() -> usize {
    #[cfg(target_os = "linux")]
    {
        let own = std::process::id();
        let spawned = SPAWNED
            .lock()
            .expect("spawned lock")
            .clone()
            .unwrap_or_default();
        let uptime = linux::uptime_ticks();
        let mut reaped = 0;
        for info in linux::all_stats() {
            if info.ppid != own || info.state != 'Z' || spawned.contains(&info.pid) {
                continue;
            }
            let old_enough = uptime.is_some_and(|now| {
                now.saturating_sub(info.start_time)
                    >= ZOMBIE_MIN_AGE.as_secs() * linux::clock_ticks()
            });
            if !old_enough {
                continue;
            }
            let mut status = 0;
            if unsafe { libc::waitpid(info.pid as i32, &mut status, libc::WNOHANG) }
                == info.pid as i32
            {
                reaped += 1;
            }
        }
        reaped
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Processes of interest at one moment: group membership and tags.
struct ProcessTable {
    groups: Vec<(ProcessIdentity, u32)>,
    tagged: Vec<(ProcessIdentity, String)>,
}

impl ProcessTable {
    fn members(&self, record: &DelegationRecord) -> HashSet<u32> {
        let mut members: HashSet<u32> = self
            .groups
            .iter()
            .filter(|(_, pgid)| *pgid == record.pgid)
            .map(|(identity, _)| identity.pid)
            .collect();
        members.extend(
            self.tagged
                .iter()
                .filter(|(_, chain)| chain_names(chain, &record.handle))
                .map(|(identity, _)| identity.pid),
        );
        members
    }

    #[cfg(target_os = "linux")]
    fn snapshot() -> Self {
        let mut groups = Vec::new();
        let mut tagged = Vec::new();
        for info in linux::all_stats() {
            if info.state == 'Z' {
                continue;
            }
            let identity = ProcessIdentity {
                pid: info.pid,
                start_time: info.start_time,
            };
            groups.push((identity, info.pgid));
            if let Some(chain) = linux::parent_chain(info.pid) {
                tagged.push((identity, chain));
            }
        }
        Self { groups, tagged }
    }

    #[cfg(not(target_os = "linux"))]
    fn snapshot() -> Self {
        let mut groups = Vec::new();
        let mut tagged = Vec::new();
        // `ps -E` appends each process's environment to its command line.
        let Ok(output) = std::process::Command::new("ps")
            .args(["-E", "-ww", "-axo", "pid=,pgid=,command="])
            .output()
        else {
            return Self { groups, tagged };
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(pgid)) = (
                fields.next().and_then(|value| value.parse::<u32>().ok()),
                fields.next().and_then(|value| value.parse::<u32>().ok()),
            ) else {
                continue;
            };
            let Some(identity) = ProcessIdentity::of(pid) else {
                continue;
            };
            groups.push((identity, pgid));
            if let Some(chain) = fields.find_map(|field| {
                field
                    .strip_prefix(PARENT_VARIABLE)
                    .and_then(|rest| rest.strip_prefix('='))
            }) {
                tagged.push((identity, chain.to_owned()));
            }
        }
        Self { groups, tagged }
    }
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    linux::stat(pid).map(|info| info.start_time)
}

#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start_time(pid: u32) -> Option<u64> {
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
    alive.then_some(0)
}

#[cfg(target_os = "linux")]
mod linux {
    pub(super) struct Stat {
        pub pid: u32,
        pub ppid: u32,
        pub pgid: u32,
        pub state: char,
        pub start_time: u64,
    }

    pub(super) fn stat(pid: u32) -> Option<Stat> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name is parenthesized and may contain spaces or ')'.
        let rest = &text[text.rfind(')')? + 2..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // After the name: state(3) ppid(4) pgrp(5) ... starttime(22).
        Some(Stat {
            pid,
            state: fields.first()?.chars().next()?,
            ppid: fields.get(1)?.parse().ok()?,
            pgid: fields.get(2)?.parse().ok()?,
            start_time: fields.get(19)?.parse().ok()?,
        })
    }

    pub(super) fn all_stats() -> Vec<Stat> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(stat)
            .collect()
    }

    /// `SCV_PARENT` from a process's environment, when readable.
    pub(super) fn parent_chain(pid: u32) -> Option<String> {
        let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let prefix = format!("{}=", super::PARENT_VARIABLE);
        environ.split(|byte| *byte == 0).find_map(|entry| {
            entry
                .strip_prefix(prefix.as_bytes())
                .map(|value| String::from_utf8_lossy(value).into_owned())
        })
    }

    pub(super) fn clock_ticks() -> u64 {
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        u64::try_from(ticks)
            .ok()
            .filter(|ticks| *ticks > 0)
            .unwrap_or(100)
    }

    pub(super) fn uptime_ticks() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/uptime").ok()?;
        let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
        Some((seconds * clock_ticks() as f64) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt as _, process::CommandExt as _};

    fn registry(home: &Path) -> Arc<DelegationRegistry> {
        Arc::new(DelegationRegistry::new(home))
    }

    /// Spawn `sh -c script` in its own process group with `environment`.
    fn spawn_tagged(script: &str, environment: &[(OsString, OsString)]) -> std::process::Child {
        std::process::Command::new("sh")
            .args(["-c", script])
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    async fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    #[test]
    fn chains_match_only_their_own_handle() {
        assert!(chain_names("abcd/s1/codex-1a2b3c", "codex-1a2b3c"));
        assert!(chain_names(
            "x/s/claude-000000;abcd/s1/codex-1a2b3c",
            "codex-1a2b3c"
        ));
        assert!(!chain_names("abcd/s1/codex-1a2b3c", "codex-1a2b3"));
        assert!(!chain_names("abcd/s1/codex-1a2b3c", "1a2b3c"));
    }

    #[test]
    fn nested_tags_extend_the_chain_and_depth() {
        let home = tempfile::tempdir().unwrap();
        let mut registry = DelegationRegistry::new(home.path());
        registry.chain = Some("aaaa/s0/codex-111111".into());
        registry.depth = 1;
        let pending = registry.begin("claude", "s1", home.path());
        let value = |name: &str| {
            pending
                .environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_str().unwrap().to_owned())
                .unwrap()
        };
        assert_eq!(
            value(PARENT_VARIABLE),
            format!(
                "aaaa/s0/codex-111111;{}/s1/{}",
                registry.instance, pending.handle
            )
        );
        assert_eq!(value(DEPTH_VARIABLE), "2");
        assert!(pending.handle.starts_with("claude-"));
    }

    #[tokio::test]
    async fn records_are_private_and_removed_when_the_run_finishes() {
        let home = tempfile::tempdir().unwrap();
        let registry = registry(home.path());
        let pending = registry.begin("codex", "session", home.path());
        let environment = pending.environment.clone();
        let mut child = spawn_tagged("sleep 30", &environment);
        let guard = registry.register(pending, child.id()).unwrap();
        let path = registry
            .record_dir()
            .join(format!("{}.json", guard.handle()));
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(registry.record_dir()), 0o700);
        assert_eq!(mode(registry.record_dir().parent().unwrap()), 0o700);

        let listed = registry.list(false);
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].orphaned);
        assert_eq!(listed[0].record.process.pid, child.id());
        assert!(listed[0].processes >= 1);

        signal_group(child.id(), libc::SIGKILL);
        child.wait().unwrap();
        guard.finish().await;
        assert!(!path.exists());
        assert!(registry.list(true).is_empty());
    }

    #[tokio::test]
    async fn kill_stops_a_local_run_and_marks_it_killed() {
        let home = tempfile::tempdir().unwrap();
        let registry = registry(home.path());
        let pending = registry.begin("claude", "session", home.path());
        let environment = pending.environment.clone();
        let mut child = spawn_tagged("trap '' TERM; sleep 30", &environment);
        let guard = registry.register(pending, child.id()).unwrap();
        registry.kill(guard.handle()).await.unwrap();
        assert!(guard.was_killed());
        assert!(child.wait().unwrap().code().is_none(), "killed by a signal");
        assert!(registry.kill("claude-nosuch").await.is_err());
        guard.finish().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reconcile_reaps_an_orphan_and_its_detached_descendants() {
        let home = tempfile::tempdir().unwrap();
        let owner = registry(home.path());
        let pending = owner.begin("codex", "session", home.path());
        let environment = pending.environment.clone();
        // The agent starts a detached descendant in a new session, outside its group.
        let mut child = spawn_tagged("setsid sleep 60 & exec sleep 60", &environment);
        let guard = owner.register(pending, child.id()).unwrap();
        let handle = guard.handle().to_owned();
        assert!(wait_for(|| tagged_processes(&handle).len() >= 2).await);
        let path = owner.record_dir().join(format!("{handle}.json"));
        // Rewrite the record as if a process that has since died owned it.
        let mut record = read_record(&path).unwrap();
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let gone_pid = gone.id();
        gone.wait().unwrap();
        record.owner = ProcessIdentity {
            pid: gone_pid,
            start_time: 1,
        };
        write_record(owner.record_dir(), &record).unwrap();
        std::mem::forget(guard);

        // Another SCV process of the same instance reconciles.
        let daemon = registry(home.path());
        assert!(daemon.list(false).is_empty());
        let orphans = daemon.list(true);
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].orphaned);
        assert!(orphans[0].processes >= 2);
        let report = daemon.reconcile().await;
        assert_eq!(report.reaped, vec![handle.clone()]);
        assert_eq!(daemon.reaped_total(), 1);
        assert!(child.wait().unwrap().code().is_none());
        assert!(wait_for(|| tagged_processes(&handle).is_empty()).await);
        assert!(!path.exists());
        assert_eq!(daemon.reconcile().await, ReconcileReport::default());
    }

    #[tokio::test]
    async fn an_abandoned_run_is_cleaned_up_when_its_guard_drops() {
        let home = tempfile::tempdir().unwrap();
        let registry = registry(home.path());
        let pending = registry.begin("pi", "session", home.path());
        let environment = pending.environment.clone();
        let mut child = spawn_tagged("sleep 30", &environment);
        let guard = registry.register(pending, child.id()).unwrap();
        let path = registry
            .record_dir()
            .join(format!("{}.json", guard.handle()));
        drop(guard);
        assert!(child.wait().unwrap().code().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn records_for_another_instance_or_under_the_wrong_name_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let registry = DelegationRegistry::new(home.path());
        let dir = registry.record_dir().to_owned();
        let record = DelegationRecord {
            handle: "codex-abcdef".into(),
            agent: "codex".into(),
            instance: "other".into(),
            session: "s".into(),
            owner: ProcessIdentity {
                pid: 1,
                start_time: 1,
            },
            process: ProcessIdentity {
                pid: 1,
                start_time: 1,
            },
            pgid: 1,
            cwd: "/".into(),
            started_unix: 0,
            depth: 1,
        };
        write_record(&dir, &record).unwrap();
        std::fs::copy(
            dir.join("codex-abcdef.json"),
            dir.join("codex-renamed.json"),
        )
        .unwrap();
        assert!(registry.list(true).is_empty());
    }
}
