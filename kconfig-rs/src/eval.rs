//! Symbol value computation.
//!
//! Values are derived, not stored: a symbol's value follows from its
//! visibility, its `default`s, what `select`s force onto it, and what the user
//! assigned. Computing one value pulls on others, so everything is memoized.
//!
//! # Invalidation
//!
//! Kconfiglib tracks, for every symbol, the set of symbols whose value might
//! change if it changes, and walks that set on assignment. This module drops
//! all cached values instead ([`Values::reset`]). Recomputing every symbol in
//! the Linux tree and re-rendering the whole `.config` costs about seven
//! milliseconds, so the bookkeeping buys nothing outside an interactive
//! `menuconfig` loop, and the simpler rule is easier to reason about.
//!
//! Expression results are memoized too, which is only possible because
//! expressions are hash-consed: identical conditions propagated into hundreds
//! of symbols share one cache slot.

use crate::expr::{CmpOp, Expr, ExprId};
use crate::model::{ChoiceId, Item, SymbolId, Tri, Type, UserValue};
use crate::Kconfig;

/// Cached values for one configuration state.
pub struct Values {
    sym_tri: Vec<Option<Tri>>,
    sym_str: Vec<Option<Box<str>>>,
    sym_vis: Vec<Option<Tri>>,
    /// Whether the symbol should appear in .config, set as a side effect of
    /// computing its value.
    write_to_conf: Vec<bool>,
    /// Guards against re-entering a symbol whose value is still being computed.
    computing: Vec<bool>,

    expr_value: Vec<Option<Tri>>,

    choice_vis: Vec<Option<Tri>>,
    choice_tri: Vec<Option<Tri>>,
    choice_selection: Vec<Option<Option<SymbolId>>>,

    modules: Option<Tri>,
}

impl Values {
    pub fn new(kconf: &Kconfig) -> Values {
        let syms = kconf.symbols.len();
        let mut values = Values {
            sym_tri: vec![None; syms],
            sym_str: vec![None; syms],
            sym_vis: vec![None; syms],
            write_to_conf: vec![false; syms],
            computing: vec![false; syms],
            expr_value: vec![None; kconf.exprs.len()],
            choice_vis: vec![None; kconf.choices.len()],
            choice_tri: vec![None; kconf.choices.len()],
            choice_selection: vec![None; kconf.choices.len()],
            modules: None,
        };
        // The constant symbols n, m and y never change.
        for (i, tri) in [Tri::N, Tri::M, Tri::Y].into_iter().enumerate() {
            values.sym_tri[i] = Some(tri);
        }
        values
    }

    /// Discards every cached value. Call after changing a user value.
    pub fn reset(&mut self, kconf: &Kconfig) {
        *self = Values::new(kconf);
    }

    // -- expressions ------------------------------------------------------

    pub fn expr(&mut self, kconf: &Kconfig, id: ExprId) -> Tri {
        if let Some(v) = self.expr_value[id.index()] {
            return v;
        }
        let value = match kconf.exprs.get(id) {
            Expr::Sym(sym) => self.tri_of(kconf, sym),
            Expr::Not(e) => self.expr(kconf, e).negate(),
            Expr::And(a, b) => {
                let v = self.expr(kconf, a);
                if v == Tri::N {
                    Tri::N
                } else {
                    v.min(self.expr(kconf, b))
                }
            }
            Expr::Or(a, b) => {
                let v = self.expr(kconf, a);
                if v == Tri::Y {
                    Tri::Y
                } else {
                    v.max(self.expr(kconf, b))
                }
            }
            Expr::Cmp(op, lhs, rhs) => self.compare(kconf, op, lhs, rhs),
        };
        self.expr_value[id.index()] = Some(value);
        value
    }

    fn compare(&mut self, kconf: &Kconfig, op: CmpOp, lhs: SymbolId, rhs: SymbolId) -> Tri {
        // Two strings compare lexicographically; anything else is compared
        // numerically when both sides parse as numbers, and lexicographically
        // otherwise.
        let both_strings =
            kconf.sym(lhs).kind == Type::String && kconf.sym(rhs).kind == Type::String;

        let ordering = if both_strings {
            let a = self.str_of(kconf, lhs).to_string();
            let b = self.str_of(kconf, rhs);
            a.as_str().cmp(b)
        } else {
            match (self.numeric(kconf, lhs), self.numeric(kconf, rhs)) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => {
                    let a = self.str_of(kconf, lhs).to_string();
                    let b = self.str_of(kconf, rhs);
                    a.as_str().cmp(b)
                }
            }
        };

