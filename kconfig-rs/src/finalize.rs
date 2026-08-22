//! Post-parse passes that turn the raw parse into a usable menu tree.
//!
//! Parsing produces a literal transcription of the files. Finalization is what
//! makes it mean something:
//!
//!  1. copy each definition's properties up onto its symbol or choice;
//!  2. push a parent's `depends on` down into every child's prompt, defaults,
//!     ranges, selects and implies;
//!  3. build the implicit submenus that Kconfig creates when a run of symbols
//!     all depend on the symbol above them;
//!  4. dissolve `if` nodes, which exist only as a way of writing a shared
//!     `depends on`;
//!  5. resolve choices — collect their symbols and infer their type.
//!
//! The passes and their order follow `menu_finalize()` in the C tools.

use rustc_hash::FxHashSet;

use crate::expr::ExprId;
use crate::model::{ChoiceId, Default as DefaultProp, Item, NodeId, SymbolId, Type};
use crate::Kconfig;

pub fn finalize(kconf: &mut Kconfig) {
    let top = kconf.top_node;
    let y = kconf.exprs.y;
    finalize_node(kconf, top, y);
    apply_configdefaults(kconf);

    kconf.unique_defined_syms = ordered_unique(&kconf.defined_syms);
    kconf.unique_choices = ordered_unique(&kconf.unique_choices);
}

fn ordered_unique<T: Copy + Eq + std::hash::Hash>(items: &[T]) -> Vec<T> {
    let mut seen = FxHashSet::default();
    items.iter().copied().filter(|x| seen.insert(*x)).collect()
}

fn finalize_node(kconf: &mut Kconfig, node: NodeId, visible_if: ExprId) {
    match kconf.node(node).item {
        Item::Symbol(_) => {
            add_props_to_sym(kconf, node);
            build_implicit_submenu(kconf, node, visible_if);
        }
        _ if kconf.node(node).list.is_some() => {
            let visible_if = if kconf.node(node).item == Item::Menu {
                let vis = kconf.node(node).visibility;
                kconf.exprs.and(visible_if, vis)
            } else {
                visible_if
            };

            // Dependencies must reach the children before they are finalized,
            // because implicit submenu creation looks at them.
            propagate_deps(kconf, node, visible_if);

            let mut cur = kconf.node(node).list;
            while let Some(id) = cur {
                finalize_node(kconf, id, visible_if);
                cur = kconf.node(id).next;
            }
        }
        _ => {}
    }

    if kconf.node(node).list.is_some() {
        flatten(kconf, node);
        remove_ifs(kconf, node);
    }

    // A choice can legitimately be empty, so this is checked outside the
    // branches above.
    if let Item::Choice(choice) = kconf.node(node).item {
        let dep = kconf.node(node).dep;
        let direct = kconf.choice(choice).direct_dep;
        kconf.choice_mut(choice).direct_dep = kconf.exprs.or(direct, dep);
        for k in 0..kconf.node(node).defaults.len() {
            let d = kconf.node(node).defaults[k];
            kconf.choice_mut(choice).defaults.push(d);
        }
        finalize_choice(kconf, node, choice);
    }
}

/// Gathers the run of following siblings that depend on `node`'s symbol into
/// an implicit submenu rooted at `node`.
fn build_implicit_submenu(kconf: &mut Kconfig, node: NodeId, visible_if: ExprId) {
    let Item::Symbol(sym) = kconf.node(node).item else {
        return;
    };

    let mut cur = node;
    while let Some(next) = kconf.node(cur).next {
        if !auto_menu_dep(kconf, sym, next) {
            break;
        }
        // Recursing here makes implicit menus nest.
        finalize_node(kconf, next, visible_if);
        cur = next;
        kconf.node_mut(cur).parent = Some(node);
    }

    if cur != node {
        let first = kconf.node(node).next;
        let after = kconf.node(cur).next;
        kconf.node_mut(node).list = first;
        kconf.node_mut(node).next = after;
        kconf.node_mut(cur).next = None;
    }
}

/// True when `candidate` should be pulled into an implicit submenu under
/// `sym`. Mirrors `menu_add_symbol()`'s check in the C tools: a prompt
/// condition is used if there is one, otherwise the plain `depends on`.
fn auto_menu_dep(kconf: &Kconfig, sym: SymbolId, candidate: NodeId) -> bool {
    let node = kconf.node(candidate);
    let expr = match node.prompt {
        Some((_, cond)) => cond,
        None => node.dep,
    };
    kconf.exprs.depends_on(expr, sym, kconf.n, kconf.m, kconf.y)
}

