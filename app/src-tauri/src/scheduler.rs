//! M3.2 / M3.4 background indexing coordinator.
//!
//! One coordinator thread owns the source schedule. File-system events only
//! stamp a source-local quiet period; the coordinator turns that into a queued
//! scan. Local `auto` sources use a `notify` watcher (with periodic rechecks),
//! every other automatic source uses its configured poll interval. Source-level
//! scan jobs are claimed from SQLite and run on short-lived worker threads so
//! the coordinator itself never blocks on extraction or OCR.
//!
//! Every database operation and every indexing job opens the database while
//! holding the read lease on `DatabaseLocation`. The lease is held for the whole
//! job, which keeps `VACUUM INTO` migrations from observing a moving database.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use indexfox_cli::pipeline::{index_registered_source, IndexReport};
use indexfox_core::{ResourceMode, SourceKind, SourceScanConfig};
use indexfox_index::{IndexDb, IndexError, IndexJobRecord, SourceRecord};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;

use crate::storage::DatabaseLocation;

/// Application-owned database with the migration lease.
pub type Database = Arc<RwLock<DatabaseLocation>>;

/// Per-source event coalescing window.
const QUIET_WINDOW: Duration = Duration::from_millis(750);
/// Retry a failed watcher backend occasionally instead of giving up forever.
const WATCH_RETRY: Duration = Duration::from_secs(60);
/// Upper bound for one coordinator sleep; wakes still interrupt it immediately.
const MAX_WAIT: Duration = Duration::from_millis(500);
/// Poll for newly queued/persisted sources when no source is due.
const IDLE_WAIT: Duration = Duration::from_secs(2);
/// Granularity for re-checking cancellation while paused.
const PAUSE_POLL: Duration = Duration::from_millis(200);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn concurrency(mode: ResourceMode) -> usize {
    match mode {
        ResourceMode::Saver => 1,
        ResourceMode::Balanced => 2,
        ResourceMode::Fast => 4,
    }
}

fn parse_mode(value: &str) -> Option<ResourceMode> {
    let value = value.trim().trim_matches('"').to_ascii_lowercase();
    match value.as_str() {
        "saver" => Some(ResourceMode::Saver),
        "balanced" => Some(ResourceMode::Balanced),
        "fast" => Some(ResourceMode::Fast),
        _ => None,
    }
}

/// Serialized runtime status returned by the IPC layer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub paused: bool,
    pub resource_mode: ResourceMode,
    pub active: Vec<ActiveJobSnapshot>,
    pub queued: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveJobSnapshot {
    pub job_id: i64,
    pub source_id: i64,
    pub root_path: String,
    pub processed: usize,
    pub total: usize,
    pub indexed: usize,
    pub failed: usize,
}

struct WaitState {
    generation: u64,
}

struct Exclusions {
    db_path: PathBuf,
}

impl Exclusions {
    fn new(db_path: &Path) -> Self {
        Self {
            db_path: db_path.to_path_buf(),
        }
    }

    /// A custom database can share a directory with user documents.
    fn matches(&self, path: &Path) -> bool {
        if path.parent() != self.db_path.parent() {
            return false;
        }
        if path == self.db_path {
            return true;
        }
        let (Some(candidate), Some(db_name)) = (path.file_name(), self.db_path.file_name()) else {
            return false;
        };
        let candidate = candidate.to_string_lossy();
        let db_name = db_name.to_string_lossy();
        let page = candidate.as_ref();
        page == db_name.as_ref()
            || page == format!("{db_name}-wal").as_str()
            || page == format!("{db_name}-shm").as_str()
            || page == format!("{db_name}-journal").as_str()
    }
}

#[derive(Clone)]
struct WatchEntry {
    path: PathBuf,
    source_id: i64,
    recursive: bool,
}

struct ActiveJob {
    job_id: i64,
    source_id: i64,
    root_path: String,
    processed: usize,
    total: usize,
    indexed: usize,
    failed: usize,
    control: Arc<JobControl>,
}

struct JobControl {
    cancelled: AtomicBool,
    inner: std::sync::Weak<Inner>,
}

impl JobControl {
    fn new(inner: &Arc<Inner>) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            inner: Arc::downgrade(inner),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(inner) = self.inner.upgrade() {
            inner.wake();
        }
    }

    /// `should_stop` is both the pause gate and the cancellation signal. While
    /// paused it waits here; cancellation and shutdown return `true`.
    fn should_stop(&self) -> bool {
        if self.cancelled.load(Ordering::SeqCst) {
            return true;
        }
        match self.inner.upgrade() {
            Some(inner) => inner.should_stop_while_paused(&self.cancelled),
            None => true,
        }
    }
}

#[derive(Debug)]
enum Outcome {
    Completed,
    Failed(String),
    Cancelled,
}

struct Completion {
    source_id: i64,
    job_id: i64,
    attempt_count: i64,
    outcome: Outcome,
}

