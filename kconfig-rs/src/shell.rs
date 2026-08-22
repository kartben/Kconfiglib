//! `$(shell,...)` execution, memoized and optionally pre-warmed in parallel.
//!
//! On Linux, `scripts/Kconfig.include` turns roughly a hundred `depends on`
//! lines into compiler probes (`cc-option`, `as-instr`, `ld-option`, ...).
//! Each one forks a shell that forks a compiler. Measured on the reference
//! machine that is ~1.5 s — half the wall clock of a full x86 Kconfig load,
//! and none of it is parsing.
//!
//! The probes are pure functions of the command string and the environment, so
//! this module treats them as such:
//!
//!  * results are memoized within a run;
//!  * results are persisted across runs, keyed by a fingerprint of the
//!    environment variables the probes actually read, so a repeat load spends
//!    nothing at all;
//!  * when the fingerprint changes, the commands recorded by the previous run
//!    are re-executed **in parallel** before parsing starts, so even a cold
//!    cache costs `total / nproc` rather than `total`.
//!
//! A command that was never recorded simply runs synchronously the first time
//! the parser asks for it, so the cache is an optimization and never a
//! correctness dependency.

use std::collections::hash_map::Entry;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;

/// Environment variables whose values feed into probe results. Changing any of
/// them invalidates the persisted cache.
const FINGERPRINT_VARS: &[&str] = &[
    "ARCH",
    "SRCARCH",
    "CC",
    "LD",
    "AS",
    "NM",
    "OBJCOPY",
    "OBJDUMP",
    "READELF",
    "AR",
    "RUSTC",
    "HOSTCC",
    "HOSTCXX",
    "PAHOLE",
    "CLANG_FLAGS",
    "KERNELVERSION",
    "srctree",
    "PATH",
    "USERCFLAGS",
    "USERLDFLAGS",
    "CC_VERSION_TEXT",
];

pub struct ShellCache {
    results: FxHashMap<String, String>,
    /// Commands seen this run, in first-seen order, for the next run's warm-up.
    seen: Vec<String>,
    path: Option<PathBuf>,
    fingerprint: u64,
    /// Commands loaded from a stale cache file, available for parallel re-probing.
    pending_warmup: Vec<String>,
    pub stats: ShellStats,
    /// Wall-clock time spent running probes on the parser's thread.
    pub elapsed: f64,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct ShellStats {
    pub hits: usize,
    pub misses: usize,
    pub warmed: usize,
}

impl ShellCache {
    /// `path` is where to persist results; `None` disables persistence.
    pub fn new(path: Option<PathBuf>) -> ShellCache {
        let fingerprint = environment_fingerprint();
        let mut cache = ShellCache {
            results: FxHashMap::default(),
            seen: Vec::new(),
            path,
            fingerprint,
            pending_warmup: Vec::new(),
            stats: ShellStats::default(),
            elapsed: 0.0,
        };
        if let Some(p) = cache.path.clone() {
            cache.load(&p);
        }
        cache
    }

    /// Runs the commands recorded by an earlier run across `jobs` threads.
    /// Returns the number of commands executed.
    pub fn warm_up(&mut self, jobs: usize) -> usize {
        let commands = std::mem::take(&mut self.pending_warmup);
        if commands.is_empty() || jobs <= 1 {
            for cmd in commands {
                let out = run_command(&cmd).0;
                self.results.insert(cmd, out);
            }
            return 0;
        }

        let queue = Arc::new(Mutex::new(commands.into_iter()));
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for _ in 0..jobs {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || loop {
                let next = queue.lock().expect("probe queue poisoned").next();
                match next {
                    Some(cmd) => {
                        let (out, _) = run_command(&cmd);
                        // The receiver outlives every worker, so this cannot fail.
                        let _ = tx.send((cmd, out));
                    }
                    None => break,
                }
            }));
        }
        drop(tx);

        let mut n = 0;
        for (cmd, out) in rx {
            self.results.insert(cmd, out);
            n += 1;
        }
        for h in handles {
            let _ = h.join();
        }
        self.stats.warmed = n;
        n
    }

    /// Returns the output of `command`, running it if it is not already known.
    /// Any stderr output is returned alongside so the caller can warn about it.
    pub fn run(&mut self, command: &str) -> (String, Option<String>) {
        if !self.results.contains_key(command) {
            self.stats.misses += 1;
            let start = std::time::Instant::now();
            let (out, err) = run_command(command);
            self.elapsed += start.elapsed().as_secs_f64();
            self.record_seen(command);
            self.results.insert(command.to_string(), out.clone());
            return (out, err);
        }
        self.stats.hits += 1;
        self.record_seen(command);
        (self.results[command].clone(), None)
    }

    fn record_seen(&mut self, command: &str) {
        if !self.seen.iter().any(|c| c == command) {
            self.seen.push(command.to_string());
        }
    }

    /// Persists this run's results so the next run starts warm.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut out = String::new();
        out.push_str(&format!("kconfig-shell-cache 1 {}\n", self.fingerprint));
        for cmd in &self.seen {
            let Some(res) = self.results.get(cmd) else {
                continue;
            };
            out.push_str(&format!("{} {}\n", cmd.len(), res.len()));
            out.push_str(cmd);
            out.push('\n');
            out.push_str(res);
            out.push('\n');
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::File::create(path) {
            let _ = f.write_all(out.as_bytes());
        }
    }

    fn load(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let mut lines = text.split_inclusive('\n');
        let Some(header) = lines.next() else { return };
        let mut fields = header.trim_end().split(' ');
        if fields.next() != Some("kconfig-shell-cache") || fields.next() != Some("1") {
            return;
        }
        let stale = fields.next().and_then(|f| f.parse::<u64>().ok()) != Some(self.fingerprint);

        let mut rest: String = lines.collect();
        let mut pos = 0usize;
        while let Some(nl) = rest[pos..].find('\n') {
            let header = &rest[pos..pos + nl];
            let mut it = header.split(' ');
            let (Some(cl), Some(rl)) = (it.next(), it.next()) else {
                break;
            };
            let (Ok(cl), Ok(rl)) = (cl.parse::<usize>(), rl.parse::<usize>()) else {
                break;
            };
            let cmd_start = pos + nl + 1;
            let res_start = cmd_start + cl + 1;
            let end = res_start + rl + 1;
            if end > rest.len() {
                break;
            }
            let cmd = rest[cmd_start..cmd_start + cl].to_string();
            if stale {
                self.pending_warmup.push(cmd);
            } else {
                let res = rest[res_start..res_start + rl].to_string();
                if let Entry::Vacant(e) = self.results.entry(cmd) {
                    e.insert(res);
                }
            }
            pos = end;
        }
        rest.clear();
    }
}

/// Runs `command` through `sh -c` and applies Kconfig's output normalization:
/// trailing newlines are dropped and interior newlines become spaces.
fn run_command(command: &str) -> (String, Option<String>) {
    let output = Command::new("sh").arg("-c").arg(command).output();
    let Ok(output) = output else {
        return (String::new(), None);
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let normalized = stdout.trim_end_matches('\n').replace('\n', " ");
    let stderr = if output.stderr.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&output.stderr).into_owned())
    };
    (normalized, stderr)
}

fn environment_fingerprint() -> u64 {
    // FNV-1a over the variables the probes depend on. Not cryptographic; it
    // only has to notice that the toolchain changed.
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    for var in FINGERPRINT_VARS {
        mix(var.as_bytes());
        mix(b"=");
        mix(std::env::var(var).unwrap_or_default().as_bytes());
        mix(b"\n");
    }
    hash
}
