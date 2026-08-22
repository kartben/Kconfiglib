//! The Kconfig preprocessor: `$(...)` macro expansion and variable assignment.
//!
//! This is a faithful port of Kconfiglib's expander, which in turn follows
//! `Documentation/kbuild/kconfig-macro-language.rst`. Expansion rewrites the
//! line being tokenized in place, so every entry point takes the whole line
//! plus an index and hands back the rewritten line and a new index.

use std::borrow::Cow;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::shell::ShellCache;

/// A preprocessor variable. Recursive (`=`) variables expand at use; simple
/// (`:=`) ones expanded once at assignment.
pub struct Variable {
    pub value: String,
    pub is_recursive: bool,
    /// Guards against a variable that expands to itself.
    expansions: u32,
}

pub struct Preprocessor {
    pub variables: FxHashMap<String, Variable>,
    pub shell: ShellCache,
    pub env_vars_used: FxHashSet<String>,
    pub warnings: Vec<String>,
    /// Whether to provide Zephyr's `kconfigfunctions` helpers.
    pub zephyr_functions: bool,
    /// Current location, for `$(filename)`, `$(lineno)` and diagnostics.
    pub cur_file: String,
    /// Identity of `cur_file`, so the parser can skip refreshing it per line.
    pub cur_file_id: Option<crate::intern::StrId>,
    pub cur_line: u32,
}

/// Raised by `$(error-if,y,...)`.
pub struct PreprocessError(pub String);

pub type Result<T> = std::result::Result<T, PreprocessError>;

impl Preprocessor {
    pub fn new(shell: ShellCache) -> Preprocessor {
        Preprocessor {
            variables: FxHashMap::default(),
            shell,
            env_vars_used: FxHashSet::default(),
            warnings: Vec::new(),
            zephyr_functions: std::env::var("KCONFIG_FUNCTIONS").as_deref() != Ok("")
                && std::env::var_os("ZEPHYR_BASE").is_some(),
            cur_file: String::new(),
            cur_file_id: None,
            cur_line: 0,
        }
    }

    /// Expands every macro in `s`.
    pub fn expand_whole(&mut self, s: &str, args: &[String]) -> Result<String> {
        if !s.contains("$(") {
            return Ok(s.to_string());
        }
        let mut s = s.to_string();
        let mut i = 0;
        while let Some(rel) = s[i..].find("$(") {
            let (expanded, next) = self.expand_macro(&s, i + rel, args)?;
            s = expanded;
            i = next;
        }
        Ok(s)
    }

