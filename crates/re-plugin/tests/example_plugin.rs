//! Runs the real example plugin (plugins/examples/sort-lines) through the
//! host. Build it first:
//!
//! ```sh
//! cd plugins/examples/sort-lines
//! cargo build --release --target wasm32-unknown-unknown
//! cargo test -p re-plugin --test example_plugin -- --ignored
//! ```

use std::path::PathBuf;

use re_plugin::{Outcome, Registry, Request};

#[test]
#[ignore = "needs the example plugin built for wasm32 (see file header)"]
fn sort_lines_example_runs_in_the_sandbox() {
    let example =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/sort-lines");
    let wasm = example.join("target/wasm32-unknown-unknown/release/sort_lines.wasm");
    assert!(
        wasm.is_file(),
        "build the example first: {}",
        wasm.display()
    );
    let root = std::env::temp_dir().join(format!("re-plugin-example-{}", std::process::id()));
    let dir = root.join("sort-lines");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(example.join("plugin.json"), dir.join("plugin.json")).unwrap();
    std::fs::copy(&wasm, dir.join("sort_lines.wasm")).unwrap();

    let registry = Registry::load(&root);
    assert!(registry.errors.is_empty(), "{:?}", registry.errors);
    let request = Request {
        inputs: vec!["pear\r\napple\r\nfig\r\n".into(), "b\na\nb".into()],
        path: None,
    };
    let (plugin, sort) = registry.find("plugin.sort-lines.sort").unwrap();
    assert_eq!(
        re_plugin::run(&plugin, &sort, &request).unwrap(),
        Outcome::Replace {
            outputs: vec!["apple\r\nfig\r\npear\r\n".into(), "a\nb\nb".into()],
            status: Some("sort: 2 selection(s)".into())
        }
    );
    let (_, unique) = registry.find("plugin.sort-lines.unique").unwrap();
    let Outcome::Replace { outputs, .. } = re_plugin::run(&plugin, &unique, &request).unwrap()
    else {
        panic!("replace");
    };
    assert_eq!(outputs[1], "b\na");
    let (_, stats) = registry.find("plugin.sort-lines.stats").unwrap();
    let Outcome::Show { text, .. } = re_plugin::run(&plugin, &stats, &request).unwrap() else {
        panic!("show");
    };
    assert!(text.contains("Selection 1: 3 lines, 3 words"), "{text}");
    std::fs::remove_dir_all(root).unwrap();
}