/// Pushes `node`'s dependencies into each of its children.
fn propagate_deps(kconf: &mut Kconfig, node: NodeId, visible_if: ExprId) {
    // Under a choice, the choice itself acts as the dependency: its mode caps
    // the visibility of the symbols inside it.
    let basedep = match kconf.node(node).item {
        Item::Choice(choice) => {
            let sym = choice_as_symbol(kconf, choice);
            kconf.exprs.sym(sym)
        }
        _ => kconf.node(node).dep,
    };

    let mut cur = kconf.node(node).list;
    while let Some(id) = cur {
        let dep = kconf.exprs.and(kconf.node(id).dep, basedep);
        kconf.node_mut(id).dep = dep;

        let is_sym_or_choice = matches!(kconf.node(id).item, Item::Symbol(_) | Item::Choice(_));
        if is_sym_or_choice {
            if let Some((text, cond)) = kconf.node(id).prompt {
                let vis_and_dep = kconf.exprs.and(visible_if, dep);
                let cond = kconf.exprs.and(cond, vis_and_dep);
                kconf.node_mut(id).prompt = Some((text, cond));
            }

            // Rewritten in place: these lists are long and this runs for every
            // node in the tree.
            for k in 0..kconf.node(id).defaults.len() {
                let cond = kconf.exprs.and(kconf.node(id).defaults[k].cond, dep);
                kconf.node_mut(id).defaults[k].cond = cond;
            }
            for k in 0..kconf.node(id).ranges.len() {
                let cond = kconf.exprs.and(kconf.node(id).ranges[k].cond, dep);
                kconf.node_mut(id).ranges[k].cond = cond;
            }
            for k in 0..kconf.node(id).selects.len() {
                let cond = kconf.exprs.and(kconf.node(id).selects[k].cond, dep);
                kconf.node_mut(id).selects[k].cond = cond;
            }
            for k in 0..kconf.node(id).implies.len() {
                let cond = kconf.exprs.and(kconf.node(id).implies[k].cond, dep);
                kconf.node_mut(id).implies[k].cond = cond;
            }
        } else if let Some((text, cond)) = kconf.node(id).prompt {
            // Menus and comments get the dependency but not `visible if`.
            let cond = kconf.exprs.and(cond, dep);
            kconf.node_mut(id).prompt = Some((text, cond));
        }

        cur = kconf.node(id).next;
    }
}

/// Copies a definition's properties onto the symbol it defines, and records
/// the reverse dependencies its `select`s and `imply`s create.
fn add_props_to_sym(kconf: &mut Kconfig, node: NodeId) {
    let Item::Symbol(sym) = kconf.node(node).item else {
        return;
    };

    // `configdefault` blocks are applied later, once every ordinary definition
    // has contributed its defaults. Record where in the list they belong.
    if kconf.node(node).is_configdefault {
        let at = kconf.sym(sym).defaults.len();
        kconf.sym_mut(sym).configdefaults.push((at, node));
        return;
    }

    let dep = kconf.node(node).dep;
    let direct = kconf.sym(sym).direct_dep;
    kconf.sym_mut(sym).direct_dep = kconf.exprs.or(direct, dep);

    // Copied by index rather than cloned: the node keeps its own copy of each
    // list, and cloning them was a measurable share of finalization.
    for k in 0..kconf.node(node).defaults.len() {
        let d = kconf.node(node).defaults[k];
        kconf.sym_mut(sym).defaults.push(d);
    }
    for k in 0..kconf.node(node).ranges.len() {
        let r = kconf.node(node).ranges[k];
        kconf.sym_mut(sym).ranges.push(r);
    }

    let self_expr = kconf.exprs.sym(sym);
    for k in 0..kconf.node(node).selects.len() {
        let sel = kconf.node(node).selects[k];
        kconf.sym_mut(sym).selects.push(sel);
        let contribution = kconf.exprs.and(self_expr, sel.cond);
        let old = kconf.sym(sel.target).rev_dep;
        kconf.sym_mut(sel.target).rev_dep = kconf.exprs.or(old, contribution);
    }
    for k in 0..kconf.node(node).implies.len() {
        let imp = kconf.node(node).implies[k];
        kconf.sym_mut(sym).implies.push(imp);
        let contribution = kconf.exprs.and(self_expr, imp.cond);
        let old = kconf.sym(imp.target).weak_rev_dep;
        kconf.sym_mut(imp.target).weak_rev_dep = kconf.exprs.or(old, contribution);
    }
}

