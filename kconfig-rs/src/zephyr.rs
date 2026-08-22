//! Zephyr's `kconfigfunctions` helpers, ported for `KCONFIG_DOC_MODE`.
//!
//! Zephyr's Kconfig files call into Python from thousands of call sites:
//! `$(dt_nodelabel_enabled,...)`, `$(dt_compat_any_has_prop,...)` and friends
//! query the devicetree that `scripts/kconfig/kconfigfunctions.py` loads from
//! a pickled `edtlib` model. Those cannot be ported without also porting the
//! devicetree layer — see `docs/INVESTIGATION.md`.
//!
//! What *can* be ported directly is everything that is a pure function of its
//! arguments, plus the constants every devicetree query degrades to when
//! `KCONFIG_DOC_MODE=1` (the mode Zephyr's own documentation build uses). That
//! is what this module implements, and it is enough to load the full
//! all-boards Zephyr tree exactly as the doc build does.

/// Evaluates `name(args)`. `args[0]` is the function name; the rest are the
/// Kconfig-level arguments. Returns `None` for names we do not provide, so the
/// caller can fall through to environment lookup.
pub fn call(name: &str, args: &[String]) -> Option<String> {
    let arg = |n: usize| -> &str { args.get(n).map(String::as_str).unwrap_or("") };

    if let Some(value) = doc_mode_devicetree(name) {
        return Some(value);
    }

    // Pure helpers.
    match name {
        "normalize_upper" => {
            return Some(
                arg(1)
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>()
                    .to_uppercase(),
            )
        }
        "shields_list_contains" => {
            let listed = std::env::var("SHIELD_AS_LIST")
                .map(|list| list.split(';').any(|s| s == arg(1)))
                .unwrap_or(false);
            return Some(if listed { "y" } else { "n" }.to_string());
        }
        "substring" => {
            let s = arg(1);
            let start: usize = arg(2).parse().ok()?;
            let stop = args.get(3).and_then(|v| v.parse::<usize>().ok());
            let end = stop.unwrap_or(s.len()).min(s.len());
            let start = start.min(end);
            return Some(s[start..end].to_string());
        }
        _ => {}
    }

    arithmetic(name, &args[1..])
}

/// `$(add,...)`, `$(inc,...)` and the rest of the integer helpers. Every
/// argument may itself be a comma-separated list, which is flattened first.
fn arithmetic(name: &str, args: &[String]) -> Option<String> {
    let (base_name, hex) = match name.strip_suffix("_hex") {
        Some(stripped) => (stripped, true),
        None => (name, false),
    };
    if !matches!(
        base_name,
        "add" | "sub" | "mul" | "div" | "mod" | "max" | "min" | "inc" | "dec"
    ) {
        return None;
    }

    let mut values = Vec::new();
    for arg in args {
        for part in arg.split(',') {
            values.push(parse_int(part.trim())?);
        }
    }
    if values.is_empty() {
        return None;
    }

    let render = |v: i128| if hex { format_hex(v) } else { v.to_string() };

    // inc/dec map over the whole sequence; the rest fold it.
    if base_name == "inc" || base_name == "dec" {
        let delta = if base_name == "inc" { 1 } else { -1 };
        return Some(
            values
                .iter()
                .map(|v| render(v + delta))
                .collect::<Vec<_>>()
                .join(","),
        );
    }

    let mut it = values.into_iter();
    let mut acc = it.next().expect("checked non-empty");
    for v in it {
        acc = match base_name {
            "add" => acc + v,
            "sub" => acc - v,
            "mul" => acc * v,
            "div" => acc.checked_div(v)?,
            "mod" => acc.checked_rem(v)?,
            "max" => acc.max(v),
            _ => acc.min(v),
        };
    }
    Some(render(acc))
}

/// Python's `int(s, base=0)`: accepts `0x`/`0o`/`0b` prefixes and plain decimal.
fn parse_int(s: &str) -> Option<i128> {
    let (sign, s) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let magnitude = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i128::from_str_radix(h, 16)
    } else if let Some(o) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        i128::from_str_radix(o, 8)
    } else if let Some(b) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        i128::from_str_radix(b, 2)
    } else {
        s.parse::<i128>()
    };
    magnitude.ok().map(|m| sign * m)
}

