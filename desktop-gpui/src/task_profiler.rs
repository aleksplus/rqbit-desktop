//! Opt-in GPUI task-profiler dump for diagnosing foreground-executor storms.
//!
//! The app's UI tasks all run on GPUI's foreground executor (the macOS main
//! thread via the dispatch queue). A task that re-arms itself on every poll
//! turns into a mach-message storm that pegs one core, and the usual samples
//! only show the dispatch machinery, not which future is responsible. GPUI's
//! profiler records every task poll with the `spawn` call site of its future,
//! so aggregating the ring buffer by location names the looping task directly.
//!
//! Usage: build with `cargo build -p desktop-gpui --features task-profiler`
//! and run with `RQBIT_TASK_PROFILER` set to an output path (an empty value
//! picks a default under the temp dir). `RQBIT_TASK_PROFILER_SECS` tunes the
//! dump interval (default 2). Nothing is collected unless the env var is set,
//! and the collection ring only exists in `task-profiler` builds, so default
//! builds keep zero overhead.

use std::path::PathBuf;

use tracing::warn;

#[cfg(feature = "task-profiler")]
use std::{collections::HashMap, path::Path, time::Duration};
#[cfg(feature = "task-profiler")]
use tracing::info;

fn default_out_path() -> PathBuf {
    std::env::temp_dir().join("rqbit-task-timings.txt")
}

/// Start the task-profiler dump loop if `RQBIT_TASK_PROFILER` is set.
pub fn maybe_start() {
    let Ok(path) = std::env::var("RQBIT_TASK_PROFILER") else {
        return;
    };
    let out_path = if path.is_empty() {
        default_out_path()
    } else {
        PathBuf::from(path)
    };

    #[cfg(not(feature = "task-profiler"))]
    {
        let _ = out_path;
        warn!(
            "RQBIT_TASK_PROFILER is set, but this binary was built without the \
             `task-profiler` feature; rebuild with \
             `cargo build -p desktop-gpui --features task-profiler`"
        );
    }

    #[cfg(feature = "task-profiler")]
    {
        let secs = std::env::var("RQBIT_TASK_PROFILER_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2)
            .clamp(1, 3600);
        gpui::profiler::set_trace_enabled(true);
        info!(path = %out_path.display(), secs, "gpui task profiler enabled");
        let thread = std::thread::Builder::new()
            .name("task-profiler-dump".into())
            .spawn(move || dump_loop(out_path, Duration::from_secs(secs)));
        if let Err(e) = thread {
            warn!(error = %e, "failed to spawn the task-profiler dump thread");
        }
    }
}

#[cfg(feature = "task-profiler")]
fn dump_loop(out_path: PathBuf, interval: Duration) {
    let mut last_pushed: HashMap<std::thread::ThreadId, u64> = HashMap::new();
    loop {
        std::thread::sleep(interval);
        let report = summarize(&mut last_pushed, interval);
        if let Err(e) = write_atomic(&out_path, &report) {
            warn!(
                error = %e,
                path = %out_path.display(),
                "failed to write the task profile"
            );
        }
    }
}

/// Aggregate the current profiler ring buffers into a text report.
///
/// Poll counts come from the per-thread ring (bounded, so very slow tasks may
/// fall off between dumps); the per-thread `total_pushed` counter is
/// cumulative, so its delta over the dump interval is an exact poll rate.
#[cfg(feature = "task-profiler")]
fn summarize(last_pushed: &mut HashMap<std::thread::ThreadId, u64>, interval: Duration) -> String {
    use std::fmt::Write as _;

    let timings = gpui::profiler::get_all_timings(gpui::TasksIncluded::CompletedAndRunning);
    let mut out = String::new();
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(out, "=== gpui task profile @ {unix_secs} ===");

    // Spawn-site aggregation: one entry = one poll of one spawned future.
    #[derive(Default)]
    struct Site {
        polls: u64,
        total_poll: Duration,
        max_poll: Duration,
    }
    let mut sites: HashMap<(&'static str, u32, u32), Site> = HashMap::new();

    for thread in &timings {
        let rate = last_pushed
            .get(&thread.thread_id)
            .map(|prev| thread.total_pushed.saturating_sub(*prev))
            .unwrap_or(thread.total_pushed);
        last_pushed.insert(thread.thread_id, thread.total_pushed);
        let name = thread.thread_name.as_deref().unwrap_or("<unnamed>");
        let _ = writeln!(
            out,
            "thread {:?} ({:?}): {} polls in the last {:?} ({} in ring)",
            name,
            thread.thread_id,
            rate,
            interval,
            thread.timings.len(),
        );
        // The profiler's own "worst tasks" stats are precomputed per thread.
        for line in thread.stats.to_string().trim().lines() {
            let _ = writeln!(out, "  {line}");
        }

        for timing in &thread.timings {
            let loc = timing.location;
            let site = sites
                .entry((loc.file(), loc.line(), loc.column()))
                .or_default();
            site.polls += 1;
            let poll = timing.poll_duration();
            site.total_poll += poll;
            site.max_poll = site.max_poll.max(poll);
        }
    }

    let mut sites: Vec<_> = sites.into_iter().collect();
    sites.sort_by_key(|(_, s)| std::cmp::Reverse(s.polls));
    let _ = writeln!(out, "--- polls by spawn site (top 20) ---");
    for ((file, line, col), s) in sites.iter().take(20) {
        let _ = writeln!(
            out,
            "{:>8} polls  mean {:>10.1?}  max {:>10.1?}  at {file}:{line}:{col}",
            s.polls,
            s.total_poll / s.polls.max(1) as u32,
            s.max_poll,
        );
    }
    out
}

#[cfg(feature = "task-profiler")]
fn write_atomic(path: &Path, data: &str) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(tmp, path)
}