struct PendingJob {
    job: IndexJobRecord,
    control: Arc<JobControl>,
}

struct Schedule {
    id: i64,
    root_path: String,
    kind: SourceKind,
    scan_mode: String,
    enabled: bool,
    offline: bool,
    recursive: bool,
    interval: Duration,
    next_poll: Instant,
    retry_after: Option<Instant>,
    /// Failed job to requeue instead of creating a fresh one, preserving the
    /// attempt count. `(job_id, attempt_count, last_error)`.
    retry_job: Option<(i64, i64, String)>,
    watch_ok: bool,
    watch_retry_at: Option<Instant>,
}

struct Inner {
    database: Database,
    started: AtomicBool,
    paused: AtomicBool,
    shutdown: AtomicBool,
    mode_explicit: AtomicBool,
    wait: Mutex<WaitState>,
    wait_cv: Condvar,
    mode: Mutex<ResourceMode>,
    active: Mutex<BTreeMap<i64, ActiveJob>>,
    queued: AtomicUsize,
    last_error: Mutex<Option<String>>,
    events: Mutex<HashMap<i64, Instant>>,
    watches: Mutex<Vec<WatchEntry>>,
    exclusions: Mutex<Exclusions>,
    job_gate: Mutex<()>,
    pending_cancels: Mutex<HashSet<i64>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    fn with_database<T>(
        &self,
        operation: impl FnOnce(&mut IndexDb) -> Result<T, String>,
    ) -> Result<T, String> {
        let location = self.database.read().unwrap_or_else(PoisonError::into_inner);
        let mut db = IndexDb::open(location.path()).map_err(|error| {
            format!(
                "cannot open index database {}: {error}",
                location.path().display()
            )
        })?;
        operation(&mut db)
    }

    fn try_with_database<T>(
        &self,
        operation: impl FnOnce(&mut IndexDb) -> Result<T, String>,
    ) -> Result<T, String> {
        let location = self
            .database
            .try_read()
            .map_err(|_| "database is busy (migration in progress)".to_string())?;
        let mut db = IndexDb::open(location.path()).map_err(|error| {
            format!(
                "cannot open index database {}: {error}",
                location.path().display()
            )
        })?;
        operation(&mut db)
    }

    fn wake(&self) {
        let mut state = lock(&self.wait);
        state.generation = state.generation.wrapping_add(1);
        self.wait_cv.notify_all();
    }

    fn wait_for_change(&self, seen: &mut u64, timeout: Option<Duration>) {
        let state = lock(&self.wait);
        if state.generation != *seen {
            *seen = state.generation;
            return;
        }
        let state = match timeout {
            Some(timeout) => {
                self.wait_cv
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => self
                .wait_cv
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner),
        };
        *seen = state.generation;
    }