/// Matches Python's `hex()`, which renders negatives as `-0x...`.
fn format_hex(v: i128) -> String {
    if v < 0 {
        format!("-0x{:x}", -v)
    } else {
        format!("0x{:x}", v)
    }
}

/// The values Zephyr's Python implementation returns for every devicetree
/// query when no devicetree is loaded.
fn doc_mode_devicetree(name: &str) -> Option<String> {
    let value = match name {
        "dt_chosen_label"
        | "dt_node_ph_prop_path"
        | "dt_nodelabel_path"
        | "dt_node_parent"
        | "dt_partition_mtd" => "",

        "dt_compat_enabled_num"
        | "dt_nodelabel_int_prop"
        | "dt_node_int_prop_int"
        | "dt_node_array_prop_int"
        | "dt_node_ph_array_prop_int"
        | "dt_highest_controller_irq_number"
        | "dt_chosen_reg_addr_int"
        | "dt_chosen_reg_size_int"
        | "dt_node_reg_addr_int"
        | "dt_node_reg_size_int"
        | "dt_nodelabel_reg_addr_int"
        | "dt_nodelabel_reg_size_int"
        | "dt_chosen_partition_addr_int" => "0",

        "dt_node_int_prop_hex"
        | "dt_node_array_prop_hex"
        | "dt_node_ph_array_prop_hex"
        | "dt_chosen_reg_addr_hex"
        | "dt_chosen_reg_size_hex"
        | "dt_node_reg_addr_hex"
        | "dt_node_reg_addr_by_name_hex"
        | "dt_node_reg_size_hex"
        | "dt_nodelabel_reg_addr_hex"
        | "dt_nodelabel_reg_size_hex"
        | "dt_chosen_partition_addr_hex" => "0x0",

        // Every remaining dt_* predicate answers "n".
        _ if name.starts_with("dt_") => "n",
        _ => return None,
    };
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::call;

    fn eval(name: &str, args: &[&str]) -> Option<String> {
        let mut all = vec![name.to_string()];
        all.extend(args.iter().map(|a| a.to_string()));
        call(name, &all)
    }

    #[test]
    fn arithmetic_folds_left_to_right() {
        assert_eq!(eval("add", &["10", "3"]).as_deref(), Some("13"));
        assert_eq!(eval("add", &["10", "3", "2"]).as_deref(), Some("15"));
        assert_eq!(eval("sub", &["10", "3", "2"]).as_deref(), Some("5"));
        assert_eq!(eval("div", &["10", "3"]).as_deref(), Some("3"));
        assert_eq!(eval("mod", &["10", "3"]).as_deref(), Some("1"));
        assert_eq!(eval("max", &["10", "3"]).as_deref(), Some("10"));
        assert_eq!(eval("mul_hex", &["0x10", "2"]).as_deref(), Some("0x20"));
    }

    #[test]
    fn inc_and_dec_map_over_a_comma_separated_list() {
        assert_eq!(eval("inc", &["1"]).as_deref(), Some("2"));
        assert_eq!(eval("inc", &["1", "1"]).as_deref(), Some("2,2"));
        assert_eq!(eval("dec", &["1,1"]).as_deref(), Some("0,0"));
        // A list flows back into the folding operators as separate arguments.
        assert_eq!(eval("add", &["2,2"]).as_deref(), Some("4"));
    }

    #[test]
    fn string_helpers_match_the_python_versions() {
        assert_eq!(
            eval("normalize_upper", &["a-b.c"]).as_deref(),
            Some("A_B_C")
        );
        assert_eq!(eval("substring", &["abcdef", "2"]).as_deref(), Some("cdef"));
        assert_eq!(
            eval("substring", &["abcdef", "1", "3"]).as_deref(),
            Some("bc")
        );
    }

    #[test]
    fn devicetree_queries_answer_with_the_doc_mode_constants() {
        assert_eq!(
            eval("dt_nodelabel_enabled", &["uart0"]).as_deref(),
            Some("n")
        );
        assert_eq!(
            eval("dt_chosen_reg_addr_hex", &["zephyr,sram"]).as_deref(),
            Some("0x0")
        );
        assert_eq!(
            eval("dt_node_int_prop_int", &["/soc", "reg"]).as_deref(),
            Some("0")
        );
        assert_eq!(eval("dt_nodelabel_path", &["uart0"]).as_deref(), Some(""));
        assert_eq!(eval("not_a_zephyr_function", &[]), None);
    }
}
