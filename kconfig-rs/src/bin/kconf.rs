//! `kconf` — load a Kconfig tree and report on it.
//!
//! Usage:
//!   kconf [options] [<top-level Kconfig>]
//!
//! Options:
//!   --write-config <path>   write a .config to <path> ("-" for stdout)
//!   --shell-cache <path>    persist $(shell,...) results here
//!   --probe-jobs <n>        threads for re-probing a stale shell cache
//!   --repeat <n>            load n times and report the fastest run
//!   --stats                 print symbol/expression counts
//!   --quiet                 suppress warnings

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use kconfig::eval::Values;
use kconfig::{Kconfig, LoadOptions};

struct Args {
    top: String,
    write_config: Option<String>,
    shell_cache: Option<PathBuf>,
    probe_jobs: Option<usize>,
    repeat: usize,
    stats: bool,
    quiet: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        top: "Kconfig".to_string(),
        write_config: None,
        shell_cache: None,
        probe_jobs: None,
        repeat: 1,
        stats: false,
        quiet: false,
    };
    let mut positional = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--write-config" => args.write_config = Some(value()?),
            "--shell-cache" => args.shell_cache = Some(PathBuf::from(value()?)),
            "--probe-jobs" => {
                args.probe_jobs = Some(value()?.parse().map_err(|_| "bad --probe-jobs")?)
            }
            "--repeat" => args.repeat = value()?.parse().map_err(|_| "bad --repeat")?,
            "--stats" => args.stats = true,
            "--quiet" => args.quiet = true,
            "-h" | "--help" => {
                println!("{}", include_str!("kconf_usage.txt"));
                std::process::exit(0);
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => positional = Some(other.to_string()),
        }
    }
    if let Some(top) = positional {
        args.top = top;
    }
    Ok(args)
}

fn main() -> ExitCode {
    // Finalization recurses over the menu tree, which can nest deeply in large
    // trees, so run on a thread with plenty of stack.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(run)
        .expect("failed to spawn worker thread");
    handle.join().unwrap_or(ExitCode::FAILURE)
}

fn run() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("kconf: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut best: Option<(f64, f64, Kconfig, String)> = None;
    for _ in 0..args.repeat.max(1) {
        let options = LoadOptions {
            top_file: args.top.clone(),
            shell_cache: args.shell_cache.clone(),
            probe_jobs: args
                .probe_jobs
                .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get())),
            warn: !args.quiet,
        };

        let start = Instant::now();
        let kconf = match Kconfig::load(options) {
            Ok(kconf) => kconf,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        let load_secs = start.elapsed().as_secs_f64();

        let start = Instant::now();
        let mut values = Values::new(&kconf);
        let header = kconf.config_header.clone();
        let contents = kconfig::write::config_contents(&kconf, &mut values, &header);
        let eval_secs = start.elapsed().as_secs_f64();

        eprintln!(
            "load {:.3}s  eval+write {:.3}s  total {:.3}s",
            load_secs,
            eval_secs,
            load_secs + eval_secs
        );
        if best
            .as_ref()
            .is_none_or(|(l, e, _, _)| load_secs + eval_secs < l + e)
        {
            best = Some((load_secs, eval_secs, kconf, contents));
        }
    }

    let Some((load_secs, eval_secs, kconf, contents)) = best else {
        return ExitCode::FAILURE;
    };

    eprintln!(
        "BEST load={:.3} eval+write={:.3} total={:.3}",
        load_secs,
        eval_secs,
        load_secs + eval_secs
    );

    if args.stats {
        let t = kconf.timings;
        eprintln!(
            "timings: probes={:.3}s parse={:.3}s finalize={:.3}s dep-check={:.3}s \
             eval+write={:.3}s  ({:.2} MiB of Kconfig in {} files, {:.0} MiB/s through the \
             parser, of which {:.3}s was file I/O)",
            t.probes,
            t.parse,
            t.finalize,
            t.dep_check,
            eval_secs,
            t.bytes_read as f64 / (1024.0 * 1024.0),
            kconf.kconfig_filenames.len(),
            t.bytes_read as f64 / (1024.0 * 1024.0) / t.parse.max(1e-9),
            t.file_read,
        );
        eprintln!(
            "symbols={} unique_defined={} choices={} menus={} comments={} nodes={} \
             exprs={} strings={} files={} config_bytes={}",
            kconf.symbols.len(),
            kconf.unique_defined_syms.len(),
            kconf.unique_choices.len(),
            kconf.menu_nodes.len(),
            kconf.comment_nodes.len(),
            kconf.nodes.len(),
            kconf.exprs.len(),
            kconf.interner.len(),
            kconf.kconfig_filenames.len(),
            contents.len(),
        );
        let s = kconf.shell_stats;
        eprintln!(
            "shell probes: {} executed, {} cached, {} pre-warmed in parallel",
            s.misses, s.hits, s.warmed
        );
    }

    if !args.quiet {
        for warning in &kconf.warnings {
            eprintln!("warning: {warning}");
        }
    }

    match args.write_config.as_deref() {
        Some("-") => print!("{contents}"),
        Some(path) => {
            if let Err(e) = std::fs::write(path, &contents) {
                eprintln!("kconf: could not write {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => {}
    }
    ExitCode::SUCCESS
}
