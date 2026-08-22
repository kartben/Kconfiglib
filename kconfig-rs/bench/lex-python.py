#!/usr/bin/env python3
"""The same lexing benchmark as kconfig-rs/src/bin/lexbench.rs and bench/lex-go.

Reads every Kconfig file in a tree and tokenizes each line: keyword lookup,
identifier and string scanning, interning identifiers into a symbol table. No
`source` resolution, no macro expansion, no tree building -- this is the floor
for what any Python implementation of the inner loop could cost.
"""
import os, re, sys, time

KEYWORDS = {
    "---help---": 1, "allnoconfig_y": 2, "bool": 3, "boolean": 3, "choice": 4,
    "comment": 5, "config": 6, "configdefault": 7, "def_bool": 8, "def_hex": 9,
    "def_int": 10, "def_string": 11, "def_tristate": 12, "default": 13,
    "defconfig_list": 14, "depends": 15, "endchoice": 16, "endif": 17,
    "endmenu": 18, "env": 19, "grsource": 20, "gsource": 21, "help": 22,
    "hex": 23, "if": 24, "imply": 25, "int": 26, "mainmenu": 27, "menu": 28,
    "menuconfig": 29, "modules": 30, "on": 31, "option": 32, "optional": 33,
    "orsource": 20, "osource": 21, "prompt": 34, "range": 35, "rsource": 36,
    "select": 37, "source": 38, "string": 39, "tristate": 40, "visible": 41,
}
STRING_LEX = frozenset((3, 4, 5, 23, 26, 27, 28, 20, 21, 34, 36, 38, 39, 40))

# The same two regexes kconfiglib uses.
command_match = re.compile(r"\s*([A-Za-z0-9_$-]+)\s*").match
id_match = re.compile(r"([A-Za-z0-9_$/.-]+)\s*").match

get_keyword = KEYWORDS.get


def tokenize(s, syms, counts):
    counts[0] += 1
    match = command_match(s)
    if not match:
        return
    kw = get_keyword(match.group(1))
    if not kw:
        return
    counts[1] += 1
    prev = kw
    i = match.end()

    while i < len(s):
        match = id_match(s, i)
        if match:
            word = match.group(1)
            k = get_keyword(word)
            if k:
                prev = k
            elif prev in STRING_LEX:
                prev = 0
            else:
                if word not in syms:
                    syms[word] = len(syms)
                prev = 0
            counts[1] += 1
            i = match.end()
            continue

        c = s[i]
        if c == '"' or c == "'":
            end = s.find(c, i + 1)
            if end == -1:
                return
            if prev not in STRING_LEX:
                val = s[i + 1:end]
                if val not in syms:
                    syms[val] = len(syms)
            prev = 0
            counts[1] += 1
            i = end + 1
        elif s.startswith(("&&", "||", "!=", "<=", ">="), i):
            prev = 0; counts[1] += 1; i += 2
        elif c in "=!()<>":
            prev = 0; counts[1] += 1; i += 1
        elif c == "#":
            return
        else:
            return
        while i < len(s) and s[i].isspace():
            i += 1


def kconfig_files(root):
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        if ".git" in dirnames:
            dirnames.remove(".git")
        found += [os.path.join(dirpath, f) for f in filenames if f.startswith("Kconfig")]
    return sorted(found)


def main():
    files = kconfig_files(sys.argv[1])
    iterations = int(sys.argv[2]) if len(sys.argv) > 2 else 3
    best = None
    for _ in range(iterations):
        syms, counts, total = {}, [0, 0], 0
        start = time.perf_counter()
        for path in files:
            with open(path, encoding="utf-8", errors="replace") as f:
                text = f.read()
            total += len(text)
            for line in text.splitlines(True):
                tokenize(line, syms, counts)
        elapsed = time.perf_counter() - start
        if best is None or elapsed < best:
            best = elapsed
    mib = total / 2**20
    print("python  %d files  %.2f MiB  %d lines  %d tokens  %d unique names  best %.3fs (%.0f MiB/s)"
          % (len(files), mib, counts[0], counts[1], len(syms), best, mib / best))


main()