    fn should_stop_while_paused(&self, cancelled: &AtomicBool) -> bool {
        if self.shutdown.load(Ordering::SeqCst) {
            return true;
        }
        if !self.paused.load(Ordering::SeqCst) {
            return false;
        }
        let mut state = lock(&self.wait);
        loop {
            if cancelled.load(Ordering::SeqCst) || self.shutdown.load(Ordering::SeqCst) {
                return true;
            }
            if !self.paused.load(Ordering::SeqCst) {
                return false;
            }
            state = self
                .wait_cv
                .wait_timeout(state, PAUSE_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn set_last_error(&self, message: impl Into<String>) {
        *lock(&self.last_error) = Some(message.into());
    }

    fn update_progress(&self, job_id: i64, report: &IndexReport) {
        let mut active = lock(&self.active);
        let Some(job) = active.get_mut(&job_id) else {
            return;
        };
        let processed = report
            .indexed
            .saturating_add(report.unchanged)
            .saturating_add(report.skipped_size)
            .saturating_add(report.failed);
        job.processed = processed;
        job.total = report.scanned.max(processed);
        job.indexed = report.indexed;
        job.failed = report.failed;
    }

    fn finish_job(&self, job_id: i64) {
        lock(&self.active).remove(&job_id);
    }

    fn handle_event(&self, event: Event) {
        if matches!(&event.kind, EventKind::Access(_)) {
            return;
        }
        let exclusions = lock(&self.exclusions);
        let watches = lock(&self.watches);
        let mut hit: Option<(usize, i64)> = None;
        for path in &event.paths {
            if exclusions.matches(path) {
                continue;
            }
            for watch in watches.iter() {
                if path.starts_with(&watch.path) {
                    let depth = watch.path.as_os_str().len();
                    if hit.map_or(true, |(best, _)| depth > best) {
                        hit = Some((depth, watch.source_id));
                    }
                }
            }
        }
        let Some((_, source_id)) = hit else {
            return;
        };
        lock(&self.events).insert(source_id, Instant::now());
        self.wake();
    }
}

/// Background coordinator. `new` only wires shared state; `start` launches the
/// single coordinator thread and can be called repeatedly (it starts once).
pub struct IndexScheduler {
    inner: Arc<Inner>,
}

impl IndexScheduler {
    pub fn new(database: Database) -> Self {
        Self {
            inner: Arc::new(Inner {
                database,
                started: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                mode_explicit: AtomicBool::new(false),
                wait: Mutex::new(WaitState { generation: 0 }),
                wait_cv: Condvar::new(),
                mode: Mutex::new(ResourceMode::Balanced),
                active: Mutex::new(BTreeMap::new()),
                queued: AtomicUsize::new(0),
                last_error: Mutex::new(None),
                events: Mutex::new(HashMap::new()),
                watches: Mutex::new(Vec::new()),
                exclusions: Mutex::new(Exclusions::new(Path::new(""))),
                job_gate: Mutex::new(()),
                pending_cancels: Mutex::new(HashSet::new()),
                thread: Mutex::new(None),
            }),
        }
    }

    pub fn start(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let inner = Arc::clone(&self.inner);
        let spawned = thread::Builder::new()
            .name("indexfox-scheduler".to_string())
            .spawn(move || coordinator_loop(inner, sender, receiver));
        match spawned {
            Ok(handle) => {
                *lock(&self.inner.thread) = Some(handle);
            }
            Err(error) => {
                self.inner.started.store(false, Ordering::SeqCst);
                self.inner
                    .set_last_error(format!("cannot start index coordinator: {error}"));
            }
        }
    }

    /// Signal cancellation and wake the coordinator. The handle is detached,
    /// not joined: shutdown must not wait for a blocked file-system operation.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.wake();
        drop(lock(&self.inner.thread).take());
    }

    pub fn wake(&self) {
        self.inner.wake();
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.paused.store(paused, Ordering::SeqCst);
        self.inner.wake();
    }

    pub fn set_mode(&self, mode: ResourceMode) {
        self.inner.mode_explicit.store(true, Ordering::SeqCst);
        *lock(&self.inner.mode) = mode;
        self.inner.wake();
    }

    /// Cancel one job. A queued job is deleted immediately. A running job is
    /// only signalled; the worker keeps the old index and drops the job row
    /// after the pipeline returns. The short gate closes the claim/register
    /// race so a running job cannot be mistaken for a queued one.
    pub fn cancel(&self, job_id: i64) -> Result<(), String> {
        let inner = &self.inner;
        let _gate = lock(&inner.job_gate);
        let state = match inner.try_with_database(|db| job_state(db, job_id)) {
            Ok(state) => state,
            Err(busy) => {
                if let Some(job) = lock(&inner.active).get_mut(&job_id) {
                    job.control.cancel();
                    return Ok(());
                }
                return Err(busy);
            }
        };
        let result = match state.as_deref() {
            None => Ok(()),
            Some("queued") => inner.with_database(|db| {
                db.delete_index_job(job_id)
                    .map_err(|error| format!("cannot delete queued job {job_id}: {error}"))
            }),
            Some("running") => {
                if let Some(job) = lock(&inner.active).get_mut(&job_id) {
                    job.control.cancel();
                } else {
                    // Claimed but not registered yet, or started by another
                    // process. Remember it so registration does not swallow it.
                    lock(&inner.pending_cancels).insert(job_id);
                }
                Ok(())
            }
            Some(_) => Ok(()),
        };
        inner.wake();
        result
    }

    pub fn remove_source(&self, source_id: i64) -> Result<(), String> {
        let inner = &self.inner;
        let _gate = lock(&inner.job_gate);
        for job in lock(&inner.active)
            .values()
            .filter(|job| job.source_id == source_id)
        {
            job.control.cancel();
        }

        // shortcut: a blocked network scan may need more time to exit, retry once it stops.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = 0;
        while lock(&inner.active)
            .values()
            .any(|job| job.source_id == source_id)
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("目录的索引任务正在停止，请稍后重试移除。".into());
            }
            inner.wait_for_change(&mut seen, Some(remaining));
        }
        inner.try_with_database(|db| {
            db.delete_source(source_id)
                .map_err(|error| error.to_string())
        })?;
        inner.wake();
        Ok(())
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let active = lock(&self.inner.active)
            .values()
            .map(|job| ActiveJobSnapshot {
                job_id: job.job_id,
                source_id: job.source_id,
                root_path: job.root_path.clone(),
                processed: job.processed,
                total: job.total,
                indexed: job.indexed,
                failed: job.failed,
            })
            .collect();
        RuntimeSnapshot {
            paused: self.inner.paused.load(Ordering::SeqCst),
            resource_mode: *lock(&self.inner.mode),
            active,
            queued: self.inner.queued.load(Ordering::SeqCst),
            last_error: lock(&self.inner.last_error).clone(),
        }
    }
}

