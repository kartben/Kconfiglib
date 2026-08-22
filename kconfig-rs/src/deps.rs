//! The reverse dependency graph, and the dependency-loop check built on it.
//!
//! `dependents[s]` lists every item whose value might change when `s` changes.
//! A cycle in that graph is a Kconfig dependency loop: a set of symbols that
//! each need each other's value to decide their own. The C tools and
//! Kconfiglib both reject such a tree rather than pick an arbitrary fixpoint,
//! and so does this loader.

use crate::model::{Item, SymbolId};
use crate::{Error, Kconfig, Result};

/// The reverse dependency graph in compressed form: one flat edge array with
/// an index per symbol. Building it per-symbol `Vec`s cost more in allocator
/// traffic than the graph is worth.
pub struct Dependents {
    offsets: Vec<u32>,
    edges: Vec<SymbolId>,
}

impl Dependents {
    #[inline]
    fn of(&self, sym: SymbolId) -> &[SymbolId] {
        let start = self.offsets[sym.index()] as usize;
        let end = self.offsets[sym.index() + 1] as usize;
        &self.edges[start..end]
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

/// Builds the reverse dependency graph.
///
/// The result is deliberately over-approximate: an expression contributes an
/// edge from every symbol it mentions, without analyzing which of them can
/// actually affect the outcome. That is what Kconfiglib does, and a few extra
/// edges cost nothing here.
pub fn build_dependents(kconf: &Kconfig) -> Dependents {
    // Collected as (depends-on, dependent) pairs, then bucketed by the first
    // element with a counting sort.
    let mut pairs: Vec<(SymbolId, SymbolId)> = Vec::with_capacity(kconf.symbols.len() * 8);

    let record = |pairs: &mut Vec<(SymbolId, SymbolId)>, item: SymbolId, expr| {
        kconf.exprs.for_each_symbol(expr, &mut |sym| {
            // Constant symbols never change, so they are not real dependencies.
            if !kconf.sym(sym).is_const {
                pairs.push((sym, item));
            }
        });
    };

    for &sym in &kconf.unique_defined_syms {
        for &node in &kconf.sym(sym).nodes {
            if let Some((_, cond)) = kconf.node(node).prompt {
                record(&mut pairs, sym, cond);
            }
        }
        for d in &kconf.sym(sym).defaults {
            record(&mut pairs, sym, d.value);
            record(&mut pairs, sym, d.cond);
        }
        record(&mut pairs, sym, kconf.sym(sym).rev_dep);
        record(&mut pairs, sym, kconf.sym(sym).weak_rev_dep);
        for r in &kconf.sym(sym).ranges {
            for bound in [r.low, r.high] {
                if !kconf.sym(bound).is_const {
                    pairs.push((bound, sym));
                }
            }
            record(&mut pairs, sym, r.cond);
        }
        // Usually redundant with the propagated properties, but `imply` only
        // consults the direct dependencies, so the edge has to be here too.
        record(&mut pairs, sym, kconf.sym(sym).direct_dep);
    }

    for &choice in &kconf.unique_choices {
        let Some(proxy) = kconf.choice(choice).proxy_symbol else {
            continue;
        };
        for &node in &kconf.choice(choice).nodes {
            if let Some((_, cond)) = kconf.node(node).prompt {
                record(&mut pairs, proxy, cond);
            }
        }
        for d in &kconf.choice(choice).defaults {
            record(&mut pairs, proxy, d.cond);
        }
    }

    let mut offsets = vec![0u32; kconf.symbols.len() + 1];
    for &(from, _) in &pairs {
        offsets[from.index() + 1] += 1;
    }
    for i in 1..offsets.len() {
        offsets[i] += offsets[i - 1];
    }

    let mut edges = vec![SymbolId(0); pairs.len()];
    let mut cursor = offsets.clone();
    for &(from, to) in &pairs {
        edges[cursor[from.index()] as usize] = to;
        cursor[from.index()] += 1;
    }
    Dependents { offsets, edges }
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Unvisited,
    OnStack,
    Clear,
}

/// Returns an error naming the cycle if the tree contains a dependency loop.
pub fn check_dependency_loops(kconf: &Kconfig, dependents: &Dependents) -> Result<()> {
    let mut marks = vec![Mark::Unvisited; kconf.symbols.len()];
    for &sym in &kconf.unique_defined_syms {
        if let Some(loop_) = visit(kconf, dependents, &mut marks, sym, false) {
            return Err(describe(kconf, &loop_));
        }
    }
    Ok(())
}

/// Depth-first search. `OnStack` means "reachable from here, still being
/// explored"; running into it again closes a cycle, which is then collected on
/// the way back up.
///
/// Choices need care: every symbol in a choice constrains every other, so
/// entering a choice from one of its symbols must not immediately bounce back
/// through the same symbol. `came_from_choice` suppresses that.
fn visit(
    kconf: &Kconfig,
    dependents: &Dependents,
    marks: &mut Vec<Mark>,
    sym: SymbolId,
    came_from_choice: bool,
) -> Option<Vec<SymbolId>> {
    match marks[sym.index()] {
        Mark::Clear => return None,
        Mark::OnStack => return Some(vec![sym]),
        Mark::Unvisited => {}
    }
    marks[sym.index()] = Mark::OnStack;

    for &next in dependents.of(sym) {
        let found = match kconf.sym(next).choice_proxy_for {
            Some(choice) => visit_choice(kconf, dependents, marks, choice, None),
            None => visit(kconf, dependents, marks, next, false),
        };
        if let Some(loop_) = found {
            return Some(close(loop_, sym));
        }
    }

    if let Some(choice) = kconf.sym(sym).choice {
        if !came_from_choice {
            if let Some(loop_) = visit_choice(kconf, dependents, marks, choice, Some(sym)) {
                return Some(close(loop_, sym));
            }
        }
    }

    marks[sym.index()] = Mark::Clear;
    None
}

fn visit_choice(
    kconf: &Kconfig,
    dependents: &Dependents,
    marks: &mut Vec<Mark>,
    choice: crate::model::ChoiceId,
    skip: Option<SymbolId>,
) -> Option<Vec<SymbolId>> {
    let proxy = kconf.choice(choice).proxy_symbol?;
    match marks[proxy.index()] {
        Mark::Clear => return None,
        Mark::OnStack => return Some(vec![proxy]),
        Mark::Unvisited => {}
    }
    marks[proxy.index()] = Mark::OnStack;

    for i in 0..kconf.choice(choice).syms.len() {
        let sym = kconf.choice(choice).syms[i];
        if Some(sym) == skip {
            continue;
        }
        // `true` stops the symbol from re-entering this same choice.
        if let Some(loop_) = visit(kconf, dependents, marks, sym, true) {
            return Some(close(loop_, proxy));
        }
    }

    marks[proxy.index()] = Mark::Clear;
    None
}

/// Extends a partially collected loop, stopping once we are back where it
/// started.
fn close(mut loop_: Vec<SymbolId>, current: SymbolId) -> Vec<SymbolId> {
    if loop_.first() != Some(&current) {
        loop_.push(current);
    }
    loop_
}

fn describe(kconf: &Kconfig, loop_: &[SymbolId]) -> Error {
    let mut msg = String::from("\nDependency loop\n===============\n\n");
    for (i, &sym) in loop_.iter().enumerate() {
        if i > 0 {
            msg.push_str("...depends on ");
        }
        msg.push_str(&item_name(kconf, sym));
        msg.push('\n');
    }
    Error(msg)
}

fn item_name(kconf: &Kconfig, sym: SymbolId) -> String {
    if let Some(choice) = kconf.sym(sym).choice_proxy_for {
        return match kconf.choice(choice).name {
            Some(name) => format!("<choice {}>", &kconf.interner[name]),
            None => "<choice>".to_string(),
        };
    }
    let locations: Vec<String> = kconf
        .sym(sym)
        .nodes
        .iter()
        .filter(|&&node| matches!(kconf.node(node).item, Item::Symbol(_)))
        .map(|&node| {
            let loc = kconf.node(node).loc;
            format!("{}:{}", &kconf.interner[loc.file], loc.line)
        })
        .collect();
    if locations.is_empty() {
        format!("{} (undefined)", kconf.name(sym))
    } else {
        format!("{} (defined at {})", kconf.name(sym), locations.join(", "))
    }
}
