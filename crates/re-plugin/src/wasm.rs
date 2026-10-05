//! Sandboxed WebAssembly host (wasmi interpreter).
//!
//! ABI (see `docs/plugins.md`): the module exports `memory`,
//! `re_alloc(len: i32) -> i32` and `re_transform(ptr: i32, len: i32) -> i64`
//! (response `ptr << 32 | len`). The only import allowed is
//! `env.log(ptr: i32, len: i32)`. Execution is limited by fuel (CPU) and a
//! memory cap, so a buggy or hostile plugin cannot hang or exhaust the
//! editor; it has no access to files, the network, the clock or processes.

use std::path::Path;

use anyhow::{Context, Result, bail};
use wasmi::{
    Caller, Config, Engine, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    TrapCode,
};

use crate::MAX_MESSAGE_BYTES;

/// Instruction budget per call: about 1-2 s of interpretation in a release
/// build (roughly 10x longer in a debug build).
pub const FUEL: u64 = 2_000_000_000;
/// Linear-memory cap per call.
pub const MEMORY_BYTES: usize = 256 * 1024 * 1024;
const MAX_MODULE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LOG_BYTES: usize = 16 * 1024;

struct HostState {
    limits: StoreLimits,
    log: String,
}

/// Loads `module` and runs one request. Returns the raw response bytes.
pub fn run(module: &Path, request: &[u8]) -> Result<Vec<u8>> {
    let size = std::fs::metadata(module)
        .with_context(|| format!("cannot read {}", module.display()))?
        .len();
    if size > MAX_MODULE_BYTES {
        bail!("module is larger than {} MiB", MAX_MODULE_BYTES >> 20);
    }
    let bytes =
        std::fs::read(module).with_context(|| format!("cannot read {}", module.display()))?;
    run_bytes(&bytes, request).map(|(response, _log)| response)
}

/// Runs one request against module bytes. Returns the response and log.
pub fn run_bytes(wasm: &[u8], request: &[u8]) -> Result<(Vec<u8>, String)> {
    run_with_fuel(wasm, request, FUEL)
}

fn run_with_fuel(wasm: &[u8], request: &[u8], fuel: u64) -> Result<(Vec<u8>, String)> {
    if request.len() > MAX_MESSAGE_BYTES {
        bail!("input is larger than {} MiB", MAX_MESSAGE_BYTES >> 20);
    }
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).context("not a valid WebAssembly module")?;
    for import in module.imports() {
        if (import.module(), import.name()) != ("env", "log") {
            bail!(
                "the plugin imports `{}.{}`, but sandboxed plugins may only import `env.log` \
                 (no files, network or system access)",
                import.module(),
                import.name()
            );
        }
    }
    let mut store = Store::new(
        &engine,
        HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_BYTES)
                .memories(1)
                .tables(4)
                .instances(1)
                .build(),
            log: String::new(),
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(fuel)?;

    let mut linker = <Linker<HostState>>::new(&engine);
    linker.func_wrap(
        "env",
        "log",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
            let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
                return;
            };
            let len = (len.max(0) as usize).min(MAX_LOG_BYTES);
            let mut buf = vec![0u8; len];
            if memory.read(&caller, ptr as u32 as usize, &mut buf).is_ok() {
                let state = caller.data_mut();
                if state.log.len() < MAX_LOG_BYTES {
                    state.log.push_str(&String::from_utf8_lossy(&buf));
                    state.log.push('\n');
                }
            }
        },
    )?;
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .map_err(explain)?;
    let memory = instance
        .get_memory(&store, "memory")
        .context("the plugin does not export `memory`")?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&store, "re_alloc")
        .context("the plugin does not export `re_alloc(i32) -> i32`")?;
    let transform = instance
        .get_typed_func::<(i32, i32), i64>(&store, "re_transform")
        .context("the plugin does not export `re_transform(i32, i32) -> i64`")?;

    let len = i32::try_from(request.len()).context("input too large")?;
    let ptr = alloc.call(&mut store, len).map_err(explain)?;
    memory
        .write(&mut store, ptr as u32 as usize, request)
        .context("re_alloc returned memory the request does not fit in")?;
    let packed = transform.call(&mut store, (ptr, len)).map_err(explain)? as u64;
    let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
    if out_len > MAX_MESSAGE_BYTES {
        bail!("response is larger than {} MiB", MAX_MESSAGE_BYTES >> 20);
    }
    let mut response = vec![0u8; out_len];
    memory
        .read(&store, out_ptr, &mut response)
        .context("the plugin returned a response outside its memory")?;
    Ok((response, std::mem::take(&mut store.data_mut().log)))
}

