//! Ahead-of-time shader translation and compute pipeline precompilation.
//!
//! Populates the exact same caches that `victory run` uses:
//! - `<cache_dir>/shaders/`: translated DXIL + binding metadata for every
//!   supported stage (VS, PS, MS, CS), keyed by the same executable hash, GPU
//!   wave sizes and translation switches.
//! - `<cache_dir>/pipelines/host-pipelines.bin` (with `--pipelines`): compiled
//!   host D3D12 compute PSOs and GPU driver shader cache entries, built with
//!   the exact same root signature and library key as run-time compute
//!   pipelines.
//!
//! Ported from `vitutu1/victory-perf` (GPL-3.0-or-later).

use std::path::PathBuf;

use anyhow::Result;
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StageFilter {
    Vs,
    Ps,
    Ms,
    Cs,
}

#[cfg(windows)]
impl StageFilter {
    fn matches(self, stage: victory_shader::HwStage) -> bool {
        use victory_shader::HwStage;
        matches!(
            (self, stage),
            (StageFilter::Vs, HwStage::Vs)
                | (StageFilter::Ps, HwStage::Ps)
                | (StageFilter::Ms, HwStage::Ms)
                | (StageFilter::Cs, HwStage::Cs)
        )
    }

    fn label(self) -> &'static str {
        match self {
            StageFilter::Vs => "vs",
            StageFilter::Ps => "ps",
            StageFilter::Ms => "ms",
            StageFilter::Cs => "cs",
        }
    }
}

