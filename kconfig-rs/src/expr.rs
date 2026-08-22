//! Boolean expressions, stored in a hash-consed arena.
//!
//! Dependency propagation ANDs a parent's `depends on` into every property of
//! every child, so the same subexpression is built over and over. Hash-consing
//! gives each distinct expression exactly one `ExprId`, which
//!
//!  * keeps memory flat (Linux collapses ~1.4M constructed expressions into
//!    ~150k unique ones), and
//!  * lets the evaluator memoize results per `ExprId` instead of re-walking
//!    identical trees.

use rustc_hash::FxHashMap;

use crate::model::SymbolId;

/// Index into [`ExprArena`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ExprId(pub u32);

impl ExprId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A comparison between two symbols, e.g. `FOO = "bar"` or `VER >= 3`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Expr {
    Sym(SymbolId),
    Not(ExprId),
    And(ExprId, ExprId),
    Or(ExprId, ExprId),
    Cmp(CmpOp, SymbolId, SymbolId),
}

/// The interning arena. `n`, `m` and `y` are always the first three entries.
pub struct ExprArena {
    nodes: Vec<Expr>,
    ids: FxHashMap<Expr, ExprId>,
    pub n: ExprId,
    pub m: ExprId,
    pub y: ExprId,
}

impl ExprArena {
    /// Builds an arena seeded with the constant symbols `n`, `m` and `y`,
    /// which the caller must have already allocated as symbols 0, 1 and 2.
    pub fn new(n_sym: SymbolId, m_sym: SymbolId, y_sym: SymbolId) -> ExprArena {
        let mut arena = ExprArena {
            nodes: Vec::new(),
            ids: FxHashMap::default(),
            n: ExprId(0),
            m: ExprId(0),
            y: ExprId(0),
        };
        arena.n = arena.sym(n_sym);
        arena.m = arena.sym(m_sym);
        arena.y = arena.sym(y_sym);
        arena
    }

