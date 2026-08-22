//! A Kconfig loader.
//!
//! This is an experimental Rust port of the parsing and evaluation core of
//! [Kconfiglib](https://github.com/zephyrproject-rtos/Kconfiglib). It reads a
//! Kconfig tree, builds the menu tree, propagates dependencies, and computes
//! symbol values, producing `.config` output byte-for-byte identical to
//! Kconfiglib's for the Linux and Zephyr trees.
//!
//! # Shape of the loader
//!
//! ```text
//!   Kconfig files
//!        │
//!        │  lexer.rs      one line at a time, with $(...) expanded by
//!        ▼                preprocess.rs as it goes
//!   token stream
//!        │
//!        │  parser.rs     recursive descent; `source` recurses into files
//!        ▼
//!   menu tree + symbols   arenas of MenuNode / Symbol / Choice (model.rs)
//!        │                expressions are hash-consed (expr.rs)
//!        │  finalize.rs   propagate deps, build implicit menus, dissolve
//!        ▼                `if` nodes, resolve choices
//!   finished Kconfig
//!        │
//!        │  eval.rs       memoized visibility / tristate / string values
//!        ▼
//!   .config              write.rs
//! ```
//!
//! # Why it is fast
//!
//! * Files are read whole and scanned as bytes; no per-line regex matching.
//! * Symbol names and file paths are interned to `u32` handles.
//! * Expressions are hash-consed, so dependency propagation — which ANDs a
//!   parent condition into every child property — reuses nodes instead of
//!   rebuilding them, and evaluation memoizes per expression rather than per
//!   symbol.
//! * `$(shell,...)` compiler probes, which dominate a cold Linux load, are
//!   memoized on disk and re-probed in parallel when the toolchain changes.

pub mod deps;
pub mod eval;
pub mod expr;
pub mod finalize;
pub mod intern;
pub mod lexer;
pub mod model;
pub mod parser;
pub mod preprocess;
pub mod shell;
pub mod write;
pub mod zephyr;

use std::path::PathBuf;

use rustc_hash::FxHashMap;

use crate::expr::{ExprArena, ExprId};
use crate::intern::{Interner, StrId};
use crate::model::{Choice, ChoiceId, Item, Loc, MenuNode, NodeId, Symbol, SymbolId, Type};
use crate::preprocess::Preprocessor;
use crate::shell::{ShellCache, ShellStats};

/// Options for [`Kconfig::load`].
pub struct LoadOptions {
    /// Path to the top-level Kconfig file, relative to `$srctree`.
    pub top_file: String,
    /// Where to persist `$(shell,...)` results. `None` disables the cache.
    pub shell_cache: Option<PathBuf>,
    /// Threads to use when re-probing a stale shell cache.
    pub probe_jobs: usize,
    /// Collect warnings instead of dropping them.
    pub warn: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            top_file: "Kconfig".to_string(),
            shell_cache: None,
            probe_jobs: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            warn: true,
        }
    }
}

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A loaded Kconfig tree.
pub struct Kconfig {
    pub interner: Interner,
    pub exprs: ExprArena,
    pub symbols: Vec<Symbol>,
    pub choices: Vec<Choice>,
    pub nodes: Vec<MenuNode>,

    /// Non-constant symbols, by name.
    pub sym_ids: FxHashMap<StrId, SymbolId>,
    /// Constant symbols (quoted strings and `n`/`m`/`y`), by name.
    pub const_ids: FxHashMap<StrId, SymbolId>,
    pub named_choices: FxHashMap<StrId, ChoiceId>,

    pub top_node: NodeId,
    /// Symbols in definition order, one entry per `config` block.
    pub defined_syms: Vec<SymbolId>,
    /// [`Self::defined_syms`] with duplicates removed, order preserved.
    pub unique_defined_syms: Vec<SymbolId>,
    pub unique_choices: Vec<ChoiceId>,
    pub menu_nodes: Vec<NodeId>,
    pub comment_nodes: Vec<NodeId>,