#[cfg(windows)]
pub fn run(
    paths: Vec<PathBuf>,
    pipelines: bool,
    only: Option<StageFilter>,
    limit: Option<usize>,
    jobs: Option<usize>,
    pipeline_jobs: Option<usize>,
) -> Result<()> {
    use std::collections::{BTreeMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    use anyhow::Context;
    use victory_hle::modules::d3d12x;
    use victory_shader::emit::Target;
    use victory_shader::{Container, FourCC, native_programs};

    use crate::shader::{MAX_FILE, sources};

    let started = Instant::now();
    let cfg = victory_core::Config::load()?;
    let paths = crate::default_paths(paths)?;
    let shader_cache_dir = cfg.cache_subdir("shaders")?;
    let pipeline_cache_dir = shader_cache_dir.with_file_name("pipelines");
    d3d12x::use_shader_cache(shader_cache_dir.clone());

    let host_info = match d3d12x::precompile::init_host() {
        Ok(info) => Some(info),
        Err(e) if pipelines => {
            anyhow::bail!("--pipelines requires a host D3D12 GPU: {e}");
        }
        Err(e) => {
            eprintln!("warning: no host D3D12 device ({e}); translating without GPU probe");
            None
        }
    };
    let target = host_info.map_or(Target::ANY, |i| i.target);

    let threads = jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()))
        .max(1);
    let max_pipeline_jobs = pipeline_jobs.unwrap_or(2).max(1);
    let max_containers = limit.unwrap_or(usize::MAX);

    println!("shader cache   {}", shader_cache_dir.display());
    if pipelines {
        println!(
            "pipeline cache {} (compute only, up to {} driver compiles at once)",
            pipeline_cache_dir.display(),
            max_pipeline_jobs
        );
    }
    if let Some(info) = host_info {
        println!(
            "host GPU       mesh shaders: {}, wave lanes: {}-{}",
            info.mesh_shaders, info.target.lanes_min, info.target.lanes_max
        );
    }
    if let Some(stage) = only {
        println!("stage filter   {}", stage.label());
    }
    println!("workers        {threads}");

    let mut files = Vec::new();
    for root in &paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let len = entry.metadata().map_or(0, |m| m.len());
            if len == 0 || len > MAX_FILE {
                continue;
            }
            files.push(entry.into_path());
        }
    }

    #[derive(Default)]
    struct Totals {
        containers: usize,
        duplicates: usize,
        skipped_stage: usize,
        skipped_non_dxil: usize,
        corrupt: usize,
        translated: usize,
        cached: usize,
        by_stage: BTreeMap<&'static str, (usize, usize)>,
        failed: BTreeMap<String, (usize, String)>,
        pipelines_compiled: usize,
        pipelines_loaded: usize,
        pipelines_refused: BTreeMap<String, (usize, String)>,
    }

    impl Totals {
        fn merge(&mut self, other: Totals) {
            self.containers += other.containers;
            self.duplicates += other.duplicates;
            self.skipped_stage += other.skipped_stage;
            self.skipped_non_dxil += other.skipped_non_dxil;
            self.corrupt += other.corrupt;
            self.translated += other.translated;
            self.cached += other.cached;
            for (stage, (t, c)) in other.by_stage {
                let entry = self.by_stage.entry(stage).or_default();
                entry.0 += t;
                entry.1 += c;
            }
            for (reason, (n, example)) in other.failed {
                self.failed.entry(reason).or_insert((0, example)).0 += n;
            }
            self.pipelines_compiled += other.pipelines_compiled;
            self.pipelines_loaded += other.pipelines_loaded;
            for (reason, (n, example)) in other.pipelines_refused {
                self.pipelines_refused
                    .entry(reason)
                    .or_insert((0, example))
                    .0 += n;
            }
        }
    }

    /// Caps concurrent GPU driver compilations so large compute shaders do not
    /// exhaust system commit on 16 GB hosts.
    struct Semaphore {
        limit: usize,
        active: Mutex<usize>,
        free: Condvar,
    }

    impl Semaphore {
        fn new(limit: usize) -> Self {
            Self {
                limit,
                active: Mutex::new(0),
                free: Condvar::new(),
            }
        }

        fn run<T>(&self, f: impl FnOnce() -> T) -> T {
            {
                let mut active = lock(&self.active);
                while *active >= self.limit {
                    active = self.free.wait(active).unwrap_or_else(|e| e.into_inner());
                }
                *active += 1;
            }
            struct Guard<'a>(&'a Semaphore);
            impl Drop for Guard<'_> {
                fn drop(&mut self) {
                    let mut active = lock(&self.0.active);
                    *active -= 1;
                    self.0.free.notify_one();
                }
            }
            let _guard = Guard(self);
            f()
        }
    }

    let seen: Mutex<HashSet<[u8; 32]>> = Mutex::new(HashSet::new());
    let total = Mutex::new(Totals::default());
    let unique_claimed = AtomicUsize::new(0);
    let processed = AtomicUsize::new(0);
    let live_translated = AtomicUsize::new(0);
    let live_cached = AtomicUsize::new(0);
    let live_failed = AtomicUsize::new(0);
    let live_pso_made = AtomicUsize::new(0);
    let live_pso_loaded = AtomicUsize::new(0);
    let driver_gate = Semaphore::new(max_pipeline_jobs);

    for file in &files {
        if unique_claimed.load(Ordering::Relaxed) >= max_containers {
            break;
        }
        let Ok(data) = std::fs::read(file) else {
            continue;
        };
        let jobs = sources(&data);
        if jobs.is_empty() {
            continue;
        }
        let next = AtomicUsize::new(0);
        let worker_count = threads.min(jobs.len());
        std::thread::scope(|s| {
            for _ in 0..worker_count {
                s.spawn(|| {
                    let mut local = Totals::default();
                    while let Some(src) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                        if unique_claimed.load(Ordering::Relaxed) >= max_containers {
                            break;
                        }
                        let Some(bytes) = src.bytes() else {
                            local.corrupt += 1;
                            continue;
                        };
                        let Ok(container) = Container::parse(&bytes) else {
                            local.skipped_non_dxil += 1;
                            continue;
                        };
                        let Some(dxil) = container.part(FourCC::DXIL).map(|p| p.data) else {
                            local.skipped_non_dxil += 1;
                            continue;
                        };
                        if let Some(filter) = only {
                            let matches = native_programs(dxil)
                                .map(|progs| progs.iter().any(|p| filter.matches(p.stage)))
                                .unwrap_or(false);
                            if !matches {
                                local.skipped_stage += 1;
                                continue;
                            }
                        }
                        // Deduplicate by the full container hash, matching
                        // `victory_gpu::pipeline` and `shader_cache` exactly.
                        let full = blake3::hash(&bytes);
                        if !lock(&seen).insert(*full.as_bytes()) {
                            local.duplicates += 1;
                            continue;
                        }
                        if unique_claimed.fetch_add(1, Ordering::Relaxed) >= max_containers {
                            break;
                        }
                        local.containers += 1;
                        let hash16 = &full.to_hex()[..16];
                        let label = format!("{}{} #{hash16}", file.display(), src.label());

                        match d3d12x::precompile::shader(&bytes, target) {
                            Ok(prepared) => {
                                let stage_counts =
                                    local.by_stage.entry(prepared.stage).or_default();
                                if prepared.cached {
                                    local.cached += 1;
                                    stage_counts.1 += 1;
                                    live_cached.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    local.translated += 1;
                                    stage_counts.0 += 1;
                                    live_translated.fetch_add(1, Ordering::Relaxed);
                                }
                                if pipelines && prepared.is_compute {
                                    let pso_res = driver_gate
                                        .run(|| d3d12x::precompile::compute_pipeline(&prepared));
                                    match pso_res {
                                        Ok(true) => {
                                            local.pipelines_loaded += 1;
                                            live_pso_loaded.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Ok(false) => {
                                            local.pipelines_compiled += 1;
                                            live_pso_made.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(reason) => {
                                            local
                                                .pipelines_refused
                                                .entry(reason)
                                                .or_insert_with(|| (0, label.clone()))
                                                .0 += 1;
                                        }
                                    }
                                }
                            }
                            Err(reason) => {
                                live_failed.fetch_add(1, Ordering::Relaxed);
                                local
                                    .failed
                                    .entry(reason)
                                    .or_insert_with(|| (0, label))
                                    .0 += 1;
                            }
                        }

                        let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
                        if done.is_multiple_of(200) {
                            let secs = started.elapsed().as_secs_f64();
                            if pipelines {
                                eprintln!(
                                    "  {done} shaders: {} translated, {} cached, {} failed | compute PSOs: {} compiled, {} loaded ({secs:.0} s)",
                                    live_translated.load(Ordering::Relaxed),
                                    live_cached.load(Ordering::Relaxed),
                                    live_failed.load(Ordering::Relaxed),
                                    live_pso_made.load(Ordering::Relaxed),
                                    live_pso_loaded.load(Ordering::Relaxed),
                                );
                            } else {
                                eprintln!(
                                    "  {done} shaders: {} translated, {} cached, {} failed ({secs:.0} s)",
                                    live_translated.load(Ordering::Relaxed),
                                    live_cached.load(Ordering::Relaxed),
                                    live_failed.load(Ordering::Relaxed),
                                );
                            }
                        }
                    }
                    lock(&total).merge(local);
                });
            }
        });
    }

    if pipelines {
        d3d12x::precompile::save_pipeline_cache();
    }

    let t = total.into_inner().unwrap_or_else(|e| e.into_inner());
    let secs = started.elapsed().as_secs_f64();
    println!(
        "containers     {} unique ({} duplicates skipped, {} non-DXIL, {} filtered, {} corrupt)",
        t.containers, t.duplicates, t.skipped_non_dxil, t.skipped_stage, t.corrupt
    );
    println!(
        "shaders        {} translated, {} already cached, {} failed ({secs:.1} s)",
        t.translated,
        t.cached,
        t.failed.values().map(|(n, _)| *n).sum::<usize>()
    );
    for (stage, (translated, cached)) in &t.by_stage {
        println!("  {stage:<12} {translated} translated, {cached} cached");
    }
    if pipelines {
        let refused: usize = t.pipelines_refused.values().map(|(n, _)| *n).sum();
        println!(
            "compute PSOs   {} compiled, {} already in library, {} refused",
            t.pipelines_compiled, t.pipelines_loaded, refused
        );
    }
    let mut failed: Vec<_> = t.failed.into_iter().collect();
    failed.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    if !failed.is_empty() {
        println!("translation failures (top 15):");
        for (reason, (n, example)) in failed.into_iter().take(15) {
            println!("  {n:>7}  {reason}  (e.g. {example})");
        }
    }
    let mut refused: Vec<_> = t.pipelines_refused.into_iter().collect();
    refused.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    if !refused.is_empty() {
        println!("compute pipeline refusals (top 10):");
        for (reason, (n, example)) in refused.into_iter().take(10) {
            println!("  {n:>7}  {reason}  (e.g. {example})");
        }
    }

    // Verify the cache directory exists so callers immediately see where
    // outputs landed even on an empty walk.
    let _ = std::fs::metadata(&shader_cache_dir)
        .with_context(|| format!("checking shader cache at {}", shader_cache_dir.display()));
    Ok(())
}

#[cfg(windows)]
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(not(windows))]
pub fn run(
    _: Vec<PathBuf>,
    _: bool,
    _: Option<StageFilter>,
    _: Option<usize>,
    _: Option<usize>,
    _: Option<usize>,
) -> Result<()> {
    anyhow::bail!("shader precompilation requires Windows")
}

#[cfg(all(test, windows))]
mod tests {
    use super::StageFilter;
    use victory_shader::HwStage;

    #[test]
    fn stage_filter_matches_expected_stages() {
        assert!(StageFilter::Vs.matches(HwStage::Vs));
        assert!(!StageFilter::Vs.matches(HwStage::Ps));
        assert!(StageFilter::Ps.matches(HwStage::Ps));
        assert!(StageFilter::Ms.matches(HwStage::Ms));
        assert!(StageFilter::Cs.matches(HwStage::Cs));
        assert!(!StageFilter::Cs.matches(HwStage::Gs));
    }
}
