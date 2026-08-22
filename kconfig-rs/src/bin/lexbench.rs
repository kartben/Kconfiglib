//! A like-for-like lexing benchmark, used to compare Rust against the Go
//! implementation in `bench/lex-go` on the phase that dominates a Kconfig load.
//!
//! It walks a source tree, reads every Kconfig file, and tokenizes each line
//! with the same rules the real loader uses — keyword lookup, identifier and
//! string scanning, and interning identifiers into a symbol table. It does not
//! resolve `source` directives, expand macros, or build a tree: the point is to
//! measure the shared inner loop, not to be a Kconfig implementation.
//!
//!     lexbench <tree> [iterations]

use std::path::PathBuf;
use std::time::Instant;

use kconfig::intern::Interner;

/// The same keyword table, with the same numbering, as `bench/lex-go`, so the
/// two programs classify every token identically.
fn keyword_id(word: &str) -> Option<u32> {
    Some(match word {
        "---help---" => 1,
        "allnoconfig_y" => 2,
        "bool" | "boolean" => 3,
        "choice" => 4,
        "comment" => 5,
        "config" => 6,
        "configdefault" => 7,
        "def_bool" => 8,
        "def_hex" => 9,
        "def_int" => 10,
        "def_string" => 11,
        "def_tristate" => 12,
        "default" => 13,
        "defconfig_list" => 14,
        "depends" => 15,
        "endchoice" => 16,
        "endif" => 17,
        "endmenu" => 18,
        "env" => 19,
        "grsource" | "orsource" => 20,
        "gsource" | "osource" => 21,
        "help" => 22,
        "hex" => 23,
        "if" => 24,
        "imply" => 25,
        "int" => 26,
        "mainmenu" => 27,
        "menu" => 28,
        "menuconfig" => 29,
        "modules" => 30,
        "on" => 31,
        "option" => 32,
        "optional" => 33,
        "prompt" => 34,
        "range" => 35,
        "rsource" => 36,
        "select" => 37,
        "source" => 38,
        "string" => 39,
        "tristate" => 40,
        "visible" => 41,
        _ => return None,
    })
}

#[inline]
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'/' | b'.' | b'-')
}

#[inline]
fn is_command_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'-')
}

#[inline]
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn skip_spaces(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && is_space(bytes[i]) {
        i += 1;
    }
    i
}

/// Keywords after which a bare word is a string rather than a symbol.
fn expects_string(id: u32) -> bool {
    matches!(
        id,
        3 | 4 | 5 | 23 | 26 | 27 | 28 | 20 | 21 | 34 | 36 | 38 | 39 | 40
    )
}

#[derive(Default)]
struct Counts {
    lines: usize,
    tokens: usize,
    symbols: usize,
}

fn tokenize(line: &str, syms: &mut Interner, c: &mut Counts) {
    c.lines += 1;
    let bytes = line.as_bytes();
    let start = skip_spaces(bytes, 0);
    let mut end = start;
    while end < bytes.len() && is_command_byte(bytes[end]) {
        end += 1;
    }
    if end == start {
        return;
    }
    let Some(kw) = keyword_id(&line[start..end]) else {
        return;
    };
    c.tokens += 1;
    let mut prev = kw;
    let mut i = skip_spaces(bytes, end);

    while i < bytes.len() {
        let ch = bytes[i];
        if is_ident_byte(ch) {
            let mut end = i;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                end += 1;
            }
            let word = &line[i..end];
            match keyword_id(word) {
                Some(k) => prev = k,
                None => {
                    if !expects_string(prev) {
                        syms.intern(word);
                        c.symbols += 1;
                    }
                    prev = 0;
                }
            }
            c.tokens += 1;
            i = skip_spaces(bytes, end);
            continue;
        }

        if ch == b'"' || ch == b'\'' {
            let Some(rel) = memchr::memchr(ch, &bytes[i + 1..]) else {
                return;
            };
            let stop = i + 1 + rel + 1;
            if !expects_string(prev) {
                syms.intern(&line[i + 1..stop - 1]);
            }
            prev = 0;
            c.tokens += 1;
            i = skip_spaces(bytes, stop);
            continue;
        }

        let width = match ch {
            b'&' if bytes.get(i + 1) == Some(&b'&') => 2,
            b'|' if bytes.get(i + 1) == Some(&b'|') => 2,
            b'!' if bytes.get(i + 1) == Some(&b'=') => 2,
            b'<' if bytes.get(i + 1) == Some(&b'=') => 2,
            b'>' if bytes.get(i + 1) == Some(&b'=') => 2,
            b'#' => return,
            b'=' | b'!' | b'(' | b')' | b'<' | b'>' => 1,
            _ => return,
        };
        prev = 0;
        c.tokens += 1;
        i = skip_spaces(bytes, i + width);
    }
}

fn kconfig_files(root: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            match entry.file_type() {
                Ok(t) if t.is_dir() => {
                    if name != ".git" {
                        stack.push(path);
                    }
                }
                Ok(_) if name.starts_with("Kconfig") => files.push(path),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        eprintln!("usage: lexbench <tree> [iterations]");
        std::process::exit(2);
    };
    let iterations: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(5);

    let files = kconfig_files(&root);
    let mut best = f64::INFINITY;
    let mut counts = Counts::default();
    let mut bytes_read = 0usize;

    for _ in 0..iterations {
        let mut syms = Interner::new();
        counts = Counts::default();
        bytes_read = 0;
        let start = Instant::now();
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            bytes_read += text.len();
            for line in text.split_inclusive('\n') {
                tokenize(line, &mut syms, &mut counts);
            }
        }
        best = best.min(start.elapsed().as_secs_f64());
        counts.symbols = syms.len();
    }

    let mib = bytes_read as f64 / (1024.0 * 1024.0);
    println!(
        "rust  {} files  {:.2} MiB  {} lines  {} tokens  {} unique names  best {:.3}s ({:.0} MiB/s)",
        files.len(),
        mib,
        counts.lines,
        counts.tokens,
        counts.symbols,
        best,
        mib / best,
    );
}
