//! The line tokenizer.
//!
//! Kconfig is line-oriented, and whether a bare word is a symbol reference or
//! a string depends on the keyword that introduced the line — `menu foo` takes
//! a title, `depends on foo` takes a symbol. The tokenizer therefore keeps one
//! token of lookback, exactly as the C tools and Kconfiglib do.
//!
//! Macro expansion happens here too, because `$(...)` can appear inside symbol
//! names and strings and changes what the rest of the line looks like.

use std::borrow::Cow;

use crate::model::SymbolId;
use crate::preprocess::Preprocessor;
use crate::{Error, Kconfig, Result};

/// A Kconfig keyword.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kw {
    AllnoconfigY,
    Bool,
    Choice,
    Comment,
    Config,
    /// Zephyr extension: a `config`-like block that only adds `default`s.
    ConfigDefault,
    DefBool,
    DefHex,
    DefInt,
    DefString,
    DefTristate,
    Default,
    DefconfigList,
    Depends,
    EndChoice,
    EndIf,
    EndMenu,
    Env,
    Help,
    Hex,
    If,
    Imply,
    Int,
    MainMenu,
    Menu,
    MenuConfig,
    Modules,
    On,
    Option,
    Optional,
    OrSource,
    OSource,
    Prompt,
    Range,
    RSource,
    Select,
    Source,
    Str,
    Tristate,
    Visible,
}

impl Kw {
    /// True for keywords after which an unquoted word is a string rather than
    /// a symbol reference. `Choice` is included so that `choice FOO` does not
    /// register a symbol named `FOO`.
    fn expects_string(self) -> bool {
        matches!(
            self,
            Kw::Bool
                | Kw::Choice
                | Kw::Comment
                | Kw::Hex
                | Kw::Int
                | Kw::MainMenu
                | Kw::Menu
                | Kw::OrSource
                | Kw::OSource
                | Kw::Prompt
                | Kw::RSource
                | Kw::Source
                | Kw::Str
                | Kw::Tristate
        )
    }

    pub fn is_type(self) -> bool {
        matches!(self, Kw::Bool | Kw::Tristate | Kw::Int | Kw::Hex | Kw::Str)
    }

    pub fn is_source(self) -> bool {
        matches!(self, Kw::Source | Kw::RSource | Kw::OSource | Kw::OrSource)
    }

    /// `rsource`/`orsource` resolve relative to the including file.
    pub fn is_relative_source(self) -> bool {
        matches!(self, Kw::RSource | Kw::OrSource)
    }

    /// `source`/`rsource` fail when the pattern matches nothing; the `o`
    /// variants do not.
    pub fn is_obligatory_source(self) -> bool {
        matches!(self, Kw::Source | Kw::RSource)
    }
}