        let holds = match op {
            CmpOp::Eq => ordering.is_eq(),
            CmpOp::Ne => ordering.is_ne(),
            CmpOp::Lt => ordering.is_lt(),
            CmpOp::Le => ordering.is_le(),
            CmpOp::Gt => ordering.is_gt(),
            CmpOp::Ge => ordering.is_ge(),
        };
        if holds {
            Tri::Y
        } else {
            Tri::N
        }
    }

    /// A symbol as a number, for relational operators. bool/tristate count as
    /// 0/1/2.
    fn numeric(&mut self, kconf: &Kconfig, sym: SymbolId) -> Option<i128> {
        if kconf.sym(sym).kind.is_bool_tristate() {
            return Some(self.tri_of(kconf, sym).0 as i128);
        }
        let text = self.str_of(kconf, sym).to_string();
        // Base 0: the value's own prefix decides how to read it.
        python_int(&text, 0)
    }

    // -- types ------------------------------------------------------------

    fn modules_on(&mut self, kconf: &Kconfig) -> bool {
        if let Some(v) = self.modules {
            return v != Tri::N;
        }
        // Break the cycle if MODULES' own evaluation asks for the type of
        // another symbol.
        self.modules = Some(Tri::N);
        let value = self.tri_of(kconf, kconf.modules);
        self.modules = Some(value);
        value != Tri::N
    }

    /// A symbol's type after modules-off and y-mode-choice promotion.
    fn sym_type(&mut self, kconf: &Kconfig, sym: SymbolId) -> Type {
        if kconf.sym(sym).kind != Type::Tristate {
            return kconf.sym(sym).kind;
        }
        let in_y_choice = match kconf.sym(sym).choice {
            Some(choice) => self.choice_tri(kconf, choice) == Tri::Y,
            None => false,
        };
        if in_y_choice || !self.modules_on(kconf) {
            Type::Bool
        } else {
            Type::Tristate
        }
    }

    fn choice_type(&mut self, kconf: &Kconfig, choice: ChoiceId) -> Type {
        if kconf.choice(choice).kind == Type::Tristate && !self.modules_on(kconf) {
            Type::Bool
        } else {
            kconf.choice(choice).kind
        }
    }

    // -- visibility -------------------------------------------------------

    pub fn visibility(&mut self, kconf: &Kconfig, sym: SymbolId) -> Tri {
        if let Some(v) = self.sym_vis[sym.index()] {
            return v;
        }
        let mut vis = Tri::N;
        for i in 0..kconf.sym(sym).nodes.len() {
            let node = kconf.sym(sym).nodes[i];
            if let Some((_, cond)) = kconf.node(node).prompt {
                vis = vis.max(self.expr(kconf, cond));
            }
        }

        if let Some(choice) = kconf.sym(sym).choice {
            let choice_tri = self.choice_tri(kconf, choice);
            let choice_is_tristate = kconf.choice(choice).kind == Type::Tristate;
            // A non-tristate symbol inside a tristate choice is only offered
            // when the choice is in y mode.
            if choice_is_tristate && kconf.sym(sym).kind != Type::Tristate && choice_tri != Tri::Y {
                self.sym_vis[sym.index()] = Some(Tri::N);
                return Tri::N;
            }
            // A tristate symbol with m visibility is hidden in a y-mode choice.
            if kconf.sym(sym).kind == Type::Tristate && vis == Tri::M && choice_tri == Tri::Y {
                self.sym_vis[sym.index()] = Some(Tri::N);
                return Tri::N;
            }
        }

        // m visibility becomes y for anything that cannot hold m.
        if vis == Tri::M && self.sym_type(kconf, sym) != Type::Tristate {
            vis = Tri::Y;
        }
        self.sym_vis[sym.index()] = Some(vis);
        vis
    }

    pub fn choice_visibility(&mut self, kconf: &Kconfig, choice: ChoiceId) -> Tri {
        if let Some(v) = self.choice_vis[choice.index()] {
            return v;
        }
        let mut vis = Tri::N;
        for i in 0..kconf.choice(choice).nodes.len() {
            let node = kconf.choice(choice).nodes[i];
            if let Some((_, cond)) = kconf.node(node).prompt {
                vis = vis.max(self.expr(kconf, cond));
            }
        }
        if vis == Tri::M && self.choice_type(kconf, choice) != Type::Tristate {
            vis = Tri::Y;
        }
        self.choice_vis[choice.index()] = Some(vis);
        vis
    }

    // -- symbol values ----------------------------------------------------

    pub fn tri_of(&mut self, kconf: &Kconfig, sym: SymbolId) -> Tri {
        if let Some(v) = self.sym_tri[sym.index()] {
            return v;
        }
        if self.computing[sym.index()] {
            // A dependency loop. The C tools reject these at load time; here
            // we simply stop descending.
            return Tri::N;
        }

        // A choice contributes to expressions through a hidden proxy symbol.
        if let Some(choice) = kconf.sym(sym).choice_proxy_for {
            let value = self.choice_tri(kconf, choice);
            self.sym_tri[sym.index()] = Some(value);
            return value;
        }

        if !kconf.sym(sym).kind.is_bool_tristate() {
            self.sym_tri[sym.index()] = Some(Tri::N);
            return Tri::N;
        }

        self.computing[sym.index()] = true;
        let vis = self.visibility(kconf, sym);
        self.write_to_conf[sym.index()] = vis != Tri::N;

        let mut val = Tri::N;
        match kconf.sym(sym).choice {
            None => {
                let user = match &kconf.sym(sym).user_value {
                    Some(UserValue::Tri(t)) if vis != Tri::N => Some(*t),
                    _ => None,
                };
                if let Some(user) = user {
                    val = user.min(vis);
                } else {
                    for i in 0..kconf.sym(sym).defaults.len() {
                        let d = kconf.sym(sym).defaults[i];
                        let cond = self.expr(kconf, d.cond);
                        if cond != Tri::N {
                            val = self.expr(kconf, d.value).min(cond);
                            if val != Tri::N {
                                self.write_to_conf[sym.index()] = true;
                            }
                            break;
                        }
                    }

                    // `imply` only applies when the direct dependencies hold.
                    let weak = self.expr(kconf, kconf.sym(sym).weak_rev_dep);
                    if weak != Tri::N && self.expr(kconf, kconf.sym(sym).direct_dep) != Tri::N {
                        val = val.max(weak);
                        self.write_to_conf[sym.index()] = true;
                    }
                }

                // `select` overrides everything, including invisibility.
                let forced = self.expr(kconf, kconf.sym(sym).rev_dep);
                if forced != Tri::N {
                    val = val.max(forced);
                    self.write_to_conf[sym.index()] = true;
                }

                // m becomes y for bools, and for anything implied to y.
                if val == Tri::M {
                    let weak_is_y = self.expr(kconf, kconf.sym(sym).weak_rev_dep) == Tri::Y;
                    if self.sym_type(kconf, sym) == Type::Bool || weak_is_y {
                        val = Tri::Y;
                    }
                }
            }
            Some(choice) if vis == Tri::Y => {
                // In a y-mode choice exactly one symbol is selected. The
                // choice's mode already caps the symbols' visibility, so
                // checking visibility here is enough.
                val = if self.choice_selection(kconf, choice) == Some(sym) {
                    Tri::Y
                } else {
                    Tri::N
                };
            }
            Some(_) => {
                let user_on = matches!(
                    &kconf.sym(sym).user_value,
                    Some(UserValue::Tri(t)) if *t != Tri::N
                );
                if vis != Tri::N && user_on {
                    val = Tri::M;
                }
            }
        }

        self.computing[sym.index()] = false;
        self.sym_tri[sym.index()] = Some(val);
        val
    }

    pub fn str_of(&mut self, kconf: &Kconfig, sym: SymbolId) -> &str {
        if self.sym_str[sym.index()].is_some() {
            return self.sym_str[sym.index()].as_deref().expect("just checked");
        }
        let value = self.compute_str(kconf, sym);
        self.sym_str[sym.index()] = Some(value.into_boxed_str());
        self.sym_str[sym.index()].as_deref().expect("just stored")
    }

    fn compute_str(&mut self, kconf: &Kconfig, sym: SymbolId) -> String {
        let kind = kconf.sym(sym).kind;
        if kind.is_bool_tristate() {
            return self.tri_of(kconf, sym).as_str().to_string();
        }
        // Undefined symbols — and constant symbols, which are the same thing
        // with a fixed name — evaluate to their own name. That is what makes
        // `FOO = bar` work as a comparison against the literal `bar`.
        if kind == Type::Unknown {
            return kconf.name(sym).to_string();
        }

        let vis = self.visibility(kconf, sym);
        self.write_to_conf[sym.index()] = vis != Tri::N;
        let mut val = String::new();

        if kind.is_int_hex() {
            let base = kind.base();
            let mut active_range = None;
            for i in 0..kconf.sym(sym).ranges.len() {
                let r = kconf.sym(sym).ranges[i];
                if self.expr(kconf, r.cond) != Tri::N {
                    let low = self.str_of(kconf, r.low).to_string();
                    let high = self.str_of(kconf, r.high).to_string();
                    // The C tools run strtoll() on these, which yields 0 for
                    // anything unparseable.
                    active_range = Some((
                        python_int(&low, base).unwrap_or(0),
                        python_int(&high, base).unwrap_or(0),
                    ));
                    break;
                }
            }

            let mut use_defaults = true;
            if vis != Tri::N {
                if let Some(UserValue::Str(user)) = &kconf.sym(sym).user_value {
                    let user = user.clone();
                    if let Some(num) = python_int(&user, base) {
                        let in_range = active_range.is_none_or(|(lo, hi)| lo <= num && num <= hi);
                        if in_range {
                            val = user;
                            use_defaults = false;
                        }
                    }
                }
            }

            if use_defaults {
                let mut value_num = 0i128;
                for i in 0..kconf.sym(sym).defaults.len() {
                    let d = kconf.sym(sym).defaults[i];
                    if self.expr(kconf, d.cond) != Tri::N {
                        self.write_to_conf[sym.index()] = true;
                        let Expr::Sym(value_sym) = kconf.exprs.get(d.value) else {
                            break;
                        };
                        val = self.str_of(kconf, value_sym).to_string();
                        value_num = python_int(&val, base).unwrap_or(0);
                        break;
                    }
                }
                if let Some((low, high)) = active_range {
                    let clamped = value_num.clamp(low, high);
                    if clamped != value_num {
                        val = match kind {
                            Type::Int => clamped.to_string(),
                            _ => format!("0x{clamped:x}"),
                        };
                    }
                }
            }
        } else if kind == Type::String {
            let user = match &kconf.sym(sym).user_value {
                Some(UserValue::Str(s)) if vis != Tri::N => Some(s.clone()),
                _ => None,
            };
            if let Some(user) = user {
                val = user;
            } else {
                for i in 0..kconf.sym(sym).defaults.len() {
                    let d = kconf.sym(sym).defaults[i];
                    if self.expr(kconf, d.cond) != Tri::N {
                        let Expr::Sym(value_sym) = kconf.exprs.get(d.value) else {
                            break;
                        };
                        val = self.str_of(kconf, value_sym).to_string();
                        self.write_to_conf[sym.index()] = true;
                        break;
                    }
                }
            }
        }

        // Symbols that only mirror an environment variable are never written.
        if kconf.sym(sym).env_var.is_some() || Some(sym) == kconf.defconfig_list {
            self.write_to_conf[sym.index()] = false;
        }
        val
    }

    /// Whether `sym` belongs in .config. Only meaningful after its value has
    /// been computed.
    pub fn is_written(&mut self, kconf: &Kconfig, sym: SymbolId) -> bool {
        let _ = self.str_of(kconf, sym);
        self.write_to_conf[sym.index()]
    }

    // -- choices ----------------------------------------------------------

    pub fn choice_tri(&mut self, kconf: &Kconfig, choice: ChoiceId) -> Tri {
        if let Some(v) = self.choice_tri[choice.index()] {
            return v;
        }
        // Provisional value, so that visibility computations that ask for the
        // choice's mode while we are computing it terminate.
        self.choice_tri[choice.index()] = Some(Tri::N);

        // A non-optional choice behaves as if something implied `m`.
        let mut val = if kconf.choice(choice).is_optional {
            Tri::N
        } else {
            Tri::M
        };
        if let Some(user) = kconf.choice(choice).user_value {
            val = val.max(user);
        }
        val = val.min(self.choice_visibility(kconf, choice));
        if val == Tri::M && self.choice_type(kconf, choice) == Type::Bool {
            val = Tri::Y;
        }

        self.choice_tri[choice.index()] = Some(val);
        val
    }

    pub fn choice_selection(&mut self, kconf: &Kconfig, choice: ChoiceId) -> Option<SymbolId> {
        if let Some(v) = self.choice_selection[choice.index()] {
            return v;
        }
        self.choice_selection[choice.index()] = Some(None);

        if self.choice_tri(kconf, choice) != Tri::Y {
            return None;
        }

        // The user's pick wins if it is still visible.
        if let Some(user) = kconf.choice(choice).user_selection {
            if self.visibility(kconf, user) != Tri::N {
                self.choice_selection[choice.index()] = Some(Some(user));
                return Some(user);
            }
        }

        // Otherwise the first satisfied, visible `default`...
        for i in 0..kconf.choice(choice).defaults.len() {
            let d = kconf.choice(choice).defaults[i];
            if self.expr(kconf, d.cond) == Tri::N {
                continue;
            }
            let Expr::Sym(candidate) = kconf.exprs.get(d.value) else {
                continue;
            };
            if self.visibility(kconf, candidate) != Tri::N {
                self.choice_selection[choice.index()] = Some(Some(candidate));
                return Some(candidate);
            }
        }

        // ...and failing that, the first visible symbol.
        for i in 0..kconf.choice(choice).syms.len() {
            let candidate = kconf.choice(choice).syms[i];
            if self.visibility(kconf, candidate) != Tri::N {
                self.choice_selection[choice.index()] = Some(Some(candidate));
                return Some(candidate);
            }
        }
        None
    }

    /// Evaluates a menu node's own condition, used when deciding whether to
    /// print a menu header or comment.
    pub fn node_is_visible(&mut self, kconf: &Kconfig, node: crate::model::NodeId) -> bool {
        let n = kconf.node(node);
        match n.item {
            Item::Menu => {
                self.expr(kconf, n.dep) != Tri::N && self.expr(kconf, n.visibility) != Tri::N
            }
            _ => self.expr(kconf, n.dep) != Tri::N,
        }
    }
}

