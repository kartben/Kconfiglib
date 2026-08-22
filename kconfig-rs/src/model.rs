//! The data model: symbols, choices, and menu nodes.
//!
//! Everything lives in arenas and refers to everything else by index. That
//! keeps the graph — which is cyclic in several places — expressible without
//! reference counting or interior mutability, and it keeps related data
//! contiguous in memory.

use crate::expr::ExprId;
use crate::intern::StrId;

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(pub u32);

        impl $name {
            #[inline]
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id_type!(/// Index into [`Kconfig::symbols`](crate::Kconfig).
    SymbolId);
id_type!(/// Index into [`Kconfig::choices`](crate::Kconfig).
    ChoiceId);
id_type!(/// Index into [`Kconfig::nodes`](crate::Kconfig).
    NodeId);

/// A tristate value. `n < m < y`, so the ordering doubles as Kconfig's
/// `min`/`max` semantics.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Tri(pub u8);

impl Tri {
    pub const N: Tri = Tri(0);
    pub const M: Tri = Tri(1);
    pub const Y: Tri = Tri(2);

    #[inline]
    pub fn is_n(self) -> bool {
        self.0 == 0
    }

    /// `!self` in Kconfig's logic: `n -> y`, `m -> m`, `y -> n`.
    #[inline]
    pub fn negate(self) -> Tri {
        Tri(2 - self.0)
    }

    pub fn as_str(self) -> &'static str {
        match self.0 {
            0 => "n",
            1 => "m",
            _ => "y",
        }
    }

    /// Parses `n`, `m` or `y`. Not `FromStr`, since it is only ever used on
    /// these three literals.
    pub fn parse(s: &str) -> Option<Tri> {
        match s {
            "n" => Some(Tri::N),
            "m" => Some(Tri::M),
            "y" => Some(Tri::Y),
            _ => None,
        }
    }
}

/// The declared type of a symbol or choice.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Type {
    #[default]
    Unknown,
    Bool,
    Tristate,
    Int,
    Hex,
    String,
}

impl Type {
    #[inline]
    pub fn is_bool_tristate(self) -> bool {
        matches!(self, Type::Bool | Type::Tristate)
    }

    #[inline]
    pub fn is_int_hex(self) -> bool {
        matches!(self, Type::Int | Type::Hex)
    }

    /// The radix used to read values of this type, mirroring the C tools'
    /// `strtoll()` calls. `0` means "not a number type".
    #[inline]
    pub fn base(self) -> u32 {
        match self {
            Type::Int => 10,
            Type::Hex => 16,
            _ => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Type::Unknown => "unknown",
            Type::Bool => "bool",
            Type::Tristate => "tristate",
            Type::Int => "int",
            Type::Hex => "hex",
            Type::String => "string",
        }
    }
}

/// Where a property was written. Used for diagnostics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Loc {
    pub file: StrId,
    pub line: u32,
}

/// A `default`, whose value is an expression and which applies when `cond`
/// evaluates to something other than `n`.
#[derive(Clone, Copy, Debug)]
pub struct Default {
    pub value: ExprId,
    pub cond: ExprId,
}

/// A `select` or `imply`: `target` is forced on when `cond` holds.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub target: SymbolId,
    pub cond: ExprId,
}

/// A `range low high if cond` property on an int/hex symbol.
#[derive(Clone, Copy, Debug)]
pub struct Range {
    pub low: SymbolId,
    pub high: SymbolId,
    pub cond: ExprId,
}

/// A Kconfig symbol.
///
/// Constant symbols (`n`, `m`, `y`, and every quoted string that appears in an
/// expression) live in the same arena as configurable ones; `is_const` tells
/// them apart.
pub struct Symbol {
    pub name: StrId,
    pub is_const: bool,
    /// The type as written. [`Symbol::effective_type`] promotes tristate to
    /// bool when modules are disabled.
    pub kind: Type,

    /// Menu nodes that define this symbol. A symbol may be defined in many
    /// places; each definition contributes its own properties.
    pub nodes: Vec<NodeId>,

    pub defaults: Vec<Default>,
    pub selects: Vec<Selection>,
    pub implies: Vec<Selection>,
    pub ranges: Vec<Range>,

    /// The `depends on` conditions of every definition, OR-ed together.
    pub direct_dep: ExprId,
    /// What `select`s force this symbol to, OR-ed together.
    pub rev_dep: ExprId,
    /// What `imply`s suggest for this symbol, OR-ed together.
    pub weak_rev_dep: ExprId,