    pub n: SymbolId,
    pub m: SymbolId,
    pub y: SymbolId,
    pub modules: SymbolId,
    pub defconfig_list: Option<SymbolId>,

    pub config_prefix: String,
    pub srctree: String,
    /// `$srctree` with a trailing separator, for making paths relative.
    pub srctree_prefix: String,
    pub config_header: String,

    pub kconfig_filenames: Vec<String>,
    pub warnings: Vec<String>,
    pub shell_stats: ShellStats,
    pub timings: Timings,

    /// Reverse dependency graph; see [`deps`].
    pub dependents: Option<deps::Dependents>,
}

/// Where the time went, for `kconf --timings`.
#[derive(Default, Debug, Clone, Copy)]
pub struct Timings {
    /// Running the `$(shell,...)` probes that were not already cached.
    pub probes: f64,
    /// Reading, tokenizing and parsing every Kconfig file.
    pub parse: f64,
    /// Dependency propagation and menu tree construction.
    pub finalize: f64,
    /// Building the reverse dependency graph and checking it for loops.
    pub dep_check: f64,
    /// Bytes of Kconfig read.
    pub bytes_read: u64,
    /// Part of `parse` spent opening and reading files.
    pub file_read: f64,
}

impl Kconfig {
    /// Reads and finalizes a Kconfig tree.
    pub fn load(options: LoadOptions) -> Result<Kconfig> {
        let mut kconf = Kconfig::empty();
        let start = std::time::Instant::now();
        let mut cache = ShellCache::new(options.shell_cache.clone());
        cache.warm_up(options.probe_jobs);
        let mut pp = Preprocessor::new(cache);
        let warmup = start.elapsed().as_secs_f64();

        let start = std::time::Instant::now();
        parser::parse(&mut kconf, &mut pp, &options.top_file)?;
        kconf.timings.parse = start.elapsed().as_secs_f64();

        let start = std::time::Instant::now();
        finalize::finalize(&mut kconf);
        kconf.timings.finalize = start.elapsed().as_secs_f64();

        let start = std::time::Instant::now();
        let dependents = deps::build_dependents(&kconf);
        deps::check_dependency_loops(&kconf, &dependents)?;
        kconf.dependents = Some(dependents);
        kconf.timings.dep_check = start.elapsed().as_secs_f64();
        kconf.timings.probes = warmup + pp.shell.elapsed;
        kconf.timings.parse -= pp.shell.elapsed;

        kconf.shell_stats = pp.shell.stats;
        pp.shell.save();
        if options.warn {
            kconf.warnings.append(&mut pp.warnings);
        }
        Ok(kconf)
    }

    fn empty() -> Kconfig {
        let srctree = std::env::var("srctree").unwrap_or_default();
        let srctree_prefix = if srctree.is_empty() {
            String::new()
        } else {
            let canonical = std::fs::canonicalize(&srctree)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| srctree.clone());
            format!("{canonical}/")
        };

        let mut interner = Interner::new();
        // n, m and y must be symbols 0, 1 and 2 so that ExprArena can seed
        // itself with them.
        let mut symbols = Vec::new();
        let placeholder = ExprId(0);
        for (i, name) in ["n", "m", "y"].iter().enumerate() {
            let mut sym = Symbol::new(interner.intern(name), true, placeholder);
            sym.kind = Type::Tristate;
            symbols.push(sym);
            debug_assert_eq!(i, symbols.len() - 1);
        }
        let (n, m, y) = (SymbolId(0), SymbolId(1), SymbolId(2));
        let exprs = ExprArena::new(n, m, y);
        for sym in &mut symbols {
            sym.direct_dep = exprs.n;
            sym.rev_dep = exprs.n;
            sym.weak_rev_dep = exprs.n;
        }