    #[inline]
    pub fn get(&self, id: ExprId) -> Expr {
        self.nodes[id.index()]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn alloc(&mut self, expr: Expr) -> ExprId {
        if let Some(&id) = self.ids.get(&expr) {
            return id;
        }
        let id = ExprId(self.nodes.len() as u32);
        self.nodes.push(expr);
        self.ids.insert(expr, id);
        id
    }

    #[inline]
    pub fn sym(&mut self, sym: SymbolId) -> ExprId {
        self.alloc(Expr::Sym(sym))
    }

    /// The existing expression for `sym`, if one was ever built. Used where a
    /// symbol appears outside an expression (range bounds) but still needs to
    /// be walked as one.
    pub fn sym_expr(&self, sym: SymbolId) -> Option<ExprId> {
        self.ids.get(&Expr::Sym(sym)).copied()
    }

    pub fn cmp(&mut self, op: CmpOp, lhs: SymbolId, rhs: SymbolId) -> ExprId {
        self.alloc(Expr::Cmp(op, lhs, rhs))
    }

    pub fn not(&mut self, e: ExprId) -> ExprId {
        self.alloc(Expr::Not(e))
    }

    /// `a && b`, with the same constant folding the C tools and Kconfiglib do.
    pub fn and(&mut self, a: ExprId, b: ExprId) -> ExprId {
        if a == self.y {
            return b;
        }
        if b == self.y {
            return a;
        }
        if a == self.n || b == self.n {
            return self.n;
        }
        self.alloc(Expr::And(a, b))
    }

    /// `a || b`, with constant folding.
    pub fn or(&mut self, a: ExprId, b: ExprId) -> ExprId {
        if a == self.n {
            return b;
        }
        if b == self.n {
            return a;
        }
        if a == self.y || b == self.y {
            return self.y;
        }
        self.alloc(Expr::Or(a, b))
    }

    /// Calls `f` for every symbol mentioned anywhere in `expr`.
    pub fn for_each_symbol(&self, expr: ExprId, f: &mut impl FnMut(SymbolId)) {
        match self.get(expr) {
            Expr::Sym(s) => f(s),
            Expr::Not(e) => self.for_each_symbol(e, f),
            Expr::And(a, b) | Expr::Or(a, b) => {
                self.for_each_symbol(a, f);
                self.for_each_symbol(b, f);
            }
            Expr::Cmp(_, a, b) => {
                f(a);
                f(b);
            }
        }
    }

    /// Reimplements `expr_depends_symbol()` from the C tools' `mconf.c`, which
    /// decides whether an item should be pulled into an implicit submenu under
    /// `sym`.
    ///
    /// True when `expr` contains `sym` in a position that makes the whole
    /// expression false if `sym` is n: a bare `sym`, `sym = y`/`sym = m`,
    /// `sym != n`, or either side of an `&&`.
    pub fn depends_on(
        &self,
        expr: ExprId,
        sym: SymbolId,
        n: SymbolId,
        m: SymbolId,
        y: SymbolId,
    ) -> bool {
        match self.get(expr) {
            Expr::Sym(s) => s == sym,
            Expr::Cmp(op @ (CmpOp::Eq | CmpOp::Ne), lhs, rhs) => {
                // Normalize so that `sym` is on the left.
                let other = if lhs == sym {
                    rhs
                } else if rhs == sym {
                    lhs
                } else {
                    return false;
                };
                match op {
                    CmpOp::Eq => other == m || other == y,
                    _ => other == n,
                }
            }
            Expr::And(a, b) => self.depends_on(a, sym, n, m, y) || self.depends_on(b, sym, n, m, y),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arena() -> ExprArena {
        ExprArena::new(SymbolId(0), SymbolId(1), SymbolId(2))
    }

    #[test]
    fn identical_expressions_share_one_id() {
        let mut e = arena();
        let a = e.sym(SymbolId(10));
        let b = e.sym(SymbolId(11));
        assert_eq!(e.and(a, b), e.and(a, b));
        assert_ne!(e.and(a, b), e.and(b, a));
    }

    #[test]
    fn constants_fold_away() {
        let mut e = arena();
        let (n, y) = (e.n, e.y);
        let a = e.sym(SymbolId(10));

        assert_eq!(e.and(y, a), a);
        assert_eq!(e.and(a, y), a);
        assert_eq!(e.and(n, a), n);
        assert_eq!(e.and(a, n), n);

        assert_eq!(e.or(n, a), a);
        assert_eq!(e.or(a, n), a);
        assert_eq!(e.or(y, a), y);
        assert_eq!(e.or(a, y), y);
    }

    #[test]
    fn folding_keeps_the_arena_small() {
        let mut e = arena();
        let y = e.y;
        let a = e.sym(SymbolId(10));
        let before = e.len();
        for _ in 0..1000 {
            let _ = e.and(y, a);
        }
        assert_eq!(e.len(), before);
    }

    #[test]
    fn implicit_submenu_dependencies_are_recognized() {
        let mut e = arena();
        let (n, m, y) = (SymbolId(0), SymbolId(1), SymbolId(2));
        let target = SymbolId(10);
        let other = SymbolId(11);

        let bare = e.sym(target);
        assert!(e.depends_on(bare, target, n, m, y));

        let eq_y = e.cmp(CmpOp::Eq, target, y);
        assert!(e.depends_on(eq_y, target, n, m, y));

        let ne_n = e.cmp(CmpOp::Ne, target, n);
        assert!(e.depends_on(ne_n, target, n, m, y));

        // `FOO = n` does not gate a submenu, and neither does an unrelated symbol.
        let eq_n = e.cmp(CmpOp::Eq, target, n);
        assert!(!e.depends_on(eq_n, target, n, m, y));
        let unrelated = e.sym(other);
        assert!(!e.depends_on(unrelated, target, n, m, y));

        // Either side of an `&&` counts; neither side of an `||` does.
        let both = e.and(unrelated, bare);
        assert!(e.depends_on(both, target, n, m, y));
        let either = e.or(unrelated, bare);
        assert!(!e.depends_on(either, target, n, m, y));
    }
}