    pub choice: Option<ChoiceId>,
    /// Set on the hidden symbol that stands in for a choice inside
    /// expressions; its value is the choice's mode.
    pub choice_proxy_for: Option<ChoiceId>,
    /// Zephyr `configdefault` blocks targeting this symbol, each recorded with
    /// the length of `defaults` at the time the block was finalized so the
    /// original ordering can be restored.
    pub configdefaults: Vec<(usize, NodeId)>,
    pub is_allnoconfig_y: bool,
    /// Set by `option env="VAR"`; such symbols are never written to .config.
    pub env_var: Option<StrId>,

    /// Value assigned from a .config file, if any.
    pub user_value: Option<UserValue>,
}

/// A value read from a .config file or set programmatically.
#[derive(Clone, Debug)]
pub enum UserValue {
    Tri(Tri),
    Str(String),
}

impl Symbol {
    pub fn new(name: StrId, is_const: bool, n: ExprId) -> Symbol {
        Symbol {
            name,
            is_const,
            kind: Type::Unknown,
            nodes: Vec::new(),
            defaults: Vec::new(),
            selects: Vec::new(),
            implies: Vec::new(),
            ranges: Vec::new(),
            direct_dep: n,
            rev_dep: n,
            weak_rev_dep: n,
            choice: None,
            choice_proxy_for: None,
            configdefaults: Vec::new(),
            is_allnoconfig_y: false,
            env_var: None,
            user_value: None,
        }
    }

    /// The type after the "modules are off, so tristate behaves like bool"
    /// promotion that Kconfig applies.
    #[inline]
    pub fn effective_type(&self, modules_on: bool) -> Type {
        match self.kind {
            Type::Tristate if !modules_on => Type::Bool,
            other => other,
        }
    }
}

/// A `choice` block.
pub struct Choice {
    pub name: Option<StrId>,
    pub kind: Type,
    pub nodes: Vec<NodeId>,
    pub syms: Vec<SymbolId>,
    pub defaults: Vec<Default>,
    pub direct_dep: ExprId,
    pub is_optional: bool,
    /// See [`Symbol::choice_proxy_for`].
    pub proxy_symbol: Option<SymbolId>,
    pub user_value: Option<Tri>,
    pub user_selection: Option<SymbolId>,
}

impl Choice {
    pub fn new(name: Option<StrId>, n: ExprId) -> Choice {
        Choice {
            name,
            kind: Type::Unknown,
            nodes: Vec::new(),
            syms: Vec::new(),
            defaults: Vec::new(),
            direct_dep: n,
            is_optional: false,
            proxy_symbol: None,
            user_value: None,
            user_selection: None,
        }
    }

    #[inline]
    pub fn effective_type(&self, modules_on: bool) -> Type {
        match self.kind {
            Type::Tristate if !modules_on => Type::Bool,
            other => other,
        }
    }
}

/// What a menu node holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    Symbol(SymbolId),
    Choice(ChoiceId),
    Menu,
    Comment,
    /// An `if` block. These are dissolved into their children during
    /// finalization and never survive into the finished tree.
    If,
}

/// A node in the menu tree.
///
/// The tree is a classic first-child / next-sibling linked structure, the same
/// shape the C tools and Kconfiglib use, so the finalization passes translate
/// across directly.
pub struct MenuNode {
    pub item: Item,
    pub prompt: Option<(StrId, ExprId)>,
    /// Help text, already dedented. Not interned: help texts are long and
    /// almost never repeat, so hashing them would only cost time.
    pub help: Option<Box<str>>,

    /// `depends on` for this definition, with parent dependencies folded in
    /// during finalization.
    pub dep: ExprId,
    /// `visible if` on menus.
    pub visibility: ExprId,

    pub defaults: Vec<Default>,
    pub selects: Vec<Selection>,
    pub implies: Vec<Selection>,
    pub ranges: Vec<Range>,

    pub parent: Option<NodeId>,
    pub next: Option<NodeId>,
    pub list: Option<NodeId>,

    pub is_menuconfig: bool,
    /// Zephyr's `configdefault` extension: a block whose `default`s are
    /// appended to an already-defined symbol without redefining it.
    pub is_configdefault: bool,

    pub loc: Loc,
}

impl MenuNode {
    pub fn new(item: Item, parent: Option<NodeId>, loc: Loc, y: ExprId) -> MenuNode {
        MenuNode {
            item,
            prompt: None,
            help: None,
            dep: y,
            visibility: y,
            defaults: Vec::new(),
            selects: Vec::new(),
            implies: Vec::new(),
            ranges: Vec::new(),
            parent,
            next: None,
            list: None,
            is_menuconfig: false,
            is_configdefault: false,
            loc,
        }
    }
}