        let mut const_ids = FxHashMap::default();
        for (i, name) in ["n", "m", "y"].iter().enumerate() {
            const_ids.insert(interner.intern(name), SymbolId(i as u32));
        }

        let top_loc = Loc {
            file: interner.intern(""),
            line: 1,
        };
        let mut top = MenuNode::new(Item::Menu, None, top_loc, exprs.y);
        top.is_menuconfig = true;
        top.prompt = Some((interner.intern("Main menu"), exprs.y));
        let nodes = vec![top];

        // Real trees run to tens of thousands of nodes and symbols; starting
        // with room for them keeps the arenas from being copied as they grow.
        symbols.reserve(16 * 1024);
        let mut nodes = nodes;
        nodes.reserve(16 * 1024);

        let mut kconf = Kconfig {
            interner,
            exprs,
            symbols,
            choices: Vec::new(),
            nodes,
            sym_ids: FxHashMap::default(),
            const_ids,
            named_choices: FxHashMap::default(),
            top_node: NodeId(0),
            defined_syms: Vec::new(),
            unique_defined_syms: Vec::new(),
            unique_choices: Vec::new(),
            menu_nodes: Vec::new(),
            comment_nodes: Vec::new(),
            n,
            m,
            y,
            modules: SymbolId(0),
            defconfig_list: None,
            config_prefix: std::env::var("CONFIG_").unwrap_or_else(|_| "CONFIG_".to_string()),
            srctree,
            srctree_prefix,
            config_header: std::env::var("KCONFIG_CONFIG_HEADER").unwrap_or_default(),
            kconfig_filenames: Vec::new(),
            warnings: Vec::new(),
            shell_stats: ShellStats::default(),
            timings: Timings::default(),
            dependents: None,
        };
        kconf.modules = kconf.lookup_sym("MODULES");
        kconf
    }

    /// Returns the symbol named `name`, creating it if it does not exist yet.
    pub fn lookup_sym(&mut self, name: &str) -> SymbolId {
        let id = self.interner.intern(name);
        if let Some(&sym) = self.sym_ids.get(&id) {
            return sym;
        }
        let sym = SymbolId(self.symbols.len() as u32);
        self.symbols.push(Symbol::new(id, false, self.exprs.n));
        self.sym_ids.insert(id, sym);
        sym
    }

    /// Returns the constant symbol for the literal `value`, creating it if
    /// needed. Constant symbols carry their name as their value.
    pub fn lookup_const_sym(&mut self, value: &str) -> SymbolId {
        let id = self.interner.intern(value);
        if let Some(&sym) = self.const_ids.get(&id) {
            return sym;
        }
        let sym = SymbolId(self.symbols.len() as u32);
        // Constant symbols keep the Unknown type, which is what makes their
        // value their own name.
        self.symbols.push(Symbol::new(id, true, self.exprs.n));
        self.const_ids.insert(id, sym);
        sym
    }

    #[inline]
    pub fn sym(&self, id: SymbolId) -> &Symbol {
        &self.symbols[id.index()]
    }

    #[inline]
    pub fn sym_mut(&mut self, id: SymbolId) -> &mut Symbol {
        &mut self.symbols[id.index()]
    }

    #[inline]
    pub fn choice(&self, id: ChoiceId) -> &Choice {
        &self.choices[id.index()]
    }

    #[inline]
    pub fn choice_mut(&mut self, id: ChoiceId) -> &mut Choice {
        &mut self.choices[id.index()]
    }

    #[inline]
    pub fn node(&self, id: NodeId) -> &MenuNode {
        &self.nodes[id.index()]
    }

    #[inline]
    pub fn node_mut(&mut self, id: NodeId) -> &mut MenuNode {
        &mut self.nodes[id.index()]
    }

    #[inline]
    pub fn name(&self, id: SymbolId) -> &str {
        &self.interner[self.symbols[id.index()].name]
    }
}