/// Turns traps into messages a user can act on.
fn explain(error: wasmi::Error) -> anyhow::Error {
    match error.as_trap_code() {
        Some(TrapCode::OutOfFuel) => {
            anyhow::anyhow!("the plugin ran too long and was stopped (CPU limit)")
        }
        Some(TrapCode::GrowthOperationLimited) => {
            anyhow::anyhow!(
                "the plugin needed more than {} MiB of memory",
                MEMORY_BYTES >> 20
            )
        }
        Some(code) => anyhow::anyhow!("the plugin crashed ({code})"),
        None => anyhow::anyhow!("the plugin failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module whose transform returns `response` and logs "hello".
    fn fixed(response: &str) -> Vec<u8> {
        let escaped: String = response.bytes().map(|b| format!("\\{b:02x}")).collect();
        wat::parse_str(format!(
            r#"(module
                (import "env" "log" (func $log (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "{escaped}")
                (data (i32.const 64) "hello")
                (func (export "re_alloc") (param i32) (result i32) i32.const 4096)
                (func (export "re_transform") (param i32 i32) (result i64)
                    (call $log (i32.const 64) (i32.const 5))
                    (i64.or (i64.shl (i64.const 1024) (i64.const 32))
                            (i64.const {len}))))"#,
            len = response.len()
        ))
        .unwrap()
    }

    #[test]
    fn runs_a_module_and_collects_its_log() {
        let (response, log) = run_bytes(&fixed(r#"{"outputs":["X"]}"#), b"{}").unwrap();
        assert_eq!(response, br#"{"outputs":["X"]}"#);
        assert_eq!(log, "hello\n");
    }

    #[test]
    fn echoes_the_request_through_memory() {
        // re_transform returns its input: checks alloc/write/read round trip.
        let wasm = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "re_alloc") (param i32) (result i32) i32.const 100)
                (func (export "re_transform") (param $p i32) (param $l i32) (result i64)
                    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
                            (i64.extend_i32_u (local.get $l)))))"#,
        )
        .unwrap();
        let request = "{\"inputs\":[\"héllo 😀\"]}".as_bytes();
        assert_eq!(run_bytes(&wasm, request).unwrap().0, request);
    }

    #[test]
    fn infinite_loops_are_stopped_by_the_cpu_limit() {
        let wasm = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "re_alloc") (param i32) (result i32) i32.const 0)
                (func (export "re_transform") (param i32 i32) (result i64)
                    (loop $l (br $l)) (i64.const 0)))"#,
        )
        .unwrap();
        // A small budget keeps the test fast; the mechanism is the same.
        let error = run_with_fuel(&wasm, b"{}", 5_000_000)
            .unwrap_err()
            .to_string();
        assert!(error.contains("CPU limit"), "{error}");
    }

    #[test]
    fn memory_growth_beyond_the_cap_is_refused() {
        // Tries to grow to 4 GiB; grow returns -1 and the plugin traps.
        let wasm = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "re_alloc") (param i32) (result i32) i32.const 0)
                (func (export "re_transform") (param i32 i32) (result i64)
                    (if (i32.eq (memory.grow (i32.const 65000)) (i32.const -1))
                        (then unreachable))
                    (i64.const 0)))"#,
        )
        .unwrap();
        let error = run_bytes(&wasm, b"{}").unwrap_err().to_string();
        assert!(
            error.contains("crashed") || error.contains("memory"),
            "{error}"
        );
    }

    #[test]
    fn system_imports_are_rejected_before_running() {
        let wasm = wat::parse_str(
            r#"(module
                (import "wasi_snapshot_preview1" "fd_write"
                    (func (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1))"#,
        )
        .unwrap();
        let error = run_bytes(&wasm, b"{}").unwrap_err().to_string();
        assert!(error.contains("may only import `env.log`"), "{error}");
    }

    #[test]
    fn contributed_tools_are_called_with_their_own_request() {
        let dir = std::env::temp_dir().join(format!("re-plugin-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.wasm"), fixed(r#"{"result":{"words":3}}"#)).unwrap();
        let plugin = crate::Plugin {
            manifest: crate::Manifest::parse(
                r#"{"api":0,"id":"wc","name":"WC","version":"1","kind":"wasm","module":"m.wasm",
                    "commands":[{"id":"c","title":"C"}],
                    "tools":[{"name":"word_count","description":"d","input_schema":{"type":"object"}}]}"#,
            )
            .unwrap(),
            dir: dir.clone(),
        };
        let result =
            crate::call_tool(&plugin, "word_count", &serde_json::json!({"text": "a b c"})).unwrap();
        assert_eq!(result, serde_json::json!({"words": 3}));
        assert!(crate::call_tool(&plugin, "other", &serde_json::json!({})).is_err());
        std::fs::write(dir.join("m.wasm"), fixed(r#"{"error":"no"}"#)).unwrap();
        assert_eq!(
            crate::call_tool(&plugin, "word_count", &serde_json::json!({}))
                .unwrap_err()
                .to_string(),
            "no"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_modules_and_out_of_bounds_responses_are_errors() {
        assert!(run_bytes(b"not wasm", b"{}").is_err());
        let missing = wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap();
        assert!(
            run_bytes(&missing, b"{}")
                .unwrap_err()
                .to_string()
                .contains("re_alloc")
        );
        let oob = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "re_alloc") (param i32) (result i32) i32.const 0)
                (func (export "re_transform") (param i32 i32) (result i64)
                    (i64.or (i64.shl (i64.const 60000) (i64.const 32)) (i64.const 10000))))"#,
        )
        .unwrap();
        assert!(
            run_bytes(&oob, b"{}")
                .unwrap_err()
                .to_string()
                .contains("outside")
        );
    }
}
