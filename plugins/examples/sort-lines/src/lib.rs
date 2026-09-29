//! Example RustEditor plugin (sandboxed WebAssembly).
//!
//! Build: `cargo build --release --target wasm32-unknown-unknown`, then copy
//! `target/wasm32-unknown-unknown/release/sort_lines.wasm` next to
//! `plugin.json` in `<config>/plugins/sort-lines/`. See `docs/plugins.md`.
//!
//! The ABI is three exports: `memory`, `re_alloc` and `re_transform`. The
//! host writes a JSON request into memory from `re_alloc` and calls
//! `re_transform(ptr, len)`, which returns `response_ptr << 32 | len`.

use serde_json::{Value, json};

#[unsafe(no_mangle)]
pub extern "C" fn re_alloc(len: i32) -> i32 {
    let mut buf = Vec::<u8>::with_capacity(len.max(0) as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf); // the host fills it; freed with the instance
    ptr as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn re_transform(ptr: i32, len: i32) -> i64 {
    // SAFETY: the host wrote `len` bytes at `ptr` (memory from `re_alloc`).
    let request = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let response = match handle(request) {
        Ok(value) => value,
        Err(message) => json!({ "error": message }),
    };
    let bytes = response.to_string().into_bytes().into_boxed_slice();
    let out_len = bytes.len() as i64;
    let out_ptr = Box::into_raw(bytes) as *mut u8 as i64;
    (out_ptr << 32) | out_len
}

fn handle(request: &[u8]) -> Result<Value, String> {
    let request: Value = serde_json::from_slice(request).map_err(|e| e.to_string())?;
    let command = request["command"].as_str().ok_or("no command")?;
    let inputs: Vec<&str> = request["inputs"]
        .as_array()
        .ok_or("no inputs")?
        .iter()
        .map(|v| v.as_str().unwrap_or_default())
        .collect();
    match command {
        "sort" | "sort-reverse" | "unique" => {
            let outputs: Vec<String> = inputs
                .iter()
                .map(|text| transform_lines(text, command))
                .collect();
            Ok(
                json!({ "outputs": outputs, "status": format!("{command}: {} selection(s)", outputs.len()) }),
            )
        }
        "stats" => {
            let mut report = String::new();
            for (i, text) in inputs.iter().enumerate() {
                let lines = text.lines().count();
                let words = text.split_whitespace().count();
                let chars = text.chars().count();
                report.push_str(&format!(
                    "Selection {}: {lines} lines, {words} words, {chars} characters\n",
                    i + 1
                ));
            }
            Ok(json!({ "show": report }))
        }
        other => Err(format!("unknown command `{other}`")),
    }
}

/// Sorts or de-duplicates lines, keeping the selection's line ending style
/// and whether it ended with a line break.
fn transform_lines(text: &str, command: &str) -> String {
    let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing = text.ends_with('\n');
    let mut lines: Vec<&str> = text.lines().collect();
    match command {
        "sort" => lines.sort_unstable(),
        "sort-reverse" => lines.sort_unstable_by(|a, b| b.cmp(a)),
        _ => {
            let mut seen = std::collections::HashSet::new();
            lines.retain(|line| seen.insert(*line));
        }
    }
    let mut out = lines.join(ending);
    if trailing {
        out.push_str(ending);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_and_dedupes_keeping_line_endings() {
        assert_eq!(transform_lines("b\r\na\r\n", "sort"), "a\r\nb\r\n");
        assert_eq!(transform_lines("b\na", "sort-reverse"), "b\na");
        assert_eq!(transform_lines("x\ny\nx\n", "unique"), "x\ny\n");
    }
}
