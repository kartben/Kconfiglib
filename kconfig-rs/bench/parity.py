#!/usr/bin/env python3
"""Differential test: this loader against Kconfiglib, over every fixture.

For each Kconfig fixture in ../tests, both implementations load the file and
render a .config. A fixture passes when the bytes match, or when both
implementations reject it.

    bench/parity.py [--kconf PATH] [--kconfiglib DIR] [FIXTURE_DIR]
"""
import argparse
import difflib
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.dirname(HERE)


def load_with_kconfiglib(directory, name):
    """Returns (config_text, error). Exactly one is None."""
    sys.modules.pop("kconfiglib", None)
    os.environ["srctree"] = directory
    try:
        import kconfiglib
        kconf = kconfiglib.Kconfig(name, warn=False)
        return kconf._config_contents(None), None
    except Exception as e:
        message = str(e).strip()
        return None, message.splitlines()[-1] if message else type(e).__name__


def load_with_kconf(binary, directory, name):
    """Returns (config_text, error). Exactly one is None."""
    env = dict(os.environ, srctree=directory)
    result = subprocess.run([binary, "--quiet", "--write-config", "-", name],
                            cwd=directory, env=env, capture_output=True, text=True)
    if result.returncode == 0:
        return result.stdout, None
    stderr = result.stderr.strip()
    return None, stderr.splitlines()[-1] if stderr else "exit %d" % result.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("fixtures", nargs="?", default=os.path.join(CRATE, "..", "tests"))
    parser.add_argument("--kconf", default=os.path.join(CRATE, "target", "release", "kconf"))
    parser.add_argument("--kconfiglib", default=os.path.join(CRATE, ".."),
                        help="directory containing kconfiglib.py")
    args = parser.parse_args()

    fixtures = os.path.abspath(args.fixtures)
    sys.path.insert(0, os.path.abspath(args.kconfiglib))

    identical = differ = both_rejected = disagreed = 0
    details = []

    for name in sorted(os.listdir(fixtures)):
        if not name.startswith("K") or not os.path.isfile(os.path.join(fixtures, name)):
            continue

        py_config, py_error = load_with_kconfiglib(fixtures, name)
        rs_config, rs_error = load_with_kconf(args.kconf, fixtures, name)

        if py_error and rs_error:
            both_rejected += 1
        elif py_error or rs_error:
            disagreed += 1
            details.append((name, "kconfiglib: %s" % py_error if py_error
                            else "kconf: %s" % rs_error))
        elif py_config == rs_config:
            identical += 1
        else:
            differ += 1
            diff = difflib.unified_diff(py_config.splitlines(), rs_config.splitlines(),
                                        "kconfiglib", "kconf", lineterm="", n=0)
            details.append((name, "\n".join(list(diff)[:20])))

    print("identical: %d   differ: %d   both rejected: %d   disagreed: %d"
          % (identical, differ, both_rejected, disagreed))
    for name, detail in details:
        print("\n--- %s ---\n%s" % (name, detail))
    return 1 if details else 0


if __name__ == "__main__":
    sys.exit(main())