fn job_state(db: &IndexDb, job_id: i64) -> Result<Option<String>, String> {
    let mut statement = db
        .conn()
        .prepare("SELECT state FROM index_jobs WHERE id = ?1")
        .map_err(|error| error.to_string())?;
    let mut rows = statement
        .query([job_id])
        .map_err(|error| error.to_string())?;
    rows.next()
        .and_then(|row| row.map(|row| row.get(0)).transpose())
        .map_err(|error| error.to_string())
}

fn source_scan_config(source: &SourceRecord) -> (bool, u64) {
    let config = SourceScanConfig::from_json(&source.config_json);
    let poll = config.poll_interval_secs.clamp(5, 86_400);
    (config.recursive, poll)
}

fn automatic(schedule: &Schedule) -> bool {
    schedule.enabled && schedule.scan_mode != "manual"
}

fn wants_watch(schedule: &Schedule) -> bool {
    schedule.enabled
        && !schedule.offline
        && schedule.scan_mode == "auto"
        && matches!(schedule.kind, SourceKind::Local)
}

fn event_capable(schedule: &Schedule) -> bool {
    wants_watch(schedule) && schedule.watch_ok
}

fn reconcile_schedule(
    sources: &[SourceRecord],
    schedule: &mut HashMap<i64, Schedule>,
    now: Instant,
) {
    let mut live = HashSet::with_capacity(sources.len());
    for source in sources {
        live.insert(source.id);
        let (recursive, poll_secs) = source_scan_config(source);
        let interval = Duration::from_secs(poll_secs);
        let entry = schedule.entry(source.id).or_insert_with(|| Schedule {
            id: source.id,
            root_path: source.root_path.clone(),
            kind: source.kind,
            scan_mode: source.scan_mode.clone(),
            enabled: source.enabled,
            offline: source.online_state == "offline",
            recursive,
            interval,
            next_poll: now,
            retry_after: None,
            retry_job: None,
            watch_ok: false,
            watch_retry_at: None,
        });
        entry.id = source.id;
        entry.root_path = source.root_path.clone();
        entry.kind = if source.kind == SourceKind::Local {
            indexfox_source::source_kind(Path::new(&source.root_path))
        } else {
            source.kind
        };
        entry.scan_mode = source.scan_mode.clone();
        entry.enabled = source.enabled;
        entry.offline = source.online_state == "offline";
        entry.recursive = recursive;
        if entry.interval != interval {
            entry.interval = interval;
            entry.next_poll = now + interval;
        }
        if !wants_watch(entry) {
            entry.watch_ok = false;
            entry.watch_retry_at = None;
        }
    }
    schedule.retain(|id, _| live.contains(id));
}

fn new_watcher(inner: &Arc<Inner>) -> notify::Result<RecommendedWatcher> {
    let events = Arc::clone(inner);
    RecommendedWatcher::new(
        move |result: notify::Result<Event>| match result {
            Ok(event) => events.handle_event(event),
            Err(error) => events.set_last_error(format!("file watcher error: {error}")),
        },
        Config::default(),
    )
}

fn sync_watcher(
    inner: &Arc<Inner>,
    watcher: &mut Option<RecommendedWatcher>,
    watched: &mut HashMap<PathBuf, WatchEntry>,
    schedule: &mut HashMap<i64, Schedule>,
    now: Instant,
    retry_at: &mut Option<Instant>,
) {
    if watcher.is_none() && retry_at.map_or(true, |retry| now >= retry) {
        match new_watcher(inner) {
            Ok(created) => {
                *watcher = Some(created);
                *retry_at = None;
            }
            Err(error) => {
                inner.set_last_error(format!("cannot start file watcher: {error}"));
                *retry_at = Some(now + WATCH_RETRY);
            }
        }
    }

    let desired: HashMap<PathBuf, WatchEntry> = schedule
        .values()
        .filter(|entry| wants_watch(entry))
        .map(|entry| {
            let path = PathBuf::from(&entry.root_path);
            (
                path.clone(),
                WatchEntry {
                    path,
                    source_id: entry.id,
                    recursive: entry.recursive,
                },
            )
        })
        .collect();

    let stale: Vec<PathBuf> = watched
        .iter()
        .filter(|(path, current)| {
            desired.get(*path).map_or(true, |wanted| {
                wanted.source_id != current.source_id || wanted.recursive != current.recursive
            })
        })
        .map(|(path, _)| (*path).clone())
        .collect();
    for path in stale {
        if let Some(watcher) = watcher.as_mut() {
            let _ = watcher.unwatch(&path);
        }
        if let Some(previous) = watched.remove(&path) {
            if let Some(entry) = schedule.get_mut(&previous.source_id) {
                if wants_watch(entry) {
                    entry.watch_ok = false;
                    entry.watch_retry_at = None;
                }
            }
        }
    }

    if let Some(watcher) = watcher.as_mut() {
        for (path, wanted) in &desired {
            if watched
                .get(path)
                .is_some_and(|current| current.source_id == wanted.source_id)
            {
                continue;
            }
            if schedule
                .get(&wanted.source_id)
                .and_then(|entry| entry.watch_retry_at)
                .is_some_and(|retry| now < retry)
            {
                continue;
            }
            let mode = if wanted.recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            match watcher.watch(path, mode) {
                Ok(()) => {
                    watched.insert(path.clone(), wanted.clone());
                    if let Some(entry) = schedule.get_mut(&wanted.source_id) {
                        entry.watch_ok = true;
                        entry.watch_retry_at = None;
                    }
                }
                Err(error) => {
                    inner.set_last_error(format!("cannot watch {}: {error}", path.display()));
                    if let Some(entry) = schedule.get_mut(&wanted.source_id) {
                        entry.watch_ok = false;
                        entry.watch_retry_at = Some(now + WATCH_RETRY);
                    }
                }
            }
        }
    } else {
        for wanted in desired.values() {
            if let Some(entry) = schedule.get_mut(&wanted.source_id) {
                entry.watch_ok = false;
            }
        }
    }

    let entries: Vec<WatchEntry> = watched.values().cloned().collect();
    *lock(&inner.watches) = entries;
}

