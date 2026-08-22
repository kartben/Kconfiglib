//! End-to-end tests: write a small Kconfig tree, load it, check the .config.

use std::path::{Path, PathBuf};

use kconfig::eval::Values;
use kconfig::{Kconfig, LoadOptions};

/// Writes `files` into a fresh directory and loads the first one.
fn load(files: &[(&str, &str)]) -> Result<(Kconfig, String), String> {
    let dir = scratch_dir();
    for (name, contents) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create test directory");
        }
        std::fs::write(&path, contents).expect("write test file");
    }

    // $srctree is process-wide state, so the tests that touch it run serially.
    let _guard = SRCTREE.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("srctree", &dir);
    let result = Kconfig::load(LoadOptions {
        top_file: files[0].0.to_string(),
        shell_cache: None,
        probe_jobs: 1,
        warn: true,
    });
    std::env::remove_var("srctree");
    drop(_guard);

    let kconf = result.map_err(|e| e.0)?;
    let mut values = Values::new(&kconf);
    let config = kconfig::write::config_contents(&kconf, &mut values, "");
    let _ = std::fs::remove_dir_all(&dir);
    Ok((kconf, config))
}

static SRCTREE: std::sync::Mutex<()> = std::sync::Mutex::new(());
static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn scratch_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("kconfig-rs-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

fn config_of(files: &[(&str, &str)]) -> String {
    match load(files) {
        Ok((_, config)) => config,
        Err(e) => panic!("tree should load: {e}"),
    }
}

fn load_error(files: &[(&str, &str)]) -> String {
    match load(files) {
        Ok(_) => panic!("tree should have been rejected"),
        Err(e) => e,
    }
}

#[test]
fn bool_defaults_and_dependencies() {
    let config = config_of(&[(
        "Kconfig",
        r#"
config A
	bool "a"
	default y

config B
	bool "b"
	depends on A
	default y

config C
	bool "c"
	depends on !A
	default y
"#,
    )]);
    // C is invisible and has no value to record, so it is left out entirely —
    // the same thing Kconfiglib and the C tools do.
    assert_eq!(config, "CONFIG_A=y\nCONFIG_B=y\n");
}

#[test]
fn select_forces_a_symbol_on_regardless_of_visibility() {
    let config = config_of(&[(
        "Kconfig",
        r#"
config HIDDEN
	bool

config VISIBLE
	bool "visible"
	default y
	select HIDDEN
"#,
    )]);
    assert!(config.contains("CONFIG_HIDDEN=y\n"), "{config}");
}

#[test]
fn imply_is_ignored_when_direct_dependencies_are_unmet() {
    let config = config_of(&[(
        "Kconfig",
        r#"
config GATE
	bool

config TARGET
	bool "target"
	depends on GATE

config SOURCE
	bool "source"
	default y
	imply TARGET
"#,
    )]);
    assert_eq!(config, "CONFIG_SOURCE=y\n");
}

#[test]
fn a_choice_picks_its_first_visible_symbol() {
    let config = config_of(&[(
        "Kconfig",
        r#"
choice
	prompt "pick one"

config FIRST
	bool "first"

config SECOND
	bool "second"

endchoice
"#,
    )]);
    assert!(config.contains("CONFIG_FIRST=y\n"), "{config}");
    assert!(config.contains("# CONFIG_SECOND is not set\n"), "{config}");
}

#[test]
fn int_and_hex_values_are_clamped_to_the_active_range() {
    let config = config_of(&[(
        "Kconfig",
        r#"
config LOW
	hex "low"
	range 0x10 0x20
	default 0x9

config HIGH
	int "high"
	range 10 20
	default 21

config INSIDE
	int "inside"
	range 10 20
	default 15
"#,
    )]);
    assert!(config.contains("CONFIG_LOW=0x10\n"), "{config}");
    assert!(config.contains("CONFIG_HIGH=20\n"), "{config}");
    assert!(config.contains("CONFIG_INSIDE=15\n"), "{config}");
}

#[test]
fn string_values_are_escaped() {
    let config = config_of(&[(
        "Kconfig",
        r#"
config TEXT
	string "text"
	default "a \"quoted\" value"
"#,
    )]);
    assert!(
        config.contains(r#"CONFIG_TEXT="a \"quoted\" value""#),
        "{config}"
    );
}

#[test]
fn menus_bracket_the_symbols_they_contain() {
    let config = config_of(&[(
        "Kconfig",
        r#"
menu "Outer"

config INNER
	bool "inner"
	default y

endmenu
"#,
    )]);
    assert!(config.contains("#\n# Outer\n#\n"), "{config}");
    assert!(config.contains("# end of Outer\n"), "{config}");
}

#[test]
fn source_pulls_in_other_files() {
    let config = config_of(&[
        ("Kconfig", "source \"sub/Kconfig\"\n"),
        (
            "sub/Kconfig",
            "config FROM_SUB\n\tbool \"sub\"\n\tdefault y\n",
        ),
    ]);
    assert!(config.contains("CONFIG_FROM_SUB=y\n"), "{config}");
}

#[test]
fn a_missing_obligatory_source_is_an_error() {
    let error = load_error(&[("Kconfig", "source \"nope/Kconfig\"\n")]);
    assert!(error.contains("not found"), "{error}");
}

#[test]
fn a_missing_optional_source_is_fine() {
    let config = config_of(&[(
        "Kconfig",
        "osource \"nope/Kconfig\"\nconfig A\n\tbool \"a\"\n\tdefault y\n",
    )]);
    assert!(config.contains("CONFIG_A=y\n"), "{config}");
}

#[test]
fn preprocessor_variables_and_functions_expand() {
    let config = config_of(&[(
        "Kconfig",
        r#"
NAME := hello
GREETING = $(NAME) world

config TEXT
	string "text"
	default "$(GREETING)"
"#,
    )]);
    assert!(config.contains(r#"CONFIG_TEXT="hello world""#), "{config}");
}

#[test]
fn a_dependency_loop_is_rejected() {
    let error = load_error(&[("Kconfig", "config FOO\n\tbool\n\tselect FOO\n")]);
    assert!(error.contains("Dependency loop"), "{error}");
}

#[test]
fn zephyr_configdefault_splices_defaults_in_where_the_block_appeared() {
    // Before the definition, the added default wins...
    let config = config_of(&[(
        "Kconfig",
        r#"
configdefault LEVEL
	default 7

config LEVEL
	int "level"
	default 1
"#,
    )]);
    assert_eq!(config, "CONFIG_LEVEL=7\n");

    // ...and after it, the original one still does.
    let config = config_of(&[(
        "Kconfig",
        r#"
config LEVEL
	int "level"
	default 1

configdefault LEVEL
	default 7
"#,
    )]);
    assert_eq!(config, "CONFIG_LEVEL=1\n");
}

#[test]
fn if_blocks_apply_their_condition_to_every_child() {
    let config = config_of(&[(
        "Kconfig",
        r#"
config GATE
	bool "gate"

if GATE

config GUARDED
	bool "guarded"
	default y

endif
"#,
    )]);
    assert_eq!(config, "# CONFIG_GATE is not set\n");
}

#[test]
fn every_fixture_in_the_python_test_suite_loads_or_is_rejected_as_expected() {
    // The Kconfiglib fixtures live next to this crate. These are the ones that
    // must be rejected: dependency loops, recursive `source`, missing files,
    // and two that need environment the Python test suite sets up.
    // `bench/parity.py` checks all of them against Kconfiglib itself, including
    // that the accepted ones produce identical .config output.
    const REJECTED: &[&str] = &[
        "Kdeploop0",
        "Kdeploop1",
        "Kdeploop2",
        "Kdeploop3",
        "Kdeploop4",
        "Kdeploop5",
        "Kdeploop6",
        "Kdeploop7",
        "Kdeploop8",
        "Kdeploop9",
        "Kdeploop10",
        "Kdeploop11",
        "Kdeploop12",
        "Klocation",
        "Kmissingrsource",
        "Kmissingsource",
        "Kpreprocess",
        "Krecursive1",
        "Krecursive2",
    ];

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests");
    let Ok(entries) = std::fs::read_dir(&fixtures) else {
        return;
    };

    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('K'))
        .collect();
    names.sort();
    assert!(names.len() > 40, "expected to find the fixture files");

    let _guard = SRCTREE.lock().unwrap_or_else(|e| e.into_inner());
    for name in names {
        std::env::set_var("srctree", &fixtures);
        let result = Kconfig::load(LoadOptions {
            top_file: name.clone(),
            shell_cache: None,
            probe_jobs: 1,
            warn: false,
        });
        assert_eq!(
            result.is_err(),
            REJECTED.contains(&name.as_str()),
            "{name}: {:?}",
            result.err().map(|e| e.0)
        );
    }
    std::env::remove_var("srctree");
}
