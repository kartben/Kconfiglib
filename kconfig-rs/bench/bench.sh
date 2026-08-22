#!/usr/bin/env bash
# Benchmarks the Rust loader against Kconfiglib and the C tools.
#
#   bench.sh linux  <path to a kernel tree>
#   bench.sh zephyr <path to a zephyr tree>
#
# The environment each tree needs must already be exported (see README.md).
set -u

KCONF=${KCONF:-"$(dirname "$0")/../target/release/kconf"}
REPEAT=${REPEAT:-5}
CACHE=$(mktemp -u)

run_python() {  # $1 = directory holding kconfiglib.py
  KCONFIGLIB_DIR="$1" PYTHONPATH="$1" python3 - "$REPEAT" <<'PY'
import os, sys, time
sys.path.insert(0, os.environ["KCONFIGLIB_DIR"])
import kconfiglib

n = int(sys.argv[1])
probe_time = [0.0]
original = kconfiglib._shell_fn
def timed(kconf, name, command):
    start = time.perf_counter()
    try:
        return original(kconf, name, command)
    finally:
        probe_time[0] += time.perf_counter() - start
kconfiglib._shell_fn = timed

best = None
for _ in range(n):
    probe_time[0] = 0.0
    t = time.perf_counter(); kc = kconfiglib.Kconfig("Kconfig", warn=False); parse = time.perf_counter() - t
    t = time.perf_counter(); out = kc._config_contents(None); ev = time.perf_counter() - t
    if best is None or parse + ev < best[0] + best[1]:
        best = (parse, ev, probe_time[0])
open("/tmp/bench-python.config", "w").write(out)
print("  kconfiglib   total %6.3fs   (parse %.3f  eval+write %.3f  of which $(shell) %.3f)"
      % (best[0] + best[1], best[0], best[1], best[2]))
PY
}

case "${1:-}" in
  linux)  PYLIB=${PYLIB:-/home/user/Kconfiglib} ;;
  zephyr) PYLIB=${PYLIB:-"$2/scripts/kconfig"} ;;
  *) echo "usage: $0 {linux|zephyr} <tree>" >&2; exit 2 ;;
esac
cd "$2" || exit 1

echo "== $1 =="
run_python "$PYLIB"

echo -n "  kconf cold   "
rm -f "$CACHE"
"$KCONF" --quiet --shell-cache "$CACHE" --write-config /tmp/bench-rust.config Kconfig 2>&1 | tail -1 \
  | sed 's/BEST load=\(.*\) eval+write=\(.*\) total=\(.*\)/total  \3s   (load \1  eval+write \2)/'

echo -n "  kconf warm   "
"$KCONF" --quiet --shell-cache "$CACHE" --repeat "$REPEAT" --write-config /tmp/bench-rust.config Kconfig 2>&1 | tail -1 \
  | sed 's/BEST load=\(.*\) eval+write=\(.*\) total=\(.*\)/total  \3s   (load \1  eval+write \2)/'

if [ -x scripts/kconfig/conf ]; then
  best=""
  for _ in $(seq "$REPEAT"); do
    s=$(python3 -c 'import time; print(time.perf_counter())')
    ./scripts/kconfig/conf --allnoconfig Kconfig >/dev/null 2>&1
    e=$(python3 -c 'import time; print(time.perf_counter())')
    best=$(python3 -c "print(min($e-$s, $best) if '$best' else $e-$s)")
  done
  printf "  C conf       total %6.3fs   (--allnoconfig, includes the same \$(shell) probes)\n" "$best"
fi

if cmp -s /tmp/bench-python.config /tmp/bench-rust.config; then
  echo "  output: byte-identical to Kconfiglib"
else
  echo "  output: DIFFERS from Kconfiglib"; diff /tmp/bench-python.config /tmp/bench-rust.config | head -20
fi
rm -f "$CACHE"