fn source_due(schedule: &Schedule, events: &HashMap<i64, Instant>, now: Instant) -> bool {
    if !automatic(schedule) {
        return false;
    }
    if let Some(retry) = schedule.retry_after {
        return now >= retry;
    }
    if event_capable(schedule) {
        if let Some(event_at) = events.get(&schedule.id) {
            if now >= *event_at + QUIET_WINDOW {
                return true;
            }
        }
    }
    now >= schedule.next_poll
}

fn next_deadline(
    schedule: &HashMap<i64, Schedule>,
    events: &HashMap<i64, Instant>,
    now: Instant,
) -> Option<Duration> {
    let mut best: Option<Instant> = None;
    for entry in schedule.values().filter(|entry| automatic(entry)) {
        if let Some(retry) = entry.retry_after {
            best = Some(best.map_or(retry, |current| current.min(retry)));
            continue;
        }
        if event_capable(entry) {
            if let Some(event_at) = events.get(&entry.id) {
                let due = *event_at + QUIET_WINDOW;
                best = Some(best.map_or(due, |current| current.min(due)));
            }
        }
        best = Some(best.map_or(entry.next_poll, |current| current.min(entry.next_poll)));
    }
    best.map(|deadline| deadline.saturating_duration_since(now))
}

fn queue_due(inner: &Inner, schedule: &mut HashMap<i64, Schedule>, now: Instant) {
    let _gate = lock(&inner.job_gate);
    let active_sources: HashSet<i64> = lock(&inner.active)
        .values()
        .map(|job| job.source_id)
        .collect();
    let mut events = lock(&inner.events);
    events.retain(|id, _| {
        schedule
            .get(id)
            .is_some_and(|entry| entry.enabled && entry.scan_mode == "auto")
    });
    let due: Vec<i64> = schedule
        .iter()
        .filter(|(_, entry)| source_due(entry, &events, now))
        .map(|(id, _)| *id)
        .collect();
    for source_id in due {
        if active_sources.contains(&source_id) {
            // The active scan may already have passed the changed file.
            if let Some(when) = events.get_mut(&source_id) {
                *when = now;
            }
            if let Some(entry) = schedule.get_mut(&source_id) {
                entry.next_poll = now + entry.interval;
                if entry.retry_after.is_some() {
                    entry.retry_after = Some(now + entry.interval);
                }
            }
            continue;
        }
        let Some(entry) = schedule.get_mut(&source_id) else {
            continue;
        };
        let interval = entry.interval;
        let retry = entry.retry_job.take();
        let queued = if let Some((job_id, attempt_count, error)) = retry {
            match inner.with_database(|db| {
                // The schedule may predate a source removal and its job ID may have been reused.
                if db
                    .source_by_id(source_id)
                    .map_err(|error| error.to_string())?
                    .is_none()
                {
                    return Ok(());
                }
                db.update_index_job(job_id, "queued", attempt_count, Some(&error))
                    .map_err(|error| format!("cannot requeue job {job_id}: {error}"))
            }) {
                Ok(()) => Ok(()),
                Err(_) => queue_source_scan(inner, source_id),
            }
        } else {
            queue_source_scan(inner, source_id)
        };
        match queued {
            Ok(()) => {
                events.remove(&source_id);
                entry.retry_after = None;
                entry.retry_job = None;
                entry.next_poll = now + interval;
            }
            Err(error) => {
                inner.set_last_error(error);
                entry.retry_after = Some(now + interval);
                entry.next_poll = now + interval;
            }
        }
    }
}

