#!/usr/bin/env python3
"""Generate the Kconfig files a Zephyr tree needs before it can be loaded.

Zephyr's Kconfig sources files that CMake produces during a build: the module
list, the board and SoC indexes, a devicetree fragment. This mirrors what
doc/_extensions/zephyr/kconfig/__init__.py does for the documentation build,
which pulls in every board, SoC and arch — the heaviest realistic Zephyr load,
and the one benchmarked in docs/INVESTIGATION.md.

    bench/setup-zephyr-tree.py <zephyr-tree> [output-dir]

Writes the generated files to <output-dir> (default <zephyr-tree>/../zephyr-kconfig-bin)
and an environment script next to it. Then:

    source <output-dir>/env.sh
    cd <zephyr-tree> && kconf --stats Kconfig
"""
import argparse
import os
import re
import sys
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("zephyr", type=Path)
    parser.add_argument("output", type=Path, nargs="?")
    args = parser.parse_args()

    zephyr = args.zephyr.resolve()
    out = (args.output or zephyr.parent / "zephyr-kconfig-bin").resolve()

    sys.path.insert(0, str(zephyr / "scripts"))
    sys.path.insert(0, str(zephyr / "scripts/kconfig"))
    import list_boards
    import list_hardware
    import zephyr_module

    for sub in ("", "soc", "arch", "boards"):
        (out / sub).mkdir(parents=True, exist_ok=True)

    modules = zephyr_module.parse_modules(zephyr)
    (out / "kconfig_module_dirs.env").write_text("".join(
        zephyr_module.process_kconfig_module_dir(m.project, m.meta, False) for m in modules))
    (out / "Kconfig.modules").write_text("".join(
        zephyr_module.process_kconfig(m.project, m.meta) for m in modules))
    (out / "Kconfig.sysbuild.modules").write_text("".join(
        zephyr_module.process_sysbuildkconfig(m.project, m.meta) for m in modules))

    # No devicetree: every dt_* query falls back to its doc-mode constant.
    (out / "Kconfig.dts").write_text("")
    (out / "soc" / "Kconfig.defconfig").write_text("")

    systems = list_hardware.find_v2_systems(argparse.Namespace(soc_roots=[zephyr]))
    # Sorted, unlike Zephyr's own generator, which iterates a set — so the
    # order of the SoC sources, and with it the order of symbols in the
    # resulting .config, varies from run to run there.
    soc_folders = sorted({soc.folder[0] for soc in systems.get_socs()})
    (out / "soc" / "Kconfig.soc").write_text("".join(
        'source "%s"\n' % (Path(f) / "Kconfig.soc").as_posix() for f in soc_folders))
    (out / "soc" / "Kconfig").write_text("".join(
        'osource "%s"\n' % (Path(f) / "Kconfig").as_posix() for f in soc_folders))

    arches = list_hardware.find_v2_archs(argparse.Namespace(arch_roots=[zephyr], arch=None))
    (out / "arch" / "Kconfig").write_text("".join(
        'source "%s"\n' % (Path(a["path"]) / "Kconfig").as_posix() for a in arches["archs"]))

    boards = list_boards.find_v2_boards(argparse.Namespace(
        board_roots=[zephyr], soc_roots=[zephyr], board=None, board_dir=[])).values()
    with open(out / "boards" / "Kconfig.boards", "w") as f:
        for board in boards:
            for name in [board.name] + list(list_boards.board_v2_qualifiers(board)):
                symbol = "BOARD_" + re.sub(r"[^a-zA-Z0-9_]", "_", name).upper()
                f.write("config  %s\n\t bool\n" % symbol)
            f.write('source "%s"\n\n' % (board.dir / ("Kconfig." + board.name)).as_posix())

    env = {
        "ZEPHYR_BASE": str(zephyr),
        "srctree": str(zephyr),
        "KCONFIG_DOC_MODE": "1",
        "KCONFIG_BINARY_DIR": str(out),
        "ARCH_DIR": "arch",
        "ARCH": "[!v][!2]*",
        "HWM_SCHEME": "v2",
        "BOARD": "boards",
        "KCONFIG_BOARD_DIR": str(out / "boards"),
        "CONFIG_": "CONFIG_",
    }
    for line in (out / "kconfig_module_dirs.env").read_text().splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            env[key.strip()] = value.strip().strip('"')
    for module in modules:
        build = module.meta.get("build") or {}
        if build.get("kconfig"):
            name = module.meta["name-sanitized"].upper()
            env["ZEPHYR_%s_KCONFIG" % name] = str(Path(module.project) / build["kconfig"])

    with open(out / "env.sh", "w") as f:
        for key, value in sorted(env.items()):
            f.write("export %s='%s'\n" % (key, value))

    print("wrote %s" % out)
    print("boards: %d  socs: %d  arches: %d  modules: %d"
          % (len(boards), len(soc_folders), len(arches["archs"]), len(modules)))
    print("now: source %s" % (out / "env.sh"))


if __name__ == "__main__":
    main()