/// Python's `int(s, base)`, which is what Kconfiglib and the C tools both end
/// up using to read `range` bounds and numeric values.
///
/// Base 16, 8 and 2 accept the matching `0x`/`0o`/`0b` prefix; base 0 infers
/// the base from the prefix and rejects a redundant leading zero, as CPython
/// does. Underscores may separate digits.
pub fn python_int(s: &str, base: u32) -> Option<i128> {
    let s = s.trim();
    let (negative, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };

    let lower = s.to_ascii_lowercase();
    let (base, digits) = match (base, lower.as_bytes()) {
        (16, [b'0', b'x', ..]) | (0, [b'0', b'x', ..]) => (16, &s[2..]),
        (8, [b'0', b'o', ..]) | (0, [b'0', b'o', ..]) => (8, &s[2..]),
        (2, [b'0', b'b', ..]) | (0, [b'0', b'b', ..]) => (2, &s[2..]),
        (0, _) => {
            // CPython rejects "010" but accepts "0" and "00".
            let bytes = s.as_bytes();
            if bytes.first() == Some(&b'0') && bytes.iter().any(|&c| c != b'0' && c != b'_') {
                return None;
            }
            (10, s)
        }
        (base, _) => (base, s),
    };

    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    let mut value: i128 = 0;
    let mut last_was_underscore = false;
    for c in digits.chars() {
        if c == '_' {
            if last_was_underscore {
                return None;
            }
            last_was_underscore = true;
            continue;
        }
        last_was_underscore = false;
        let digit = c.to_digit(base)? as i128;
        value = value.checked_mul(base as i128)?.checked_add(digit)?;
    }
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::python_int;

    #[test]
    fn hex_bounds_accept_the_0x_prefix() {
        assert_eq!(python_int("0x10", 16), Some(16));
        assert_eq!(python_int("10", 16), Some(16));
        assert_eq!(python_int("15", 16), Some(21));
        assert_eq!(python_int("-0xff", 16), Some(-255));
    }

    #[test]
    fn decimal_rejects_prefixes_and_junk() {
        assert_eq!(python_int("21", 10), Some(21));
        assert_eq!(python_int("0x10", 10), None);
        assert_eq!(python_int("", 10), None);
        assert_eq!(python_int("12ab", 10), None);
    }

    #[test]
    fn base_zero_infers_from_the_prefix_like_cpython() {
        assert_eq!(python_int("0x1f", 0), Some(31));
        assert_eq!(python_int("0b101", 0), Some(5));
        assert_eq!(python_int("0o17", 0), Some(15));
        assert_eq!(python_int("42", 0), Some(42));
        assert_eq!(python_int("0", 0), Some(0));
        // CPython rejects a redundant leading zero in base 0.
        assert_eq!(python_int("010", 0), None);
    }

    #[test]
    fn underscores_may_separate_digits() {
        assert_eq!(python_int("1_000", 10), Some(1000));
        assert_eq!(python_int("_1", 10), None);
        assert_eq!(python_int("1__0", 10), None);
    }
}
