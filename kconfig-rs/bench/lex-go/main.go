// A like-for-like lexing benchmark, used to compare Go against the Rust
// implementation on the phase that dominates a Kconfig load.
//
// It walks a source tree, reads every Kconfig file, and tokenizes each line
// with the same rules the real loader uses — keyword lookup, identifier and
// string scanning, and interning identifiers into a symbol table. It does not
// resolve `source` directives, expand macros, or build a tree: the point is to
// measure the shared inner loop, not to be a Kconfig implementation.
//
//	go run . <tree> [iterations]
package main

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

var keywords = map[string]int{
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

// Keywords after which a bare word is a string rather than a symbol.
var stringLex = map[int]bool{
	3: true, 4: true, 5: true, 23: true, 26: true, 27: true, 28: true,
	20: true, 21: true, 34: true, 36: true, 38: true, 39: true, 40: true,
}

type interner struct {
	ids     map[string]int32
	strings []string
}

func newInterner() *interner {
	return &interner{ids: make(map[string]int32, 1<<16)}
}

func (in *interner) intern(s string) int32 {
	if id, ok := in.ids[s]; ok {
		return id
	}
	id := int32(len(in.strings))
	in.strings = append(in.strings, s)
	in.ids[s] = id
	return id
}

func isIdentByte(c byte) bool {
	return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
		c == '_' || c == '$' || c == '/' || c == '.' || c == '-'
}

func isCommandByte(c byte) bool {
	return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
		c == '_' || c == '$' || c == '-'
}

func isSpace(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == 0x0b || c == 0x0c
}

func skipSpaces(s string, i int) int {
	for i < len(s) && isSpace(s[i]) {
		i++
	}
	return i
}

type counts struct {
	lines, tokens, symbols int
}

func tokenize(line string, syms *interner, c *counts) {
	c.lines++
	start := skipSpaces(line, 0)
	end := start
	for end < len(line) && isCommandByte(line[end]) {
		end++
	}
	if end == start {
		return
	}
	kw, ok := keywords[line[start:end]]
	if !ok {
		return
	}
	c.tokens++
	prev := kw
	i := skipSpaces(line, end)

	for i < len(line) {
		ch := line[i]
		if isIdentByte(ch) {
			end := i
			for end < len(line) && isIdentByte(line[end]) {
				end++
			}
			word := line[i:end]
			if k, ok := keywords[word]; ok {
				prev = k
			} else if stringLex[prev] {
				prev = 0
			} else {
				syms.intern(word)
				c.symbols++
				prev = 0
			}
			c.tokens++
			i = skipSpaces(line, end)
			continue
		}

		if ch == '"' || ch == '\'' {
			rel := strings.IndexByte(line[i+1:], ch)
			if rel < 0 {
				return
			}
			stop := i + 1 + rel + 1
			if !stringLex[prev] {
				syms.intern(line[i+1 : stop-1])
			}
			prev = 0
			c.tokens++
			i = skipSpaces(line, stop)
			continue
		}

		width := 1
		switch {
		case ch == '&' && i+1 < len(line) && line[i+1] == '&':
			width = 2
		case ch == '|' && i+1 < len(line) && line[i+1] == '|':
			width = 2
		case ch == '!' && i+1 < len(line) && line[i+1] == '=':
			width = 2
		case ch == '<' && i+1 < len(line) && line[i+1] == '=':
			width = 2
		case ch == '>' && i+1 < len(line) && line[i+1] == '=':
			width = 2
		case ch == '#':
			return
		case ch == '=' || ch == '!' || ch == '(' || ch == ')' || ch == '<' || ch == '>':
		default:
			return
		}
		prev = 0
		c.tokens++
		i = skipSpaces(line, i+width)
	}
}

func kconfigFiles(root string) []string {
	var files []string
	_ = filepath.Walk(root, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return nil
		}
		if info.IsDir() {
			if info.Name() == ".git" {
				return filepath.SkipDir
			}
			return nil
		}
		if strings.HasPrefix(info.Name(), "Kconfig") {
			files = append(files, path)
		}
		return nil
	})
	sort.Strings(files)
	return files
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: lexbench <tree> [iterations]")
		os.Exit(2)
	}
	iterations := 5
	if len(os.Args) > 2 {
		fmt.Sscanf(os.Args[2], "%d", &iterations)
	}

	files := kconfigFiles(os.Args[1])
	best := time.Duration(1<<62 - 1)
	var c counts
	var bytes int

	for n := 0; n < iterations; n++ {
		syms := newInterner()
		c = counts{}
		bytes = 0
		start := time.Now()
		for _, path := range files {
			data, err := os.ReadFile(path)
			if err != nil {
				continue
			}
			bytes += len(data)
			text := string(data)
			for len(text) > 0 {
				nl := strings.IndexByte(text, '\n')
				var line string
				if nl < 0 {
					line, text = text, ""
				} else {
					line, text = text[:nl+1], text[nl+1:]
				}
				tokenize(line, syms, &c)
			}
		}
		if elapsed := time.Since(start); elapsed < best {
			best = elapsed
		}
		c.symbols = len(syms.strings)
	}

	fmt.Printf("go    %d files  %.2f MiB  %d lines  %d tokens  %d unique names  best %.3fs (%.0f MiB/s)\n",
		len(files), float64(bytes)/(1<<20), c.lines, c.tokens, c.symbols,
		best.Seconds(), float64(bytes)/(1<<20)/best.Seconds())
}