fn keyword(word: &str) -> Option<Kw> {
    Some(match word {
        "---help---" => Kw::Help,
        "allnoconfig_y" => Kw::AllnoconfigY,
        "bool" | "boolean" => Kw::Bool,
        "choice" => Kw::Choice,
        "comment" => Kw::Comment,
        "config" => Kw::Config,
        "configdefault" => Kw::ConfigDefault,
        "def_bool" => Kw::DefBool,
        "def_hex" => Kw::DefHex,
        "def_int" => Kw::DefInt,
        "def_string" => Kw::DefString,
        "def_tristate" => Kw::DefTristate,
        "default" => Kw::Default,
        "defconfig_list" => Kw::DefconfigList,
        "depends" => Kw::Depends,
        "endchoice" => Kw::EndChoice,
        "endif" => Kw::EndIf,
        "endmenu" => Kw::EndMenu,
        "env" => Kw::Env,
        // Backwards compatibility with pre-4.18 spellings.
        "grsource" => Kw::OrSource,
        "gsource" => Kw::OSource,
        "help" => Kw::Help,
        "hex" => Kw::Hex,
        "if" => Kw::If,
        "imply" => Kw::Imply,
        "int" => Kw::Int,
        "mainmenu" => Kw::MainMenu,
        "menu" => Kw::Menu,
        "menuconfig" => Kw::MenuConfig,
        "modules" => Kw::Modules,
        "on" => Kw::On,
        "option" => Kw::Option,
        "optional" => Kw::Optional,
        "orsource" => Kw::OrSource,
        "osource" => Kw::OSource,
        "prompt" => Kw::Prompt,
        "range" => Kw::Range,
        "rsource" => Kw::RSource,
        "select" => Kw::Select,
        "source" => Kw::Source,
        "string" => Kw::Str,
        "tristate" => Kw::Tristate,
        "visible" => Kw::Visible,
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    And,
    Or,
    Not,
    LParen,
    RParen,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Token {
    Keyword(Kw),
    Op(Op),
    /// A symbol reference. Quoted strings in symbol position become constant
    /// symbols and also land here.
    Sym(SymbolId),
    /// A string literal, or an unquoted word where a string was expected.
    Text(Box<str>),
}

impl Token {
    pub fn keyword(&self) -> Option<Kw> {
        match self {
            Token::Keyword(k) => Some(*k),
            _ => None,
        }
    }

    pub fn symbol(&self) -> Option<SymbolId> {
        match self {
            Token::Sym(s) => Some(*s),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Token::Text(s) => Some(s),
            _ => None,
        }
    }
}

#[inline]
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'/' | b'.' | b'-')
}

/// The character class of the first word on a line (no `/` or `.`).
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

/// Tokenizes one logical line into `tokens`, which is cleared first.
///
/// Returns false for lines the parser can ignore: blank lines, comments, and
/// preprocessor assignments (which are applied here as a side effect).
pub fn tokenize(
    kconf: &mut Kconfig,
    pp: &mut Preprocessor,
    line: &str,
    tokens: &mut Vec<Token>,
) -> Result<bool> {
    tokens.clear();

    let bytes = line.as_bytes();
    let start = skip_spaces(bytes, 0);
    let word_end = {
        let mut i = start;
        while i < bytes.len() && is_command_byte(bytes[i]) {
            i += 1;
        }
        i
    };

    if word_end == start {
        // No leading word: blank line or comment, otherwise a syntax error.
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return Ok(false);
        }
        return Err(parse_error(pp, line, "unknown token at start of line"));
    }

    let Some(kw) = keyword(&line[start..word_end]) else {
        // Old C tools accidentally accepted "--help--" and friends.
        if line.trim_matches([' ', '\t', '\n', '-']) == "help" {
            tokens.push(Token::Keyword(Kw::Help));
            return Ok(true);
        }
        // Anything else that starts with a non-keyword is a preprocessor
        // assignment, or a bare macro evaluated for its side effects.
        pp.parse_assignment(line).map_err(|e| Error(e.0))?;
        return Ok(false);
    };

    tokens.push(Token::Keyword(kw));
    // The line is only copied if a macro rewrites it, which is rare.
    let mut buf = Cow::Borrowed(line);
    // Quoted strings take a fast path unless the line has something to expand.
    // Kconfiglib tests this per string; testing once per line is the same
    // answer for far less scanning.
    let mut needs_expansion = has_dollar_or_escape(bytes);
    let mut i = skip_spaces(bytes, word_end);

    while i < buf.len() {
        let s: &str = &buf;
        let bytes = s.as_bytes();

        if is_ident_byte(bytes[i]) {
            let mut end = i;
            let mut word_has_macro = false;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                word_has_macro |= bytes[end] == b'$';
                end += 1;
            }
            let word = &s[i..end];

            if let Some(kw) = keyword(word) {
                tokens.push(Token::Keyword(kw));
                i = skip_spaces(bytes, end);
                continue;
            }

            let after_string_keyword =
                matches!(tokens.last(), Some(Token::Keyword(k)) if k.expects_string());
            if after_string_keyword {
                // Missing quotes, e.g. `menu unquoted title` or `choice FOO`.
                tokens.push(Token::Text(word.into()));
                i = skip_spaces(bytes, end);
                continue;
            }

            // A symbol reference. `n`, `m` and `y` map to the constants.
            if word_has_macro {
                let (expanded, name, next) = expand_name(pp, s, i)?;
                buf = Cow::Owned(expanded);
                needs_expansion = has_dollar_or_escape(buf.as_bytes());
                i = next;
                tokens.push(Token::Sym(named_symbol(kconf, &name)));
            } else {
                let sym = named_symbol(kconf, word);
                i = skip_spaces(bytes, end);
                tokens.push(Token::Sym(sym));
            }
            continue;
        }

        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            let value: Cow<str>;
            let end;
            if !needs_expansion {
                // Fast path: the string cannot contain escapes or macros.
                let Some(rel) = s[i + 1..].find(c as char) else {
                    return Err(parse_error(pp, s, "unterminated string"));
                };
                end = i + 1 + rel + 1;
                value = Cow::Borrowed(&s[i + 1..end - 1]);
            } else {
                let (expanded, stop) = expand_string(pp, s, i)?;
                buf = Cow::Owned(expanded);
                needs_expansion = has_dollar_or_escape(buf.as_bytes());
                end = stop;
                // Legacy `$FOO` references are resolved from the environment
                // after macro expansion, for compatibility with old kernels.
                let raw = &buf[i + 1..end - 1];
                let raw = if raw.contains("$UNAME_RELEASE") {
                    Cow::Owned(raw.replace("$UNAME_RELEASE", uname_release()))
                } else {
                    Cow::Borrowed(raw)
                };
                value = Cow::Owned(expand_env_vars(&raw));
            }

            // `option env="FOO"` names an environment variable, not a symbol.
            let as_string = matches!(tokens.last(), Some(Token::Keyword(k)) if k.expects_string())
                || tokens.first() == Some(&Token::Keyword(Kw::Option));
            if as_string {
                tokens.push(Token::Text(value.as_ref().into()));
            } else {
                let sym = kconf.lookup_const_sym(value.as_ref());
                tokens.push(Token::Sym(sym));
            }
            i = skip_spaces(buf.as_bytes(), end);
            continue;
        }

        let (op, width) = match c {
            b'&' if bytes.get(i + 1) == Some(&b'&') => (Op::And, 2),
            b'|' if bytes.get(i + 1) == Some(&b'|') => (Op::Or, 2),
            b'=' => (Op::Eq, 1),
            b'!' if bytes.get(i + 1) == Some(&b'=') => (Op::Ne, 2),
            b'!' => (Op::Not, 1),
            b'(' => (Op::LParen, 1),
            b')' => (Op::RParen, 1),
            b'#' => break,
            b'<' if bytes.get(i + 1) == Some(&b'=') => (Op::Le, 2),
            b'<' => (Op::Lt, 1),
            b'>' if bytes.get(i + 1) == Some(&b'=') => (Op::Ge, 2),
            b'>' => (Op::Gt, 1),
            _ => return Err(parse_error(pp, s, "unknown tokens in line")),
        };
        tokens.push(Token::Op(op));
        i = skip_spaces(bytes, i + width);
    }

    Ok(true)
}