    /// Expands the macro that starts at byte offset `i` in `s`.
    ///
    /// Returns the rewritten string and the offset just past the substituted
    /// text, so the caller can continue scanning without rescanning the result.
    pub fn expand_macro(&mut self, s: &str, i: usize, args: &[String]) -> Result<(String, usize)> {
        // The line is only copied if a *nested* macro rewrites it; a plain
        // `$(name,args)` is expanded straight into `res`.
        let mut s = Cow::Borrowed(s);
        let mut res = s[..i].to_string();
        let mut i = i + 2; // skip "$("

        let mut arg_start = i;
        let mut new_args: Vec<String> = Vec::new();
        let mut nesting = 0usize;

        loop {
            let Some((pos, tok)) = find_macro_special(&s, i) else {
                return Err(PreprocessError(format!(
                    "{}:{}: missing end parenthesis in macro expansion",
                    self.cur_file, self.cur_line
                )));
            };

            match tok {
                MacroSpecial::Open => {
                    nesting += 1;
                    i = pos + 1;
                }
                MacroSpecial::Close => {
                    if nesting > 0 {
                        nesting -= 1;
                        i = pos + 1;
                        continue;
                    }
                    new_args.push(s[arg_start..pos].to_string());

                    // `$(1)` and friends substitute the enclosing call's arguments.
                    let substituted = new_args[0]
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| args.get(n).cloned());
                    match substituted {
                        Some(v) => res.push_str(&v),
                        None => {
                            let v = self.function_value(&new_args)?;
                            res.push_str(&v);
                        }
                    }

                    let end = res.len();
                    res.push_str(&s[pos + 1..]);
                    return Ok((res, end));
                }
                MacroSpecial::Comma => {
                    i = pos + 1;
                    if nesting > 0 {
                        continue;
                    }
                    new_args.push(s[arg_start..pos].to_string());
                    arg_start = i;
                }
                MacroSpecial::Nested => {
                    let (expanded, next) = self.expand_macro(&s, pos, args)?;
                    s = Cow::Owned(expanded);
                    i = next;
                }
            }
        }
    }

    /// Evaluates `$(name,arg...)`. Variables shadow functions, which shadow
    /// environment variables; anything else expands to the empty string.
    fn function_value(&mut self, args: &[String]) -> Result<String> {
        let name = args[0].as_str();

        if self.variables.contains_key(name) {
            let var = &self.variables[name];
            if args.len() == 1 && var.expansions > 0 {
                return Err(PreprocessError(format!(
                    "{}:{}: preprocessor variable {} recursively references itself",
                    self.cur_file, self.cur_line, name
                )));
            }
            if var.expansions > 100 {
                return Err(PreprocessError(format!(
                    "{}:{}: preprocessor function {} seems stuck in infinite recursion",
                    self.cur_file, self.cur_line, name
                )));
            }
            let body = var.value.clone();
            self.variables
                .get_mut(name)
                .expect("checked above")
                .expansions += 1;
            let out = self.expand_whole(&body, args);
            self.variables
                .get_mut(name)
                .expect("checked above")
                .expansions -= 1;
            return out;
        }

        if let Some(v) = self.builtin(name, args)? {
            return Ok(v);
        }

        if let Ok(v) = std::env::var(name) {
            self.env_vars_used.insert(name.to_string());
            return Ok(v);
        }

        Ok(String::new())
    }

    fn builtin(&mut self, name: &str, args: &[String]) -> Result<Option<String>> {
        let arg = |n: usize| -> &str { args.get(n).map(String::as_str).unwrap_or("") };

        let value = match name {
            "info" => {
                self.warnings.push(arg(1).to_string());
                String::new()
            }
            "warning-if" => {
                if arg(1) == "y" {
                    self.warnings
                        .push(format!("{}:{}: {}", self.cur_file, self.cur_line, arg(2)));
                }
                String::new()
            }
            "error-if" => {
                if arg(1) == "y" {
                    return Err(PreprocessError(format!(
                        "{}:{}: {}",
                        self.cur_file,
                        self.cur_line,
                        arg(2)
                    )));
                }
                String::new()
            }
            "filename" => self.cur_file.clone(),
            "lineno" => self.cur_line.to_string(),
            "shell" => {
                let (out, stderr) = self.shell.run(arg(1));
                if let Some(err) = stderr {
                    self.warnings.push(format!(
                        "'{}' wrote to stderr: {}",
                        arg(1),
                        err.lines().collect::<Vec<_>>().join("\n")
                    ));
                }
                out
            }
            _ if self.zephyr_functions => match crate::zephyr::call(name, args) {
                Some(v) => v,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    /// Handles a line that is a preprocessor assignment (`FOO = bar`,
    /// `FOO := bar`, `FOO += bar`) or a bare macro call evaluated for its side
    /// effects.
    pub fn parse_assignment(&mut self, line: &str) -> Result<()> {
        let mut s = line.trim_start().to_string();
        let mut i = 0;

        // The left-hand side may itself contain macros.
        loop {
            i += lhs_fragment_len(&s[i..]);
            if s[i..].starts_with("$(") {
                let (expanded, next) = self.expand_macro(&s, i, &[])?;
                s = expanded;
                i = next;
            } else {
                break;
            }
        }

        if s.trim().is_empty() {
            // A bare macro that expanded to nothing, e.g. `$(error-if,...)`.
            return Ok(());
        }

        let name = s[..i].to_string();
        let Some((op, value)) = split_assignment(&s[i..]) else {
            return Err(PreprocessError(format!(
                "{}:{}: couldn't parse '{}': syntax error",
                self.cur_file,
                self.cur_line,
                line.trim()
            )));
        };

        let known = self.variables.contains_key(&name);
        // `+=` on an undefined variable behaves like `=`.
        let op = if !known && op == "+=" { "=" } else { op };

        match op {
            "=" => {
                let entry = self.variables.entry(name).or_insert(Variable {
                    value: String::new(),
                    is_recursive: true,
                    expansions: 0,
                });
                entry.is_recursive = true;
                entry.value = value.to_string();
            }
            ":=" => {
                let expanded = self.expand_whole(value, &[])?;
                let entry = self.variables.entry(name).or_insert(Variable {
                    value: String::new(),
                    is_recursive: false,
                    expansions: 0,
                });
                entry.is_recursive = false;
                entry.value = expanded;
            }
            _ => {
                // `+=` expands immediately if the variable was last set with `:=`.
                let recursive = self.variables[&name].is_recursive;
                let addition = if recursive {
                    value.to_string()
                } else {
                    self.expand_whole(value, &[])?
                };
                let entry = self.variables.get_mut(&name).expect("checked above");
                entry.value.push(' ');
                entry.value.push_str(&addition);
            }
        }
        Ok(())
    }
}

enum MacroSpecial {
    Open,
    Close,
    Comma,
    Nested,
}

/// Finds the next `(`, `)`, `,` or `$(` at or after `from`.
fn find_macro_special(s: &str, from: usize) -> Option<(usize, MacroSpecial)> {
    let bytes = s.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'$' if bytes.get(i + 1) == Some(&b'(') => return Some((i, MacroSpecial::Nested)),
            b'(' => return Some((i, MacroSpecial::Open)),
            b')' => return Some((i, MacroSpecial::Close)),
            b',' => return Some((i, MacroSpecial::Comma)),
            _ => i += 1,
        }
    }
    None
}

/// Length of the leading `[A-Za-z0-9_-]*` run.
fn lhs_fragment_len(s: &str) -> usize {
    s.bytes()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
        .count()
}

/// Splits `\s*(=|:=|\+=)\s*(.*)` off the front of `s`.
fn split_assignment(s: &str) -> Option<(&'static str, &str)> {
    let rest = s.trim_start();
    let (op, rest) = if let Some(r) = rest.strip_prefix(":=") {
        (":=", r)
    } else if let Some(r) = rest.strip_prefix("+=") {
        ("+=", r)
    } else if let Some(r) = rest.strip_prefix('=') {
        ("=", r)
    } else {
        return None;
    };
    Some((op, rest.trim_start().trim_end_matches('\n')))
}