fn queue_source_scan(inner: &Inner, source_id: i64) -> Result<(), String> {
    inner.with_database(|db| match db.queue_source_scan(source_id, false) {
        Ok(_) | Err(IndexError::SourceNotFound(_)) => Ok(()),
        Err(error) => Err(format!("cannot queue source {source_id}: {error}")),
    })
}

fn claim_and_register(inner: &Arc<Inner>) -> Result<Option<PendingJob>, String> {
    let _gate = lock(&inner.job_gate);
    let claimed = inner.with_database(|db| {
        let Some(job) = db
            .claim_source_scan()
            .map_err(|error| format!("cannot claim source scan: {error}"))?
        else {
            return Ok(None);
        };
        let source = db
            .source_by_id(job.source_id)
            .map_err(|error| format!("cannot read source {}: {error}", job.source_id))?;
        Ok(Some((job, source)))
    })?;
    let Some((job, source)) = claimed else {
        return Ok(None);
    };
    let Some(source) = source else {
        let message = format!("source {} for job {} disappeared", job.source_id, job.id);
        let _ = inner.with_database(|db| {
            db.update_index_job(job.id, "failed", job.attempt_count, Some(&message))
                .map_err(|error| error.to_string())
        });
        return Err(message);
    };

    let control = Arc::new(JobControl::new(inner));
    if lock(&inner.pending_cancels).remove(&job.id) {
        control.cancelled.store(true, Ordering::SeqCst);
    }
    lock(&inner.active).insert(
        job.id,
        ActiveJob {
            job_id: job.id,
            source_id: job.source_id,
            root_path: source.root_path,
            processed: 0,
            total: 0,
            indexed: 0,
            failed: 0,
            control: Arc::clone(&control),
        },
    );
    Ok(Some(PendingJob { job, control }))
}

fn run_job(inner: &Inner, job: &IndexJobRecord, control: &JobControl, force: bool) -> Outcome {
    let result = inner.with_database(|db| {
        let pipeline = index_registered_source(
            db,
            job.source_id,
            force,
            &|| control.should_stop(),
            &|report: &IndexReport| inner.update_progress(job.id, report),
        );
        if control.cancelled.load(Ordering::SeqCst) {
            // A cancelled pipeline keeps the previous index and must not
            // reconcile. Its job row is no longer needed.
            let _ = db.delete_index_job(job.id);
            return Ok(Outcome::Cancelled);
        }
        if inner.shutdown.load(Ordering::SeqCst) {
            // Startup recovery requeues work left running when the app closed.
            return Ok(Outcome::Cancelled);
        }
        let failure = match pipeline {
            Ok(report) if !report.incomplete_scan => {
                if let Some(issue) = report.issues.first() {
                    inner.set_last_error(issue.clone());
                }
                db.delete_index_job(job.id)
                    .map_err(|error| format!("cannot finish job {}: {error}", job.id))?;
                return Ok(Outcome::Completed);
            }
            Ok(report) => report
                .issues
                .into_iter()
                .next()
                .unwrap_or_else(|| "扫描未完成。".into()),
            Err(error) => format!("{error:#}"),
        };
        inner.set_last_error(failure.clone());
        if let Err(error) = db.update_index_job(job.id, "failed", job.attempt_count, Some(&failure))
        {
            inner.set_last_error(format!("cannot record failure for job {}: {error}", job.id));
        }
        Ok(Outcome::Failed(failure))
    });
    match result {
        Ok(outcome) => outcome,
        Err(error) => {
            inner.set_last_error(error.clone());
            Outcome::Failed(error)
        }
    }
}

fn spawn_worker(inner: Arc<Inner>, sender: Sender<Completion>, pending: PendingJob) {
    let PendingJob { job, control } = pending;
    let job_id = job.id;
    let source_id = job.source_id;
    let attempt_count = job.attempt_count;
    let force = job.kind == "rebuild";
    let worker_inner = Arc::clone(&inner);
    let worker_sender = sender.clone();
    let spawned = thread::Builder::new()
        .name(format!("indexfox-index-{job_id}"))
        .spawn(move || {
            let outcome = run_job(&worker_inner, &job, &control, force);
            worker_inner.finish_job(job_id);
            let _ = worker_sender.send(Completion {
                source_id,
                job_id,
                attempt_count,
                outcome,
            });
            worker_inner.wake();
        });
    if let Err(error) = spawned {
        let message = format!("cannot start index worker for source {source_id}: {error}");
        let _ = inner.with_database(|db| {
            db.update_index_job(job_id, "failed", attempt_count, Some(&message))
                .map_err(|error| error.to_string())
        });
        inner.finish_job(job_id);
        inner.set_last_error(message.clone());
        let _ = sender.send(Completion {
            source_id,
            job_id,
            attempt_count,
            outcome: Outcome::Failed(message),
        });
        inner.wake();
    }
}

