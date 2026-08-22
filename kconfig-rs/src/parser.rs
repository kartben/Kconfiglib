//! Recursive-descent parser for Kconfig files.
//!
//! The grammar is small — a file is a sequence of blocks, and each block is a
//! header line followed by property lines — so the parser mirrors it directly:
//! [`Parser::parse_block`] handles `config`/`menu`/`choice`/`if`/`source`, and
//! [`Parser::parse_props`] handles the property lines that follow a header.
//!
//! Files are read whole and walked by byte offset. `source` pushes a new frame
//! onto the file stack and recurses.

use std::path::{Path, PathBuf};

use crate::expr::{CmpOp, ExprId};
use crate::lexer::{tokenize, Kw, Op, Token};
use crate::model::{
    ChoiceId, Default as DefaultProp, Item, Loc, MenuNode, NodeId, Range, Selection, SymbolId, Type,
};
use crate::preprocess::Preprocessor;
use crate::{Choice, Error, Kconfig, Result};

/// One open Kconfig file.
struct OpenFile {
    /// Path as reported in diagnostics: relative to `$srctree` where possible.
    display_path: String,
    text: String,
    /// Byte offset of the next unread line.
    pos: usize,
    line: u32,
}

pub struct Parser<'a> {
    kconf: &'a mut Kconfig,
    pp: &'a mut Preprocessor,
    files: Vec<OpenFile>,
    /// The logical line currently being tokenized, with continuations joined.
    line: String,
    tokens: Vec<Token>,
    /// Index of the next token to consume, starting at 1 because callers have
    /// already looked at `tokens[0]`.
    at: usize,
    /// Set when a line turned out to belong to the enclosing construct and has
    /// to be handed back.
    reuse_tokens: bool,
    /// Scratch space for assembling help texts, reused across blocks.
    help_buf: String,
    /// File buffers returned by closed files, reused for the next `source`.
    /// Kconfig trees open thousands of small files; recycling the buffers
    /// keeps that out of the allocator.
    spare_buffers: Vec<String>,
    loc: Loc,
}

/// Parses `top_file` and everything it sources into `kconf`.
pub fn parse(kconf: &mut Kconfig, pp: &mut Preprocessor, top_file: &str) -> Result<()> {
    let mut parser = Parser {
        loc: Loc {
            file: kconf.interner.intern(top_file),
            line: 0,
        },
        kconf,
        pp,
        files: Vec::new(),
        line: String::new(),
        tokens: Vec::new(),
        at: 1,
        reuse_tokens: false,
        help_buf: String::new(),
        spare_buffers: Vec::new(),
    };
    parser.kconf.node_mut(NodeId(0)).loc = parser.loc;

    let path = Path::new(&parser.kconf.srctree).join(top_file);
    if !parser.push_file(&path, top_file.to_string())? {
        return Err(Error(format!("could not open '{}'", path.display())));
    }

    let top = parser.kconf.top_node;
    let last = parser.parse_block(None, top, top)?;
    parser.kconf.node_mut(last).next = None;
    let first_child = parser.kconf.node(top).next;
    parser.kconf.node_mut(top).list = first_child;
    parser.kconf.node_mut(top).next = None;
    Ok(())
}

impl<'a> Parser<'a> {
    // -- file handling ----------------------------------------------------