/// Applies Zephyr's `configdefault` blocks.
///
/// Their defaults are spliced into the symbol's default list at the position
/// the block appeared, so relative order is preserved, and each is guarded by
/// the symbol's direct dependencies.
fn apply_configdefaults(kconf: &mut Kconfig) {
    for i in 0..kconf.symbols.len() {
        let sym = SymbolId(i as u32);
        if kconf.sym(sym).configdefaults.is_empty() {
            continue;
        }
        let blocks = std::mem::take(&mut kconf.sym_mut(sym).configdefaults);
        let direct_dep = kconf.sym(sym).direct_dep;
        let mut inserted = 0usize;
        for (idx, node) in blocks {
            let defaults = kconf.node(node).defaults.clone();
            for d in defaults {
                let cond = kconf.exprs.and(direct_dep, d.cond);
                let at = (inserted + idx).min(kconf.sym(sym).defaults.len());
                kconf.sym_mut(sym).defaults.insert(
                    at,
                    DefaultProp {
                        value: d.value,
                        cond,
                    },
                );
                inserted += 1;
            }
        }
    }
}

/// Hoists the children of prompt-less nodes up to sibling level, so the menu
/// structure has no surprising jumps in indentation. Promptless choices are
/// left alone: a named choice may legitimately be extended in several places.
fn flatten(kconf: &mut Kconfig, node: NodeId) {
    let mut cur = kconf.node(node).list;
    while let Some(id) = cur {
        let n = kconf.node(id);
        let hoistable =
            n.list.is_some() && n.prompt.is_none() && !matches!(n.item, Item::Choice(_));
        if hoistable {
            let parent = kconf.node(id).parent;
            let first_child = kconf.node(id).list;

            let mut last = first_child.expect("list is Some");
            loop {
                kconf.node_mut(last).parent = parent;
                match kconf.node(last).next {
                    Some(next) => last = next,
                    None => break,
                }
            }

            let after = kconf.node(id).next;
            kconf.node_mut(last).next = after;
            kconf.node_mut(id).next = first_child;
            kconf.node_mut(id).list = None;
        }
        cur = kconf.node(id).next;
    }
}

/// Drops `if` nodes, which have already been flattened and whose only job was
/// to carry a shared dependency.
fn remove_ifs(kconf: &mut Kconfig, node: NodeId) {
    let mut cur = kconf.node(node).list;
    while let Some(id) = cur {
        if kconf.node(id).item != Item::If {
            break;
        }
        cur = kconf.node(id).next;
    }
    kconf.node_mut(node).list = cur;

    while let Some(id) = cur {
        let mut next = kconf.node(id).next;
        while let Some(n) = next {
            if kconf.node(n).item != Item::If {
                break;
            }
            next = kconf.node(n).next;
        }
        kconf.node_mut(id).next = next;
        cur = next;
    }
}

/// Registers a choice's symbols and settles its type.
fn finalize_choice(kconf: &mut Kconfig, node: NodeId, choice: ChoiceId) {
    let mut cur = kconf.node(node).list;
    while let Some(id) = cur {
        if let Item::Symbol(sym) = kconf.node(id).item {
            kconf.sym_mut(sym).choice = Some(choice);
            kconf.choice_mut(choice).syms.push(sym);
        }
        cur = kconf.node(id).next;
    }

    // An untyped choice takes the type of its first typed symbol...
    if kconf.choice(choice).kind == Type::Unknown {
        for i in 0..kconf.choice(choice).syms.len() {
            let sym = kconf.choice(choice).syms[i];
            if kconf.sym(sym).kind != Type::Unknown {
                kconf.choice_mut(choice).kind = kconf.sym(sym).kind;
                break;
            }
        }
    }
    // ...and untyped symbols take the type of the choice.
    let kind = kconf.choice(choice).kind;
    for i in 0..kconf.choice(choice).syms.len() {
        let sym = kconf.choice(choice).syms[i];
        if kconf.sym(sym).kind == Type::Unknown {
            kconf.sym_mut(sym).kind = kind;
        }
    }
}

/// A choice takes part in expressions as if it were a symbol (its value is its
/// mode). We give each choice a hidden symbol so expressions can refer to it
/// uniformly.
fn choice_as_symbol(kconf: &mut Kconfig, choice: ChoiceId) -> SymbolId {
    if let Some(sym) = kconf.choice(choice).proxy_symbol {
        return sym;
    }
    let name = format!("<choice {}>", choice.0);
    let id = kconf.interner.intern(&name);
    let sym = SymbolId(kconf.symbols.len() as u32);
    let mut symbol = crate::model::Symbol::new(id, false, kconf.exprs.n);
    symbol.choice_proxy_for = Some(choice);
    kconf.symbols.push(symbol);
    kconf.choice_mut(choice).proxy_symbol = Some(sym);
    sym
}
