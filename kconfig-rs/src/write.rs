//! `.config` output.
//!
//! The file is produced by walking the menu tree in display order, so symbols
//! appear grouped under the menus they belong to, exactly as the C tools and
//! Kconfiglib emit them.

use crate::eval::Values;
use crate::model::{Item, SymbolId, Type};
use crate::Kconfig;

/// Renders the whole `.config`, starting with `header`.
pub fn config_contents(kconf: &Kconfig, values: &mut Values, header: &str) -> String {
    let mut out = String::with_capacity(1 << 20);
    out.push_str(header);

    let mut visited = vec![false; kconf.symbols.len()];
    let mut after_end_comment = false;
    let mut node = kconf.top_node;

    loop {
        // Walk to the next node: first child, else next sibling, else up.
        if let Some(child) = kconf.node(node).list {
            node = child;
        } else if let Some(next) = kconf.node(node).next {
            node = next;
        } else {
            let mut climbed = false;
            while let Some(parent) = kconf.node(node).parent {
                node = parent;
                if kconf.node(node).item == Item::Menu
                    && node != kconf.top_node
                    && values.node_is_visible(kconf, node)
                {
                    let (title, _) = kconf.node(node).prompt.expect("menus have prompts");
                    out.push_str("# end of ");
                    out.push_str(&kconf.interner[title]);
                    out.push('\n');
                    after_end_comment = true;
                }
                if let Some(next) = kconf.node(node).next {
                    node = next;
                    climbed = true;
                    break;
                }
            }
            if !climbed {
                return out;
            }
        }

        match kconf.node(node).item {
            Item::Symbol(sym) => {
                if visited[sym.index()] {
                    continue;
                }
                visited[sym.index()] = true;

                let Some(line) = config_string(kconf, values, sym) else {
                    continue;
                };
                if after_end_comment {
                    // A blank line separates the first symbol after a menu.
                    after_end_comment = false;
                    out.push('\n');
                }
                out.push_str(&line);
            }
            item @ (Item::Menu | Item::Comment) => {
                let show = match item {
                    Item::Menu => values.node_is_visible(kconf, node),
                    _ => values.expr(kconf, kconf.node(node).dep) != crate::model::Tri::N,
                };
                if show {
                    let (title, _) = kconf
                        .node(node)
                        .prompt
                        .expect("menus and comments have prompts");
                    out.push_str("\n#\n# ");
                    out.push_str(&kconf.interner[title]);
                    out.push_str("\n#\n");
                    after_end_comment = false;
                }
            }
            _ => {}
        }
    }
}

/// The `.config` line for one symbol, or `None` if it is not written out.
pub fn config_string(kconf: &Kconfig, values: &mut Values, sym: SymbolId) -> Option<String> {
    let value = values.str_of(kconf, sym).to_string();
    if !values.is_written(kconf, sym) {
        return None;
    }
    let prefix = &kconf.config_prefix;
    let name = kconf.name(sym);

    Some(match kconf.sym(sym).kind {
        kind if kind.is_bool_tristate() => {
            if value == "n" {
                format!("# {prefix}{name} is not set\n")
            } else {
                format!("{prefix}{name}={value}\n")
            }
        }
        Type::Int | Type::Hex => format!("{prefix}{name}={value}\n"),
        _ => format!("{prefix}{name}=\"{}\"\n", escape(&value)),
    })
}

/// Escapes `\` and `"` for a quoted .config value.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