/// True if the line contains anything the preprocessor or the escape handling
/// would have to rewrite.
#[inline]
fn has_dollar_or_escape(bytes: &[u8]) -> bool {
    memchr::memchr2(b'$', b'\\', bytes).is_some()
}

/// Resolves a name in symbol position, mapping `n`/`m`/`y` to the constants.
fn named_symbol(kconf: &mut Kconfig, name: &str) -> SymbolId {
    match name {
        "n" => kconf.n,
        "m" => kconf.m,
        "y" => kconf.y,
        _ => kconf.lookup_sym(name),
    }
}

/// Expands a symbol name that contains `$(...)`, starting at `i`.
/// Returns the rewritten line, the name, and the offset of the next token.
fn expand_name(pp: &mut Preprocessor, s: &str, i: usize) -> Result<(String, String, usize)> {
    let mut s = s.to_string();
    let end;
    loop {
        // Scan to the first character that cannot be part of a name.
        let bytes = s.as_bytes();
        let mut j = i;
        while j < bytes.len() && is_ident_byte(bytes[j]) {
            if bytes[j] == b'$' && bytes.get(j + 1) == Some(&b'(') {
                break;
            }
            j += 1;
        }
        if s.as_bytes().get(j) == Some(&b'$') && s.as_bytes().get(j + 1) == Some(&b'(') {
            let (expanded, next) = pp.expand_macro(&s, j, &[]).map_err(|e| Error(e.0))?;
            s = expanded;
            let _ = next;
            continue;
        }
        end = j;
        break;
    }

    let name = s[i..end].trim().to_string();
    if name.is_empty() {
        return Err(parse_error(pp, &s, "macro expanded to blank string"));
    }
    let next = skip_spaces(s.as_bytes(), end);
    Ok((s, name, next))
}

/// Expands a quoted string starting at `i`, resolving `\x` escapes and macros.
/// Returns the rewritten line and the offset just past the closing quote.
fn expand_string(pp: &mut Preprocessor, s: &str, i: usize) -> Result<(String, usize)> {
    let quote = s.as_bytes()[i];
    let mut s = s.to_string();
    let mut j = i + 1;
    loop {
        let bytes = s.as_bytes();
        let mut k = j;
        while k < bytes.len() {
            match bytes[k] {
                b'"' | b'\'' | b'\\' => break,
                b'$' if bytes.get(k + 1) == Some(&b'(') => break,
                _ => k += 1,
            }
        }
        if k >= s.len() {
            return Err(parse_error(pp, &s, "unterminated string"));
        }
        match s.as_bytes()[k] {
            c if c == quote => return Ok((s, k + 1)),
            b'\\' => {
                // Drop the backslash, keeping the escaped character. This also
                // lets `\$(foo)` suppress macro expansion.
                s.replace_range(k..k + 1, "");
                j = k + 1;
            }
            b'$' => {
                let (expanded, next) = pp.expand_macro(&s, k, &[]).map_err(|e| Error(e.0))?;
                s = expanded;
                j = next;
            }
            // A `'` inside `"` or vice versa.
            _ => j = k + 1,
        }
    }
}

/// Python's `os.path.expandvars`: substitutes `$NAME` and `${NAME}` from the
/// environment, leaving unknown names untouched.
fn expand_env_vars(s: &str) -> String {
    if !s.contains('$') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        let (name, end) = if bytes.get(i + 1) == Some(&b'{') {
            match s[i + 2..].find('}') {
                Some(rel) => (&s[i + 2..i + 2 + rel], i + 2 + rel + 1),
                None => {
                    out.push('$');
                    i += 1;
                    continue;
                }
            }
        } else {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            (&s[i + 1..j], j)
        };
        match std::env::var(name) {
            Ok(value) if !name.is_empty() => out.push_str(&value),
            _ => out.push_str(&s[i..end]),
        }
        i = end;
    }
    out
}

/// The running kernel's release string, for the legacy `$UNAME_RELEASE`
/// reference. Read once — it cannot change while we run.
fn uname_release() -> &'static str {
    static RELEASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RELEASE.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    })
}

fn parse_error(pp: &Preprocessor, line: &str, msg: &str) -> Error {
    Error(format!(
        "{}:{}: error: couldn't parse '{}': {}",
        pp.cur_file,
        pp.cur_line,
        line.trim(),
        msg
    ))
}