fn drain_completions(receiver: &Receiver<Completion>, schedule: &mut HashMap<i64, Schedule>) {
    while let Ok(completion) = receiver.try_recv() {
        let now = Instant::now();
        let Some(entry) = schedule.get_mut(&completion.source_id) else {
            continue;
        };
        let interval = entry.interval;
        match completion.outcome {
            Outcome::Completed | Outcome::Cancelled => {
                entry.retry_after = None;
                entry.retry_job = None;
                entry.next_poll = now + interval;
            }
            Outcome::Failed(message) => {
                if entry.scan_mode == "manual" {
                    entry.retry_after = None;
                    entry.retry_job = None;
                } else {
                    entry.retry_job = Some((completion.job_id, completion.attempt_count, message));
                    entry.retry_after = Some(now + interval);
                    entry.next_poll = now + interval;
                }
            }
        }
    }
}

fn refresh_exclusions(inner: &Inner) {
    if let Ok(location) = inner.database.try_read() {
        *lock(&inner.exclusions) = Exclusions::new(location.path());
    }
}

fn load_mode(inner: &Inner) {
    if inner.mode_explicit.load(Ordering::SeqCst) {
        return;
    }
    let loaded = inner.with_database(|db| {
        let value = db
            .conn()
            .query_row(
                "SELECT value FROM app_meta WHERE key = 'resource_mode'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok();
        Ok(value.and_then(|value| parse_mode(&value)))
    });
    if let Ok(Some(mode)) = loaded {
        if !inner.mode_explicit.load(Ordering::SeqCst) {
            *lock(&inner.mode) = mode;
        }
    }
}

fn load_sources(inner: &Inner) -> Result<(Vec<SourceRecord>, usize), String> {
    inner.with_database(|db| {
        let sources = db
            .list_sources()
            .map_err(|error| format!("cannot list sources: {error}"))?;
        let queued = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM index_jobs
                 WHERE state = 'queued' AND kind IN ('scan', 'rebuild')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| format!("cannot count queued jobs: {error}"))?;
        Ok((
            sources,
            usize::try_from(queued.max(0)).unwrap_or(usize::MAX),
        ))
    })
}

fn pump_jobs(inner: &Arc<Inner>, sender: &Sender<Completion>) {
    loop {
        if inner.paused.load(Ordering::SeqCst) || inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let limit = concurrency(*lock(&inner.mode));
        if lock(&inner.active).len() >= limit {
            break;
        }
        match claim_and_register(inner) {
            Ok(Some(pending)) => spawn_worker(Arc::clone(inner), sender.clone(), pending),
            Ok(None) => break,
            Err(error) => {
                inner.set_last_error(error);
                break;
            }
        }
    }
}

