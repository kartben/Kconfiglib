# kconfig-rs

An experimental Rust loader for Kconfig trees — a port of the parsing and
evaluation core of [Kconfiglib](../kconfiglib.py), built to find out how much
faster loading a full Linux or Zephyr Kconfig can get, and whether the result
reads better than the Python it came from.

It loads the complete Linux and Zephyr trees and writes `.config` output that is
**byte-identical to Kconfiglib's**, in about a tenth of the time — and about a
thirtieth for Linux once the compiler probes are cached. See
[docs/INVESTIGATION.md](docs/INVESTIGATION.md) for the measurements, the design
decisions behind them, and what adopting this would actually take.

This is a prototype, not a replacement. It covers loading and `.config` output;
it does not yet read `.config` files back in, drive a `menuconfig`, or expose a
Python API. [What is not implemented](docs/INVESTIGATION.md#what-is-not-implemented-yet)
has the full list.

## Building

```
cargo build --release
```

No build script, no code generation, three small dependencies (`glob`, `memchr`,
`rustc-hash`).

## Using it

```
$ export srctree=/path/to/linux ARCH=x86 SRCARCH=x86 KERNELVERSION=6.12 CC=gcc LD=ld
$ ./target/release/kconf --write-config .config --shell-cache .kconfig-probe-cache Kconfig
```

| Option | |
|---|---|
| `--write-config <path>` | write a `.config` (`-` for stdout) |
| `--shell-cache <path>` | persist `$(shell,...)` results across runs |
| `--probe-jobs <n>` | threads for re-probing a stale cache |
| `--repeat <n>` | load `n` times, report the fastest |
| `--stats` | symbol/expression counts and a phase breakdown |
| `--quiet` | suppress warnings |

`--shell-cache` matters on Linux and nowhere else: `scripts/Kconfig.include`
turns about a hundred `depends on` lines into compiler probes, and running them
is roughly two thirds of a cold load. See
[the investigation](docs/INVESTIGATION.md#finding-1-most-of-a-linux-load-is-not-parsing).

## Layout

| File | |
|---|---|
| `src/lexer.rs` | line tokenizer; context-sensitive, one token of lookback |
| `src/preprocess.rs` | `$(...)` macro expansion and variable assignment |
| `src/parser.rs` | recursive descent, file stack, `source` globbing |
| `src/model.rs` | symbols, choices, menu nodes — arenas indexed by `u32` |
| `src/expr.rs` | hash-consed expression arena |
| `src/finalize.rs` | dependency propagation, implicit menus, choice resolution |
| `src/deps.rs` | reverse dependency graph and loop detection |
| `src/eval.rs` | memoized visibility, tristate and string values |
| `src/write.rs` | `.config` rendering |
| `src/shell.rs` | `$(shell,...)` execution, cached and pre-warmed in parallel |
| `src/zephyr.rs` | Zephyr's `kconfigfunctions` helpers, for `KCONFIG_DOC_MODE` |

## Testing

```
cargo test                     # unit tests and end-to-end loader tests
bench/parity.py                # every Kconfiglib fixture, diffed against Kconfiglib
```

`bench/parity.py` is the real correctness check: it loads each of the fixtures
in [`../tests`](../tests) with both implementations and compares the `.config`
they produce. All 34 loadable fixtures match byte for byte, and both
implementations reject the same 19.

## Benchmarking

```
bench/bench.sh linux  /path/to/linux     # after exporting the tree's environment
bench/bench.sh zephyr /path/to/zephyr
```

Reports Kconfiglib, this loader cold and warm, and the C `conf` tool if it has
been built, then diffs the output. `bench/setup-zephyr-tree.py` generates the
files a Zephyr tree needs before it can be loaded outside CMake.

`bench/lex-go/` holds a Go implementation of the tokenizer used to compare the
two languages on the same work; `src/bin/lexbench.rs` is its Rust counterpart.
