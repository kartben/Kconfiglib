# Can a native rewrite load Kconfig 10x faster?

**Question.** Would rewriting Kconfiglib's loader in Rust or Go make loading a
full Linux or Zephyr Kconfig at least ten times faster, and would the result be
more readable than the Python?

**Short answer.** Yes on both counts, but the headline number is not where you
would expect it, and part of it belongs to the Python.

Two thirds of a Linux Kconfig load is not parsing at all: it is a hundred
`fork`/`exec` compiler probes. Caching those, and keeping the cyclic garbage
collector out of the parse, are **two patches to `kconfiglib.py`** worth 2.9x on
Linux and 1.18x on Zephyr — no rewrite involved. They are in this branch.

Measured against Kconfiglib *with* those patches, the Rust loader is **8.7x** on
Linux and **7.6x** on Zephyr — a consistent order of magnitude, on the work that
is actually parsing. Against Kconfiglib as it was, Linux reads 26x, but that
number is mostly the probe cache, which the Python now has too.

Rust is the right language for the native side; Go measures 1.5–1.7x slower on
the dominant loop and has a materially worse story for the Python interop Zephyr
requires. And the Python cannot close the remaining gap on its own —
[eleven other things were tried](#what-the-python-can-get-on-its-own) and none of
them moved it.

Everything below is measured against a working prototype in
[`kconfig-rs/`](../), which loads both trees and produces `.config` output
byte-identical to Kconfiglib's.

---

## How this was measured

| | |
|---|---|
| Machine | 4-core Intel Xeon @ 2.80 GHz, 15 GiB RAM, Linux 6.18, cloud VM |
| Linux tree | v6.12, `ARCH=x86`, 1605 files / 5.47 MiB / 186k lines sourced |
| Zephyr tree | `main` @ 8dafb9a89, documentation mode (all 1142 boards, 137 SoCs, 11 arches), 6044 files / 6.04 MiB / 228k lines sourced |
| Python | CPython 3.11.15 |
| Rust | 1.94.1, `--release`, thin LTO |
| Go | 1.24.7 |
| Baseline | Kconfiglib 14.1.0 (Zephyr's fork, which adds `configdefault`) |

"Load" means: parse every file, build the menu tree, propagate dependencies,
build the reverse dependency graph and check it for loops, then evaluate every
symbol and render a `.config`. Both implementations do all of it. Timings are
the best of seven runs with a warm page cache; the VM is noisy, so treat
everything as ±10%.

Reproduce with `bench/bench.sh`.

---

## Finding 1: most of a Linux load is not parsing

Profiling Kconfiglib on the Linux tree puts 1.64 s of a 2.57 s load inside
`select.poll` — waiting on subprocesses. `scripts/Kconfig.include` defines
`cc-option`, `as-instr`, `ld-option` and friends in terms of `$(shell,...)`, and
Kconfig lines like

```
depends on $(cc-option,-fsanitize=kcfi)
```

expand to a shell that runs the compiler. A full x86 load fires **103 of them,
102 distinct, 1.64 s in total, median 16 ms each**.

That reframes the whole question. Any implementation that keeps the probes
serial is bounded below by 1.6 s no matter how fast it parses. The C
implementation is the proof: `scripts/kconfig/conf --allnoconfig` on the same
tree takes **2.06 s**, only 1.25x faster than Python.

The probes are pure functions of the command string and the toolchain, so the
prototype treats them as such: results are memoized within a run, persisted
across runs keyed by a fingerprint of the environment variables they read, and —
when that fingerprint changes — the commands recorded by the previous run are
re-executed across all cores before parsing starts. A command that was never
recorded still runs synchronously the first time it is asked for, so the cache
is an optimization and never a correctness dependency.

This is the single highest-value change in the whole investigation, and it is
not a rewrite — so it is now **also in `kconfiglib.py`** on this branch, behind
`KCONFIG_SHELL_CACHE`. A warm cache takes a full kernel load from 2.43 s to
0.83 s.

## Finding 2: the parsing work itself is worth about 10x

Zephyr fires zero shell probes, so its numbers are the clean measurement of
parser and evaluator speed.

### Linux v6.12, `ARCH=x86`

| | total | breakdown |
|---|---|---|
| Kconfiglib, as it was | **2.432 s** | parse 2.364, eval+write 0.068, of which `$(shell)` ~1.6 |
| C `conf --allnoconfig` | 2.058 s | includes the same probes |
| Kconfiglib, patched | 2.301 s | the GC patch only; probes still run |
| Kconfiglib, patched, probe cache warm | **0.830 s** | parse 0.702, eval+write 0.128 |
| `kconf`, cold probe cache | 1.674 s | load 1.665, eval+write 0.009 |
| `kconf`, warm probe cache | **0.095 s** | load 0.088, eval+write 0.007 |

Like for like — both implementations with a warm probe cache — the Rust is
**8.7x**. Against Kconfiglib as it was it reads 26x, but most of that gap is the
probe cache rather than the language.

### Zephyr, all boards

| | total | breakdown |
|---|---|---|
| Kconfiglib, as it was | **1.679 s** | parse 1.591, eval+write 0.088 |
| Kconfiglib, patched | **1.410 s** | parse 1.224, eval+write 0.187 |
| `kconf` | **0.186 s** | load 0.174, eval+write 0.012 |

**7.6x** against the patched Python, 9.0x against the original. Zephyr sits
lower than Linux because it sources 6044 files to Linux's 1605, and opening and
reading them costs 34 ms that no amount of parser speed removes — the same 34 ms
Kconfiglib pays.

The patched Python spends noticeably more of its time rendering (0.187 s against
0.088 s). That is the deferred cost of the GC patch: the first cyclic collection
after the parse is a full one, which is the point — one full collection over the
finished tree instead of six while it is being built.

### Where the remaining time goes

`kconf --stats` breaks it down. For Zephyr: parse 0.150 s (of which 0.034 s is
file I/O), finalize 0.024 s, dependency graph and loop check 0.014 s, evaluate
and write 0.012 s.

A `callgrind` profile puts roughly 20–25% of remaining instructions in
`malloc`/`free`. Most of that is the per-symbol and per-node property lists —
thousands of two-element `Vec`s. Moving them into a shared arena would plausibly
buy another 15%, at some cost in directness. It was left undone: the code being
easy to follow is part of what was being evaluated.

### Memory

| | Kconfiglib | `kconf` |
|---|---|---|
| Linux | 54 MiB | 30 MiB |
| Zephyr | 81 MiB | 34 MiB |

## Finding 3: Rust over Go, and not only on speed

To compare the languages rather than two different programs, the tokenizer was
written twice — [`src/bin/lexbench.rs`](../src/bin/lexbench.rs) and
[`bench/lex-go/main.go`](../bench/lex-go/main.go) — with the same keyword table,
the same character classes, the same string interning, and the same file walk.
Both produce identical token and symbol counts, so they are doing identical
work.

| corpus | Rust | Go | |
|---|---|---|---|
| Linux tree, 1811 files, 6.39 MiB | 0.035 s (184 MiB/s) | 0.051 s (126 MiB/s) | Rust 1.46x |
| Zephyr tree, 7566 files, 6.23 MiB | 0.065 s (97 MiB/s) | 0.110 s (56 MiB/s) | Rust 1.69x |

Go lands at roughly 1.5–1.7x of Rust's time on the loop that dominates a load,
which would put a Go port at about 5–6x over Python on Zephyr — short of the
target — and still comfortably past it on Linux once probes are cached.

Speed is not the deciding factor, though. Three other things are:

- **Python interop.** Kconfiglib is a library first. Zephyr's build, its
  documentation generator, `menuconfig`, and esp-idf all import it. A
  replacement has to be importable from Python, and Zephyr additionally
  needs Kconfig to *call back into* Python (see the next section). Rust plus
  PyO3 does both cheaply and links statically. cgo callbacks are slower and
  the Go runtime in a `c-shared` library is awkward to embed.
- **Modelling the AST.** Kconfig expressions are a five-case sum type. Rust
  gives you `enum Expr { Sym, Not, And, Or, Cmp }` with exhaustive matching;
  Go gives you an interface with type switches or a struct with a tag and
  unused fields. This is the code you read most while working on a Kconfig
  implementation.
- **Arenas without a GC.** The menu tree is cyclic (parent, next, child, plus
  symbols pointing back at their nodes). The prototype models it as arenas of
  `u32` indices, which is both the fast option and the readable one. Go would
  reach for real pointers and hand the GC a large object graph to trace.

## Finding 4: Zephyr's Python hooks are the real adoption blocker

Zephyr's Kconfig calls Python. `KCONFIG_FUNCTIONS` points Kconfiglib at
`scripts/kconfig/kconfigfunctions.py`, which registers about 70 preprocessor
functions that query the devicetree:

```
config FOO
	default y if $(dt_nodelabel_enabled,uart0)
```

There are **thousands of such call sites** across the Zephyr tree —
`dt_nodelabel_enabled` alone appears 473 times. They are backed by `edtlib`, a
substantial Python devicetree library loaded from a pickle produced earlier in
the build.

Three ways out, in increasing order of work:

1. **Keep Python in the loop.** Expose the loader as a Python extension and let
   it call back for unknown preprocessor functions. Cheapest, keeps Zephyr's
   `kconfigfunctions.py` as the source of truth, and costs one FFI hop per call
   — negligible against the ~11k macro-bearing lines in the tree.
2. **Port the devicetree layer too.** Much larger, and duplicates a moving
   target.
3. **Precompute.** Have Zephyr's build resolve every `dt_*` call into a
   generated Kconfig fragment before the load. Architecturally the cleanest and
   the biggest change to Zephyr.

Option 1 is what makes an incremental adoption possible at all.

The prototype takes a fourth path, valid only for benchmarking: it implements
the pure helpers (`normalize_upper`, `substring`, the `add`/`inc`/`div` family)
natively and returns the same constants Zephyr's own code returns under
`KCONFIG_DOC_MODE=1`, which is the mode its documentation build uses. That is
enough to load the whole all-boards tree exactly as the doc build does, and it
is why the outputs match byte for byte — but it is not enough for a real build.

## Finding 5: Kconfiglib is behind upstream Kconfig

Not part of the original question, but it turned up immediately. Current
`torvalds/linux` master does not load:

```
arch/Kconfig:972: error: couldn't parse 'transitional': syntax error
```

`transitional` is a symbol property added to upstream Kconfig that Kconfiglib
does not implement, so the benchmarks here use v6.12. Whatever happens with a
rewrite, this needs fixing in the Python — and it is a good first exercise for
any port, since it touches the lexer, the parser and the evaluator.

---

## What makes the prototype fast

None of it is exotic. In rough order of contribution:

1. **Cache the shell probes** (Linux: 1.64 s → 0). Not a language issue at all.
2. **Read files whole and scan bytes.** Kconfiglib runs several compiled
   regexes per line; the prototype walks bytes and uses `memchr` for
   line-splitting and special-character detection. The tokenizer runs at
   ~180 MiB/s standalone.
3. **Intern every name to a `u32`.** Symbol names, file paths, prompts.
4. **Hash-cons expressions.** Dependency propagation ANDs a parent's condition
   into every property of every child, so the same subexpression is built over
   and over. Interning collapses Linux's construction traffic into 55k unique
   nodes, which makes both memory and evaluation cheaper.
5. **Memoize evaluation per expression, not per symbol.** Only possible because
   of (4): one identical condition propagated into two hundred symbols has one
   cache slot.
6. **Reuse buffers.** One line buffer, one help-text buffer, a small pool of
   file buffers recycled as `source` statements come and go.
7. **Skip the globber for literal paths.** `source "drivers/Kconfig"` needs one
   `open`, not a pattern walk. This alone was 30% of the Zephyr load before it
   was fixed — the kind of thing that only shows up under `strace`.

## What makes it more readable

The Python it is ported from is dense and heavily micro-optimized, with comments
that say so ("Micro-optimization. This code is pretty hot.") The Rust is not
faster because it is cleverer; it is faster because the language makes the plain
version fast. Concretely:

- **Arenas and typed indices instead of object graphs.** `SymbolId`, `NodeId`,
  `ExprId` are `u32` newtypes. The menu tree's cycles need no `Rc<RefCell<>>`,
  no weak references, and no ownership puzzles.
- **A real sum type for expressions.** `Expr` is a five-variant enum. The Python
  represents expressions as tuples whose first element is an integer token
  constant, with the module docstring explaining the encoding; `expr_value()`
  dispatches on `expr[0]` and indexes `expr[1]`/`expr[2]`.
- **Immutable model, separate mutable evaluation state.** `Kconfig` is the
  parsed tree; `Values` holds every cache. In Kconfiglib the caches are fields
  on `Symbol` (`_cached_tri_val`, `_cached_vis`, `_cached_str_val`,
  `_write_to_conf`, `_visited`) mutated by property getters, with comments
  warning that reading `self.visibility` is "a hidden function call (property
  magic)".
- **Simpler invalidation.** Kconfiglib maintains, for each symbol, the set of
  symbols whose value might change with it, then walks that set on every
  assignment — `_build_dep`, `_rec_invalidate`,
  `_rec_invalidate_if_has_prompt`, and a `Symbol._dependents` set per symbol.
  The prototype drops all cached values instead. Recomputing every symbol in
  the Linux tree *and* re-rendering the whole `.config` costs 7 ms, so the
  bookkeeping only pays for itself inside an interactive `menuconfig` loop — and
  even there, 7 ms is one keystroke. (The dependency graph is still built,
  because loop detection needs it.)
- **Errors as values.** Every fallible path returns `Result`; there is no
  equivalent of Kconfiglib's `_parse_error` reaching into instance state to
  reconstruct where it was.

For calibration, and not in the rewrite's favour: the Rust library is 4.9k
lines, 3.8k of them non-comment, against `kconfiglib.py`'s 7.2k lines — of which
about 2.4k are actual code once comments and docstrings come out. So this is
roughly 1.5x the code for a subset of the functionality. Some of that is Rust
being more explicit, some is machinery Kconfiglib has no equivalent for (the
probe cache, the Zephyr function shims, the compressed dependency graph). The
claim here is not that the Rust is shorter. It is that each piece of it says
what it does.

## What is not implemented yet

The prototype covers loading and `.config` output. Missing, roughly in order of
how much work each is:

- Reading `.config` files back in (`load_config`, `load_allconfig`), and the
  assignment warnings that go with them.
- `write_autoconf`, `sync_deps`, `write_min_config` / `savedefconfig`.
- A public `set_value` / `unset_value` API. The model has the user-value fields
  and cache invalidation; there is no setter on top of them.
- `eval_string`, `expr_str`, and the `__str__`/`__repr__` renderings that
  documentation generators use.
- Warning parity: `_check_sym_sanity`, `_check_choice_sanity`, unsatisfied
  `select` warnings, `KCONFIG_WARN_UNDEF`.
- Dependency loops are detected and rejected, but the error names the cycle
  rather than reproducing Kconfiglib's full annotated report.
- `menuconfig` / `guiconfig`.
- A Python module wrapping any of it.
- `transitional` (see Finding 5) — missing from Kconfiglib too.

## What adoption would take

The prototype is roughly two thirds of a loader and about a fifth of
Kconfiglib's total surface. A usable replacement is a matter of months, not
weeks, and the parser was never the risky part — the API surface and Zephyr's
Python hooks are.

If it were to be pursued, the order that keeps it useful throughout:

1. ~~**Cache the shell probes in `kconfiglib.py`.**~~ Done on this branch,
   along with keeping the cyclic collector out of the parse. Together: 2.9x on
   the kernel, 1.18x on Zephyr, byte-identical output. Do this regardless of
   what happens to the rest.
2. **Finish the loader** — `.config` reading, `set_value`, warning parity.
3. **Ship it as a Python extension behind Kconfiglib's API**, with the callback
   hook for `KCONFIG_FUNCTIONS`. Not a fork: an optional accelerator that the
   existing scripts can opt into, falling back to the Python when anything is
   unsupported.
4. **Differential-test it in CI** — `bench/parity.py` generalized to run both
   implementations over Zephyr's boards and a spread of kernel versions and
   diff the output. Byte-identical `.config` is a strong, cheap invariant.
5. Only then consider what the native implementation could offer that the
   Python cannot: parallel probe evaluation, a persistent parse cache, a
   language server for Kconfig files.

## Verifying the claims

```
cargo test                                          # 27 tests
bench/parity.py                                     # every fixture vs Kconfiglib
bench/bench.sh linux  /path/to/linux                # the tables above
bench/bench.sh zephyr /path/to/zephyr

bench/lex-python.py /path/to/tree                   # the Python tokenizer floor
target/release/lexbench /path/to/tree               # the same algorithm in Rust
(cd bench/lex-go && go run . /path/to/tree)         # and in Go
```

`bench.sh` diffs the two implementations' output on every run, so a regression
in correctness shows up in the same command that reports the speed.