    /// Opens `path` and makes it the current file. Returns false if it does
    /// not exist, which is how an `osource` of a missing file is handled.
    fn push_file(&mut self, path: &Path, display_path: String) -> Result<bool> {
        let mut text = self.spare_buffers.pop().unwrap_or_default();
        text.clear();
        let started = std::time::Instant::now();
        let read = read_file_into(path, &mut text);
        self.kconf.timings.file_read += started.elapsed().as_secs_f64();
        match read {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.spare_buffers.push(text);
                return Ok(false);
            }
            Err(e) => {
                return Err(Error(format!(
                    "{}:{}: Could not open '{}' ({e})",
                    self.loc_file(),
                    self.loc.line,
                    path.display()
                )))
            }
        }
        for open in &self.files {
            if open.display_path == display_path {
                return Err(Error(format!(
                    "{}:{}: recursive 'source' of '{}' detected. Check that \
                     environment variables are set correctly.",
                    self.loc_file(),
                    self.loc.line,
                    display_path
                )));
            }
        }
        self.kconf.timings.bytes_read += text.len() as u64;
        self.kconf.kconfig_filenames.push(display_path.clone());
        self.loc = Loc {
            file: self.kconf.interner.intern(&display_path),
            line: 0,
        };
        self.files.push(OpenFile {
            display_path,
            text,
            pos: 0,
            line: 0,
        });
        Ok(true)
    }

    fn pop_file(&mut self) {
        if let Some(file) = self.files.pop() {
            self.spare_buffers.push(file.text);
        }
        if let Some(open) = self.files.last() {
            self.loc = Loc {
                file: self.kconf.interner.intern(&open.display_path),
                line: open.line,
            };
        }
    }

    fn loc_file(&self) -> &str {
        &self.kconf.interner[self.loc.file]
    }

    /// Reads one physical line into `out`, including its newline. Returns
    /// false at end of file.
    fn read_raw_line(&mut self, out: &mut String) -> bool {
        let Some(file) = self.files.last_mut() else {
            return false;
        };
        if file.pos >= file.text.len() {
            return false;
        }
        let rest = &file.text[file.pos..];
        let end = memchr::memchr(b'\n', rest.as_bytes()).map_or(rest.len(), |i| i + 1);
        out.clear();
        out.push_str(&rest[..end]);
        file.pos += end;
        file.line += 1;
        true
    }

    /// Fetches the next logical line, joining `\`-continuations, and tokenizes
    /// it. Returns false at end of file.
    fn next_line(&mut self) -> Result<bool> {
        if self.reuse_tokens {
            self.reuse_tokens = false;
            return Ok(true);
        }

        let mut line = std::mem::take(&mut self.line);
        if !self.read_raw_line(&mut line) {
            self.line = line;
            return Ok(false);
        }
        let mut cont = String::new();
        while line.ends_with("\\\n") {
            line.truncate(line.len() - 2);
            if !self.read_raw_line(&mut cont) {
                break;
            }
            line.push_str(&cont);
        }
        self.line = line;

        self.sync_location();
        self.tokenize_current()
    }

    fn sync_location(&mut self) {
        if let Some(file) = self.files.last() {
            self.loc.line = file.line;
            self.pp.cur_line = file.line;
            if self.pp.cur_file_id != Some(self.loc.file) {
                self.pp.cur_file_id = Some(self.loc.file);
                self.pp.cur_file.clear();
                self.pp.cur_file.push_str(&file.display_path);
            }
        }
    }

    fn tokenize_current(&mut self) -> Result<bool> {
        // Disjoint field borrows: the line and token buffers are separate from
        // the model, so all three can be handed to the tokenizer at once.
        let Parser {
            kconf,
            pp,
            line,
            tokens,
            ..
        } = self;
        tokenize(kconf, pp, line, tokens)?;
        self.at = 1;
        Ok(true)
    }

    // -- token helpers ----------------------------------------------------

    /// The keyword that opened this line. The tokenizer only emits tokens for
    /// lines that start with one, so this is `None` only for skipped lines.
    fn head(&self) -> Option<Kw> {
        self.tokens.first().and_then(Token::keyword)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn accept(&mut self, kw: Kw) -> bool {
        if self.peek().and_then(Token::keyword) == Some(kw) {
            self.at += 1;
            return true;
        }
        false
    }

    fn at_eol(&self) -> bool {
        self.at >= self.tokens.len()
    }

    fn expect_eol(&self) -> Result<()> {
        if self.at_eol() {
            Ok(())
        } else {
            Err(self.error("trailing tokens on line"))
        }
    }

    /// Consumes a symbol token, which may be a constant symbol.
    fn expect_sym(&mut self) -> Result<SymbolId> {
        let sym = self.peek().and_then(Token::symbol);
        self.at += 1;
        sym.ok_or_else(|| self.error("expected symbol"))
    }

    fn expect_nonconst_sym(&mut self) -> Result<SymbolId> {
        let sym = self.expect_sym()?;
        if self.kconf.sym(sym).is_const {
            return Err(self.error("expected nonconstant symbol"));
        }
        Ok(sym)
    }

    /// Consumes a string token that must be the last thing on the line.
    fn expect_str_and_eol(&mut self) -> Result<String> {
        let text = self.peek().and_then(Token::text).map(str::to_string);
        self.at += 1;
        let text = text.ok_or_else(|| self.error("expected string"))?;
        self.expect_eol()?;
        Ok(text)
    }

    fn error(&self, msg: &str) -> Error {
        Error(format!(
            "{}:{}: error: couldn't parse '{}': {}",
            self.loc_file(),
            self.loc.line,
            self.line.trim(),
            msg
        ))
    }

    fn warn(&mut self, msg: String) {
        self.kconf.warnings.push(msg);
    }

    // -- tree construction ------------------------------------------------

    fn new_node(&mut self, item: Item, parent: NodeId) -> NodeId {
        let y = self.kconf.exprs.y;
        let id = NodeId(self.kconf.nodes.len() as u32);
        self.kconf
            .nodes
            .push(MenuNode::new(item, Some(parent), self.loc, y));
        id
    }

    // -- block parsing ----------------------------------------------------

    /// Parses until `end` (or end of file when `end` is `None`), appending
    /// nodes after `prev`. Returns the last node in the block.
    fn parse_block(&mut self, end: Option<Kw>, parent: NodeId, prev: NodeId) -> Result<NodeId> {
        let mut prev = prev;

        while self.next_line()? {
            let Some(head) = self.head() else { continue };

            match head {
                kw @ (Kw::Config | Kw::MenuConfig | Kw::ConfigDefault) => {
                    prev = self.parse_config(kw, parent, prev)?;
                }

                kw if kw.is_source() => {
                    prev = self.parse_source(kw, parent, prev)?;
                }

                kw if Some(kw) == end => {
                    self.expect_eol()?;
                    self.kconf.node_mut(prev).next = None;
                    return Ok(prev);
                }

                Kw::If => {
                    let node = self.new_node(Item::If, parent);
                    let dep = self.parse_expr_and_eol(true)?;
                    self.kconf.node_mut(node).dep = dep;

                    self.parse_block(Some(Kw::EndIf), node, node)?;
                    let first_child = self.kconf.node(node).next;
                    self.kconf.node_mut(node).list = first_child;

                    self.kconf.node_mut(prev).next = Some(node);
                    prev = node;
                }

                Kw::Menu => {
                    let node = self.new_node(Item::Menu, parent);
                    let title = self.expect_str_and_eol()?;
                    let title = self.kconf.interner.intern(&title);
                    let y = self.kconf.exprs.y;
                    {
                        let n = self.kconf.node_mut(node);
                        n.is_menuconfig = true;
                        n.prompt = Some((title, y));
                        n.visibility = y;
                    }
                    self.kconf.menu_nodes.push(node);

                    self.parse_props(node)?;
                    self.parse_block(Some(Kw::EndMenu), node, node)?;
                    let first_child = self.kconf.node(node).next;
                    self.kconf.node_mut(node).list = first_child;

                    self.kconf.node_mut(prev).next = Some(node);
                    prev = node;
                }

                Kw::Comment => {
                    let node = self.new_node(Item::Comment, parent);
                    let title = self.expect_str_and_eol()?;
                    let title = self.kconf.interner.intern(&title);
                    let y = self.kconf.exprs.y;
                    self.kconf.node_mut(node).prompt = Some((title, y));
                    self.kconf.comment_nodes.push(node);

                    self.parse_props(node)?;

                    self.kconf.node_mut(prev).next = Some(node);
                    prev = node;
                }

                Kw::Choice => {
                    prev = self.parse_choice(parent, prev)?;
                }

                Kw::MainMenu => {
                    let title = self.expect_str_and_eol()?;
                    let title = self.kconf.interner.intern(&title);
                    let y = self.kconf.exprs.y;
                    let top = self.kconf.top_node;
                    self.kconf.node_mut(top).prompt = Some((title, y));
                }

                Kw::EndChoice => return Err(self.error("no corresponding 'choice'")),
                Kw::EndIf => return Err(self.error("no corresponding 'if'")),
                Kw::EndMenu => return Err(self.error("no corresponding 'menu'")),
                _ => return Err(self.error("unrecognized construct")),
            }
        }

        if let Some(end) = end {
            let what = match end {
                Kw::EndChoice => "endchoice",
                Kw::EndIf => "endif",
                _ => "endmenu",
            };
            return Err(Error(format!(
                "error: expected '{}' at end of '{}'",
                what,
                self.loc_file()
            )));
        }
        Ok(prev)
    }

    fn parse_config(&mut self, kw: Kw, parent: NodeId, prev: NodeId) -> Result<NodeId> {
        let sym = self
            .tokens
            .get(1)
            .and_then(Token::symbol)
            .filter(|s| !self.kconf.sym(*s).is_const)
            .ok_or_else(|| self.error("missing or bad symbol name"))?;
        if self.tokens.len() > 2 {
            return Err(self.error("trailing tokens on line"));
        }

        self.kconf.defined_syms.push(sym);
        let node = self.new_node(Item::Symbol(sym), parent);
        {
            let n = self.kconf.node_mut(node);
            n.is_menuconfig = kw == Kw::MenuConfig;
            n.is_configdefault = kw == Kw::ConfigDefault;
        }
        self.kconf.sym_mut(sym).nodes.push(node);

        self.parse_props(node)?;

        let n = self.kconf.node(node);
        if n.is_configdefault
            && (n.prompt.is_some()
                || n.dep != self.kconf.exprs.y
                || !n.ranges.is_empty()
                || !n.selects.is_empty()
                || !n.implies.is_empty())
        {
            return Err(self.error("configdefault can only contain `default`"));
        }
        if n.is_menuconfig && n.prompt.is_none() {
            let name = self.kconf.name(sym).to_string();
            self.warn(format!("the menuconfig symbol {name} has no prompt"));
        }

        self.kconf.node_mut(prev).next = Some(node);
        Ok(node)
    }

    fn parse_choice(&mut self, parent: NodeId, prev: NodeId) -> Result<NodeId> {
        let n_expr = self.kconf.exprs.n;
        let choice = if self.tokens.len() > 1 {
            let name = self.expect_str_and_eol()?;
            let name_id = self.kconf.interner.intern(&name);
            match self.kconf.named_choices.get(&name_id) {
                Some(&existing) => existing,
                None => {
                    let id = ChoiceId(self.kconf.choices.len() as u32);
                    self.kconf.choices.push(Choice::new(Some(name_id), n_expr));
                    self.kconf.named_choices.insert(name_id, id);
                    id
                }
            }
        } else {
            let id = ChoiceId(self.kconf.choices.len() as u32);
            self.kconf.choices.push(Choice::new(None, n_expr));
            id
        };
        self.kconf.unique_choices.push(choice);

        let node = self.new_node(Item::Choice(choice), parent);
        self.kconf.node_mut(node).is_menuconfig = true;
        self.kconf.choice_mut(choice).nodes.push(node);

        self.parse_props(node)?;
        self.parse_block(Some(Kw::EndChoice), node, node)?;
        let first_child = self.kconf.node(node).next;
        self.kconf.node_mut(node).list = first_child;

        self.kconf.node_mut(prev).next = Some(node);
        Ok(node)
    }

    fn parse_source(&mut self, kw: Kw, parent: NodeId, prev: NodeId) -> Result<NodeId> {
        let mut pattern = self.expect_str_and_eol()?;
        if kw.is_relative_source() {
            let dir = Path::new(self.loc_file())
                .parent()
                .unwrap_or(Path::new(""))
                .to_path_buf();
            pattern = dir.join(&pattern).to_string_lossy().into_owned();
        }

        let full = Path::new(&self.kconf.srctree_prefix).join(&pattern);
        let matches = expand_source_pattern(&full);

        let mut prev = prev;
        let mut sourced = 0;
        for path in matches {
            let display = self.relative_path(&path);
            if !self.push_file(&path, display)? {
                continue;
            }
            sourced += 1;
            let result = self.parse_block(None, parent, prev);
            self.pop_file();
            prev = result?;
        }

        if sourced == 0 && kw.is_obligatory_source() {
            return Err(Error(format!(
                "{}:{}: '{}' not found (in '{}'). Check that environment variables are set \
                 correctly (e.g. $srctree, which is {}). Also note that unset environment \
                 variables expand to the empty string.",
                self.loc_file(),
                self.loc.line,
                pattern,
                self.line.trim(),
                if self.kconf.srctree.is_empty() {
                    "unset or blank".to_string()
                } else {
                    format!("set to '{}'", self.kconf.srctree)
                }
            )));
        }
        Ok(prev)
    }

    fn relative_path(&self, path: &Path) -> String {
        let text = path.to_str().unwrap_or_default();
        let prefix = &self.kconf.srctree_prefix;
        let relative = match text.strip_prefix(prefix.as_str()) {
            Some(rel) if !prefix.is_empty() => rel,
            _ => text,
        };
        if relative.contains('\\') {
            relative.replace('\\', "/")
        } else {
            relative.to_string()
        }
    }

    // -- property parsing -------------------------------------------------

    /// Parses the property lines that follow a block header, stopping at the
    /// first line that is not a property.
    fn parse_props(&mut self, node: NodeId) -> Result<()> {
        self.kconf.node_mut(node).dep = self.kconf.exprs.y;

        while self.next_line()? {
            let Some(kw) = self.head() else { continue };

            match kw {
                kw if kw.is_type() => {
                    self.set_type(node, type_of(kw))?;
                    if self.tokens.len() > 1 {
                        self.parse_prompt(node)?;
                    }
                }

                Kw::Depends => {
                    if !self.accept(Kw::On) {
                        return Err(self.error("expected 'on' after 'depends'"));
                    }
                    let dep = self.parse_depends()?;
                    let old = self.kconf.node(node).dep;
                    self.kconf.node_mut(node).dep = self.kconf.exprs.and(old, dep);
                }

                Kw::Help => self.parse_help(node)?,

                Kw::Select | Kw::Imply => {
                    if !matches!(self.kconf.node(node).item, Item::Symbol(_)) {
                        return Err(self.error("only symbols can select"));
                    }
                    let target = self.expect_nonconst_sym()?;
                    let cond = self.parse_cond()?;
                    let sel = Selection { target, cond };
                    let n = self.kconf.node_mut(node);
                    if kw == Kw::Select {
                        n.selects.push(sel);
                    } else {
                        n.implies.push(sel);
                    }
                }

                Kw::Default => {
                    let value = self.parse_expr(false)?;
                    let cond = self.parse_cond()?;
                    self.kconf
                        .node_mut(node)
                        .defaults
                        .push(DefaultProp { value, cond });
                }

                Kw::DefBool | Kw::DefTristate | Kw::DefInt | Kw::DefHex | Kw::DefString => {
                    self.set_type(node, def_type_of(kw))?;
                    let value = self.parse_expr(false)?;
                    let cond = self.parse_cond()?;
                    self.kconf
                        .node_mut(node)
                        .defaults
                        .push(DefaultProp { value, cond });
                }

                Kw::Prompt => self.parse_prompt(node)?,

                Kw::Range => {
                    let low = self.expect_sym()?;
                    let high = self.expect_sym()?;
                    let cond = self.parse_cond()?;
                    self.kconf
                        .node_mut(node)
                        .ranges
                        .push(Range { low, high, cond });
                }

                Kw::Visible => {
                    if !self.accept(Kw::If) {
                        return Err(self.error("expected 'if' after 'visible'"));
                    }
                    let vis = self.parse_expr_and_eol(true)?;
                    let old = self.kconf.node(node).visibility;
                    self.kconf.node_mut(node).visibility = self.kconf.exprs.and(old, vis);
                }

                Kw::Option => self.parse_option(node)?,

                Kw::Modules => {}

                Kw::Optional => {
                    let Item::Choice(choice) = self.kconf.node(node).item else {
                        return Err(self.error("\"optional\" is only valid for choices"));
                    };
                    self.kconf.choice_mut(choice).is_optional = true;
                }

                _ => {
                    self.reuse_tokens = true;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn parse_option(&mut self, node: NodeId) -> Result<()> {
        if self.accept(Kw::Env) {
            if self.peek().and_then(Token::keyword).is_some() || !self.accept_eq() {
                return Err(self.error("expected '=' after 'env'"));
            }
            let var = self.expect_str_and_eol()?;
            let var_id = self.kconf.interner.intern(&var);
            let Item::Symbol(sym) = self.kconf.node(node).item else {
                return Err(self.error("'option env' is only valid for symbols"));
            };
            self.kconf.sym_mut(sym).env_var = Some(var_id);

            match std::env::var(&var) {
                Ok(value) => {
                    let const_sym = self.kconf.lookup_const_sym(&value);
                    let y = self.kconf.exprs.y;
                    let value_expr = self.kconf.exprs.sym(const_sym);
                    self.kconf.node_mut(node).defaults.push(DefaultProp {
                        value: value_expr,
                        cond: y,
                    });
                }
                Err(_) => {
                    let name = self.kconf.name(sym).to_string();
                    self.warn(format!(
                        "{name} has 'option env=\"{var}\"', but the environment variable {var} \
                         is not set"
                    ));
                }
            }
            return Ok(());
        }

        if self.accept(Kw::DefconfigList) {
            if let Item::Symbol(sym) = self.kconf.node(node).item {
                if self.kconf.defconfig_list.is_none() {
                    self.kconf.defconfig_list = Some(sym);
                }
            }
            return Ok(());
        }

        if self.accept(Kw::Modules) {
            return Ok(());
        }

        if self.accept(Kw::AllnoconfigY) {
            let Item::Symbol(sym) = self.kconf.node(node).item else {
                return Err(self.error("the 'allnoconfig_y' option is only valid for symbols"));
            };
            self.kconf.sym_mut(sym).is_allnoconfig_y = true;
            return Ok(());
        }

        Err(self.error("unrecognized option"))
    }

    fn accept_eq(&mut self) -> bool {
        if self.peek() == Some(&Token::Op(Op::Eq)) {
            self.at += 1;
            return true;
        }
        false
    }

    fn set_type(&mut self, node: NodeId, new_type: Type) -> Result<()> {
        let (old, name) = match self.kconf.node(node).item {
            Item::Symbol(sym) => (self.kconf.sym(sym).kind, self.kconf.name(sym).to_string()),
            Item::Choice(choice) => (
                self.kconf.choice(choice).kind,
                self.kconf
                    .choice(choice)
                    .name
                    .map(|n| self.kconf.interner[n].to_string())
                    .unwrap_or_else(|| "<choice>".to_string()),
            ),
            _ => return Err(self.error("type set on a menu or comment")),
        };
        if old != Type::Unknown && old != new_type {
            self.warn(format!(
                "{name} defined with multiple types, {} will be used",
                new_type.as_str()
            ));
        }
        match self.kconf.node(node).item {
            Item::Symbol(sym) => self.kconf.sym_mut(sym).kind = new_type,
            Item::Choice(choice) => self.kconf.choice_mut(choice).kind = new_type,
            _ => {}
        }
        Ok(())
    }

    fn parse_prompt(&mut self, node: NodeId) -> Result<()> {
        if self.kconf.node(node).prompt.is_some() {
            let what = self.describe(node);
            self.warn(format!(
                "{what} defined with multiple prompts in single location"
            ));
        }
        let Parser { kconf, tokens, .. } = self;
        let Some(prompt) = tokens.get(1).and_then(Token::text) else {
            return Err(self.error("expected prompt string"));
        };
        let trimmed = prompt.trim();
        let had_padding = trimmed.len() != prompt.len();
        // Trailing whitespace breaks e.g. reStructuredText's `*prompt *`.
        let text = kconf.interner.intern(trimmed);
        self.at = 2;

        if had_padding {
            let what = self.describe(node);
            self.warn(format!(
                "{what} has leading or trailing whitespace in its prompt"
            ));
        }
        let cond = self.parse_cond()?;
        self.kconf.node_mut(node).prompt = Some((text, cond));
        Ok(())
    }

    fn describe(&self, node: NodeId) -> String {
        match self.kconf.node(node).item {
            Item::Symbol(sym) => self.kconf.name(sym).to_string(),
            Item::Choice(choice) => self
                .kconf
                .choice(choice)
                .name
                .map(|n| self.kconf.interner[n].to_string())
                .unwrap_or_else(|| "<choice>".to_string()),
            _ => "<menu>".to_string(),
        }
    }

    /// Reads a help text: everything indented at least as far as its first
    /// non-blank line.
    fn parse_help(&mut self, node: NodeId) -> Result<()> {
        if self.kconf.node(node).help.is_some() {
            let what = self.describe(node);
            self.warn(format!(
                "{what} defined with more than one help text -- only the last one will be used"
            ));
        }

        let mut raw = String::new();
        // Skip blank lines to find the one that sets the indentation.
        loop {
            if !self.read_raw_line(&mut raw) {
                self.kconf.node_mut(node).help = Some(String::new().into_boxed_str());
                return Ok(());
            }
            if !raw.trim().is_empty() {
                break;
            }
        }

        let indent = indent_width(&raw);
        if indent == 0 {
            let what = self.describe(node);
            self.warn(format!("{what} has 'help' but empty help text"));
            self.kconf.node_mut(node).help = Some(String::new().into_boxed_str());
            self.line = raw;
            self.after_help()?;
            return Ok(());
        }

        let mut text = std::mem::take(&mut self.help_buf);
        text.clear();
        push_dedented(&mut text, &raw, indent);

        let ended_early = loop {
            if !self.read_raw_line(&mut raw) {
                break false;
            }
            if raw.trim().is_empty() {
                // Blank lines inside help do not need their spacing preserved.
                text.push('\n');
                continue;
            }
            if indent_width(&raw) < indent {
                break true;
            }
            push_dedented(&mut text, &raw, indent);
        };

        self.kconf.node_mut(node).help = Some(text.trim_end().into());
        self.help_buf = text;

        if ended_early {
            self.line = raw;
            self.after_help()?;
        }
        Ok(())
    }

    /// Tokenizes the line that ended a help text, which has already been read.
    fn after_help(&mut self) -> Result<()> {
        let mut cont = String::new();
        while self.line.ends_with("\\\n") {
            self.line.truncate(self.line.len() - 2);
            if !self.read_raw_line(&mut cont) {
                break;
            }
            self.line.push_str(&cont);
        }
        self.sync_location();
        self.tokenize_current()?;
        self.reuse_tokens = true;
        Ok(())
    }

    // -- expressions ------------------------------------------------------

    /// Parses an optional `if <expr>` suffix, defaulting to `y`.
    fn parse_cond(&mut self) -> Result<ExprId> {
        let expr = if self.accept(Kw::If) {
            self.parse_expr(true)?
        } else {
            self.kconf.exprs.y
        };
        self.expect_eol()?;
        Ok(expr)
    }

    /// Parses `depends on <expr> [if <cond>]`. The conditional form is a
    /// Kconfiglib extension meaning "if cond does not hold, the dependency is
    /// satisfied", i.e. `!cond || expr`.
    fn parse_depends(&mut self) -> Result<ExprId> {
        let main = self.parse_expr(true)?;
        if self.accept(Kw::If) {
            let cond = self.parse_expr(true)?;
            self.expect_eol()?;
            let negated = self.kconf.exprs.not(cond);
            return Ok(self.kconf.exprs.or(negated, main));
        }
        self.expect_eol()?;
        Ok(main)
    }

    fn parse_expr_and_eol(&mut self, transform_m: bool) -> Result<ExprId> {
        let expr = self.parse_expr(transform_m)?;
        self.expect_eol()?;
        Ok(expr)
    }

    /// `expr: and_expr ['||' expr]`
    fn parse_expr(&mut self, transform_m: bool) -> Result<ExprId> {
        let lhs = self.parse_and_expr(transform_m)?;
        if self.peek() == Some(&Token::Op(Op::Or)) {
            self.at += 1;
            let rhs = self.parse_expr(transform_m)?;
            return Ok(self.kconf.exprs.or(lhs, rhs));
        }
        Ok(lhs)
    }

    /// `and_expr: factor ['&&' and_expr]`
    fn parse_and_expr(&mut self, transform_m: bool) -> Result<ExprId> {
        let lhs = self.parse_factor(transform_m)?;
        if self.peek() == Some(&Token::Op(Op::And)) {
            self.at += 1;
            let rhs = self.parse_and_expr(transform_m)?;
            return Ok(self.kconf.exprs.and(lhs, rhs));
        }
        Ok(lhs)
    }

    /// `factor: <symbol> [<relation> <symbol>] | '!' factor | '(' expr ')'`
    fn parse_factor(&mut self, transform_m: bool) -> Result<ExprId> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;

        match token {
            Some(Token::Sym(sym)) => {
                if let Some(op) = self.peek().and_then(relation_of) {
                    self.at += 1;
                    let rhs = self.expect_sym()?;
                    return Ok(self.kconf.exprs.cmp(op, sym, rhs));
                }
                // In conditions, a bare `m` means "m and modules are on".
                if transform_m && sym == self.kconf.m {
                    let m = self.kconf.exprs.m;
                    let modules = self.kconf.modules;
                    let modules = self.kconf.exprs.sym(modules);
                    return Ok(self.kconf.exprs.and(m, modules));
                }
                Ok(self.kconf.exprs.sym(sym))
            }
            Some(Token::Op(Op::Not)) => {
                let inner = self.parse_factor(transform_m)?;
                Ok(self.kconf.exprs.not(inner))
            }
            Some(Token::Op(Op::LParen)) => {
                let inner = self.parse_expr(transform_m)?;
                if self.peek() == Some(&Token::Op(Op::RParen)) {
                    self.at += 1;
                    return Ok(inner);
                }
                Err(self.error("malformed expression"))
            }
            _ => Err(self.error("malformed expression")),
        }
    }
}

fn relation_of(token: &Token) -> Option<CmpOp> {
    match token {
        Token::Op(Op::Eq) => Some(CmpOp::Eq),
        Token::Op(Op::Ne) => Some(CmpOp::Ne),
        Token::Op(Op::Lt) => Some(CmpOp::Lt),
        Token::Op(Op::Le) => Some(CmpOp::Le),
        Token::Op(Op::Gt) => Some(CmpOp::Gt),
        Token::Op(Op::Ge) => Some(CmpOp::Ge),
        _ => None,
    }
}

fn type_of(kw: Kw) -> Type {
    match kw {
        Kw::Bool => Type::Bool,
        Kw::Tristate => Type::Tristate,
        Kw::Int => Type::Int,
        Kw::Hex => Type::Hex,
        _ => Type::String,
    }
}

fn def_type_of(kw: Kw) -> Type {
    match kw {
        Kw::DefBool => Type::Bool,
        Kw::DefTristate => Type::Tristate,
        Kw::DefInt => Type::Int,
        Kw::DefHex => Type::Hex,
        _ => Type::String,
    }
}

/// Reads `path` into `out`, which is recycled across files and therefore
/// already has capacity — so, unlike `fs::read_to_string`, this needs no
/// `stat` to size the buffer.
fn read_file_into(path: &Path, out: &mut String) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open(path)?.read_to_string(out)?;
    Ok(())
}

/// The indentation of `line` in columns, counting a tab as advancing to the
/// next multiple of 8 — Python's `str.expandtabs()` default, which is what
/// decides how far a help text is indented.
fn indent_width(line: &str) -> usize {
    let mut column = 0;
    for &c in line.as_bytes() {
        match c {
            b'\t' => column += 8 - (column % 8),
            b' ' => column += 1,
            _ => break,
        }
    }
    column
}

/// Appends `line` to `out` with tabs expanded and the first `indent` columns
/// of leading whitespace removed.
fn push_dedented(out: &mut String, line: &str, indent: usize) {
    let bytes = line.as_bytes();
    let mut column = 0;
    let mut start = bytes.len();
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'\t' => column += 8 - (column % 8),
            b' ' => column += 1,
            _ => {
                start = i;
                break;
            }
        }
        if column >= indent {
            start = i + 1;
            break;
        }
    }

    // A tab may have overshot the cut; keep the columns past it.
    for _ in indent..column {
        out.push(' ');
    }

    // Tabs inside the text still expand relative to their column.
    let rest = &line[start..];
    if !rest.as_bytes().contains(&b'\t') {
        out.push_str(rest);
        return;
    }
    let mut column = column.max(indent);
    for c in rest.chars() {
        match c {
            '\t' => {
                let width = 8 - (column % 8);
                for _ in 0..width {
                    out.push(' ');
                }
                column += width;
            }
            '\n' | '\r' => {
                out.push(c);
                column = 0;
            }
            _ => {
                out.push(c);
                column += 1;
            }
        }
    }
}

/// Expands a `source` pattern into the files to read, sorted so that symbol
/// order — and therefore .config order — is stable.
///
/// Most `source` lines name a file outright. Those are returned as-is: the
/// caller finds out whether the file exists by opening it, which saves a
/// `stat` per `source` across thousands of them.
fn expand_source_pattern(pattern: &Path) -> Vec<PathBuf> {
    let text = pattern.to_str().unwrap_or_default();
    if !text.contains(['*', '?', '[']) {
        return vec![pattern.to_path_buf()];
    }

    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: true,
    };
    let Ok(paths) = glob::glob_with(text, options) else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = paths.filter_map(std::result::Result::ok).collect();
    matches.sort();
    matches
}