fn coordinator_loop(inner: Arc<Inner>, sender: Sender<Completion>, receiver: Receiver<Completion>) {
    if !inner.mode_explicit.load(Ordering::SeqCst) {
        load_mode(&inner);
    }
    let mut seen = 0u64;
    let mut schedule: HashMap<i64, Schedule> = HashMap::new();
    let mut watcher: Option<RecommendedWatcher> = None;
    let mut watched: HashMap<PathBuf, WatchEntry> = HashMap::new();
    let mut watcher_retry_at = Some(Instant::now());
    let mut recovered = false;

    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        drain_completions(&receiver, &mut schedule);
        refresh_exclusions(&inner);
        if !recovered {
            match inner.with_database(|db| {
                db.recover_source_scans()
                    .map_err(|error| format!("cannot recover source scans: {error}"))
            }) {
                Ok(_) => recovered = true,
                Err(error) => {
                    inner.set_last_error(error);
                    inner.wait_for_change(&mut seen, Some(MAX_WAIT));
                    continue;
                }
            }
        }

        let now = Instant::now();
        match load_sources(&inner) {
            Ok((sources, queued)) => {
                inner.queued.store(queued, Ordering::SeqCst);
                reconcile_schedule(&sources, &mut schedule, now);
                sync_watcher(
                    &inner,
                    &mut watcher,
                    &mut watched,
                    &mut schedule,
                    now,
                    &mut watcher_retry_at,
                );
                if !inner.paused.load(Ordering::SeqCst) {
                    queue_due(&inner, &mut schedule, now);
                }
            }
            Err(error) => inner.set_last_error(error),
        }

        if inner.paused.load(Ordering::SeqCst) {
            inner.wait_for_change(&mut seen, None);
            continue;
        }
        pump_jobs(&inner, &sender);

        let wait = next_deadline(&schedule, &lock(&inner.events), Instant::now())
            .map(|deadline| deadline.min(MAX_WAIT).max(Duration::from_millis(10)))
            .unwrap_or(IDLE_WAIT);
        inner.wait_for_change(&mut seen, Some(wait));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn concurrency_matches_resource_mode() {
        assert_eq!(concurrency(ResourceMode::Saver), 1);
        assert_eq!(concurrency(ResourceMode::Balanced), 2);
        assert_eq!(concurrency(ResourceMode::Fast), 4);
    }

    #[test]
    fn exclusions_cover_database_sidecars_and_directory() {
        let exclusions = Exclusions::new(Path::new("/var/data/index.db"));
        assert!(exclusions.matches(Path::new("/var/data/index.db")));
        assert!(exclusions.matches(Path::new("/var/data/index.db-wal")));
        assert!(exclusions.matches(Path::new("/var/data/index.db-shm")));
        assert!(exclusions.matches(Path::new("/var/data/index.db-journal")));
        assert!(!exclusions.matches(Path::new("/var/data/unrelated.txt")));
        assert!(!exclusions.matches(Path::new("/home/user/index.db")));
        assert!(!exclusions.matches(Path::new("/home/user/doc.txt")));
    }

    #[test]
    fn snapshot_serializes_camel_case() {
        let location = DatabaseLocation::load(
            PathBuf::from("/tmp/indexfox-scheduler-test/index.db"),
            PathBuf::from("/tmp/indexfox-scheduler-test/settings.json"),
        );
        let scheduler = IndexScheduler::new(Arc::new(RwLock::new(location)));
        let snapshot = scheduler.snapshot();
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["paused"], Value::Bool(false));
        assert_eq!(value["resourceMode"], Value::String("balanced".to_string()));
        assert_eq!(value["active"], Value::Array(Vec::new()));
        assert_eq!(value["queued"], Value::Number(0.into()));
        assert_eq!(value["lastError"], Value::Null);
    }

    #[test]
    fn shutdown_retains_running_rebuild_for_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let db = IndexDb::open(&path).unwrap();
        let source = indexfox_cli::pipeline::register_source(&db, dir.path()).unwrap();
        let job_id = db.queue_source_scan(source, true).unwrap();
        let location = DatabaseLocation::load(path, dir.path().join("location.json"));
        let scheduler = IndexScheduler::new(Arc::new(RwLock::new(location)));
        let pending = claim_and_register(&scheduler.inner).unwrap().unwrap();

        scheduler.shutdown();
        assert!(matches!(
            run_job(&scheduler.inner, &pending.job, &pending.control, true),
            Outcome::Cancelled
        ));
        assert_eq!(db.recover_source_scans().unwrap(), 1);
        let recovered = db.claim_source_scan().unwrap().unwrap();
        assert_eq!(recovered.id, job_id);
        assert_eq!(recovered.kind, "rebuild");
    }

    #[test]
    fn removing_source_cancels_its_worker_and_keeps_original_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let original = dir.path().join("original.txt");
        std::fs::write(&original, "original contents").unwrap();
        let db = IndexDb::open(&path).unwrap();
        let source = indexfox_cli::pipeline::register_source(&db, dir.path()).unwrap();
        let other_source = db.ensure_source(SourceKind::Local, "/other").unwrap();
        db.queue_source_scan(source, false).unwrap();
        db.queue_source_scan(other_source, false).unwrap();
        let location = DatabaseLocation::load(path, dir.path().join("location.json"));
        let scheduler = IndexScheduler::new(Arc::new(RwLock::new(location)));
        let pending = claim_and_register(&scheduler.inner).unwrap().unwrap();
        assert_eq!(pending.job.source_id, source);
        scheduler.set_paused(true);
        let (sender, receiver) = mpsc::channel();
        spawn_worker(Arc::clone(&scheduler.inner), sender, pending);

        scheduler.remove_source(source).unwrap();

        let completion = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(completion.outcome, Outcome::Cancelled));
        assert!(db.source_by_id(source).unwrap().is_none());
        assert!(db.index_jobs_for_source(source).unwrap().is_empty());
        assert_eq!(db.index_jobs_for_source(other_source).unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(original).unwrap(),
            "original contents"
        );
    }

    fn test_schedule(now: Instant) -> Schedule {
        Schedule {
            id: 1,
            root_path: "/tmp/source".to_string(),
            kind: SourceKind::Local,
            scan_mode: "auto".to_string(),
            enabled: true,
            offline: false,
            recursive: true,
            interval: Duration::from_secs(60),
            next_poll: now + Duration::from_secs(60),
            retry_after: None,
            retry_job: None,
            watch_ok: true,
            watch_retry_at: None,
        }
    }

    #[test]
    fn events_must_be_quiet_and_retry_gates_win() {
        let now = Instant::now();
        let mut schedule = test_schedule(now);
        let mut events = HashMap::new();
        assert!(!source_due(&schedule, &events, now));

        events.insert(1, now);
        assert!(!source_due(&schedule, &events, now));
        assert!(source_due(&schedule, &events, now + QUIET_WINDOW));

        schedule.retry_after = Some(now + Duration::from_secs(10));
        assert!(!source_due(&schedule, &events, now + QUIET_WINDOW));
        assert!(source_due(
            &schedule,
            &events,
            now + Duration::from_secs(10)
        ));
    }
}
