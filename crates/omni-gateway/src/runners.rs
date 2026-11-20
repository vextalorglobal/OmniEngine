use mlua::{
    ChunkMode, Error as LuaError, HookTriggers, Lua, LuaOptions, StdLib, Value as LuaValue, VmState,
};
use omni_core::{CPU_LIMIT_MESSAGE, DEFAULT_STEP_LIMIT, Engine, ErrorKind, MEMORY_LIMIT_MESSAGE};
use serde::Serialize;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wasmi::core::{LimiterError, ResourceLimiter};
use wasmi::{
    Config as WasmConfig, Engine as WasmEngine, Linker as WasmLinker, Module as WasmModule,
    StackLimits as WasmStackLimits, Store as WasmStore, StoreLimits, StoreLimitsBuilder,
};

const LUA_CPU_LIMIT: &str = "__OMNI_CPU_LIMIT__";
const LUA_GAS_LIMIT: &str = "__OMNI_GAS_LIMIT__";

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RunnerCapabilities {
    pub filesystem_reads: bool,
    pub filesystem_writes: bool,
    pub module_imports: bool,
    pub host_functions: bool,
    pub network_access: bool,
    pub gas_limit: bool,
    pub cpu_limit: bool,
    pub memory_limit: bool,
    pub unicode_text: bool,
    pub deterministic_verification: bool,
    pub cluster_dispatch: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct LanguageDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub runtime: &'static str,
    pub available: bool,
    pub standard_libraries: &'static [&'static str],
    pub capabilities: RunnerCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutionLimits {
    pub gas_limit: u64,
    pub cpu_limit: Duration,
    pub memory_limit_bytes: usize,
    pub max_output_bytes: usize,
}

#[derive(Debug)]
pub struct RunnerValue {
    pub tag: &'static str,
    pub text: String,
    pub number: Option<f64>,
    pub boolean: Option<bool>,
}

#[derive(Debug)]
pub struct RunnerOutput {
    pub value: RunnerValue,
    pub cpu_ms: u128,
    pub memory_bytes: u64,
}

#[derive(Debug)]
pub enum RunnerError {
    Compile(String),
    Runtime(String),
    GasLimit,
    CpuLimit,
    MemoryLimit,
    OutputLimit,
    UnsupportedResult(String),
}

pub trait ExecutionRunner: Send + Sync {
    fn descriptor(&self) -> LanguageDescriptor;
    fn execute(&self, source: &str, limits: ExecutionLimits) -> Result<RunnerOutput, RunnerError>;
    fn supports_cluster_dispatch(&self) -> bool;
    fn supports_deterministic_verification(&self) -> bool;
}

#[derive(Debug)]
pub enum RunnerResolutionError {
    Unsupported(String),
    Unavailable(LanguageDescriptor),
}

#[derive(Default)]
pub struct RunnerRegistry {
    omni: OmniRunner,
    lua: LuaRunner,
    wasm: WasmRunner,
}

impl RunnerRegistry {
    pub fn resolve(&self, id: &str) -> Result<&dyn ExecutionRunner, RunnerResolutionError> {
        match id {
            "default" | "omni" => Ok(&self.omni),
            "lua" => Ok(&self.lua),
            "javascript" | "js" => Err(RunnerResolutionError::Unavailable(UNAVAILABLE_JAVASCRIPT)),
            "python" => Err(RunnerResolutionError::Unavailable(UNAVAILABLE_PYTHON)),
            "wasm" => Ok(&self.wasm),
            _ => Err(RunnerResolutionError::Unsupported(id.to_string())),
        }
    }

    pub fn languages(&self) -> [LanguageDescriptor; 5] {
        [
            OMNI_DESCRIPTOR,
            LUA_DESCRIPTOR,
            UNAVAILABLE_JAVASCRIPT,
            UNAVAILABLE_PYTHON,
            WASM_DESCRIPTOR,
        ]
    }

    pub fn omni(&self) -> &dyn ExecutionRunner {
        &self.omni
    }
}

const OMNI_CAPABILITIES: RunnerCapabilities = RunnerCapabilities {
    filesystem_reads: false,
    filesystem_writes: false,
    module_imports: false,
    host_functions: false,
    network_access: false,
    gas_limit: true,
    cpu_limit: true,
    memory_limit: true,
    unicode_text: true,
    deterministic_verification: true,
    cluster_dispatch: true,
};

const LUA_CAPABILITIES: RunnerCapabilities = RunnerCapabilities {
    filesystem_reads: false,
    filesystem_writes: false,
    module_imports: false,
    host_functions: false,
    network_access: false,
    gas_limit: true,
    cpu_limit: true,
    memory_limit: true,
    unicode_text: true,
    deterministic_verification: false,
    cluster_dispatch: false,
};

const WASM_CAPABILITIES: RunnerCapabilities = RunnerCapabilities {
    filesystem_reads: false,
    filesystem_writes: false,
    module_imports: false,
    host_functions: false,
    network_access: false,
    gas_limit: true,
    cpu_limit: true,
    memory_limit: true,
    unicode_text: false,
    deterministic_verification: false,
    cluster_dispatch: false,
};

const UNAVAILABLE_CAPABILITIES: RunnerCapabilities = RunnerCapabilities {
    filesystem_reads: false,
    filesystem_writes: false,
    module_imports: false,
    host_functions: false,
    network_access: false,
    gas_limit: false,
    cpu_limit: false,
    memory_limit: false,
    unicode_text: false,
    deterministic_verification: false,
    cluster_dispatch: false,
};

const OMNI_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "omni",
    name: "OmniEngine",
    runtime: "omni-core",
    available: true,
    standard_libraries: &[],
    capabilities: OMNI_CAPABILITIES,
    unavailable_reason: None,
};

const LUA_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "lua",
    name: "Lua 5.4",
    runtime: "mlua / Lua 5.4",
    available: true,
    standard_libraries: &["base", "math"],
    capabilities: LUA_CAPABILITIES,
    unavailable_reason: None,
};

const UNAVAILABLE_JAVASCRIPT: LanguageDescriptor = LanguageDescriptor {
    id: "javascript",
    name: "JavaScript",
    runtime: "not configured",
    available: false,
    standard_libraries: &[],
    capabilities: UNAVAILABLE_CAPABILITIES,
    unavailable_reason: Some(
        "No embedded JavaScript interpreter with enforced gas and memory limits is configured; host processes are disabled.",
    ),
};

const UNAVAILABLE_PYTHON: LanguageDescriptor = LanguageDescriptor {
    id: "python",
    name: "Python",
    runtime: "not configured",
    available: false,
    standard_libraries: &[],
    capabilities: UNAVAILABLE_CAPABILITIES,
    unavailable_reason: Some(
        "No embedded Python interpreter with enforced gas and memory limits is configured; host processes are disabled.",
    ),
};

const WASM_DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "wasm",
    name: "WebAssembly",
    runtime: "Wasmi 0.46 / WAT",
    available: true,
    standard_libraries: &[],
    capabilities: WASM_CAPABILITIES,
    unavailable_reason: None,
};

#[derive(Default)]
struct OmniRunner;

impl ExecutionRunner for OmniRunner {
    fn descriptor(&self) -> LanguageDescriptor {
        OMNI_DESCRIPTOR
    }

    fn execute(&self, source: &str, limits: ExecutionLimits) -> Result<RunnerOutput, RunnerError> {
        if limits.memory_limit_bytes == 0 {
            return Err(RunnerError::MemoryLimit);
        }

        let mut engine = Engine::new();
        engine.set_step_limit(limits.gas_limit.min(DEFAULT_STEP_LIMIT));
        engine.set_time_limit(Some(limits.cpu_limit));
        if !engine.set_memory_limit(limits.memory_limit_bytes) {
            return Err(RunnerError::MemoryLimit);
        }

        let started = Instant::now();
        let result = engine.exec(source);
        let elapsed = started.elapsed();
        let stats = engine.stats();
        let memory_bytes = stats
            .arena_bytes_reserved
            .saturating_add(stats.egress_bytes_reserved);

        if elapsed >= limits.cpu_limit {
            return Err(RunnerError::CpuLimit);
        }
        if memory_bytes > limits.memory_limit_bytes as u64 {
            return Err(RunnerError::MemoryLimit);
        }

        let value = result.map_err(|error| {
            if error.message == CPU_LIMIT_MESSAGE {
                RunnerError::CpuLimit
            } else if error.message == MEMORY_LIMIT_MESSAGE {
                RunnerError::MemoryLimit
            } else {
                match error.kind {
                    ErrorKind::Lex | ErrorKind::Parse => RunnerError::Compile(error.to_string()),
                    _ => RunnerError::Runtime(error.to_string()),
                }
            }
        })?;
        let rendered = engine.render(value);
        if rendered.len() > limits.max_output_bytes {
            return Err(RunnerError::OutputLimit);
        }

        Ok(RunnerOutput {
            value: RunnerValue {
                tag: if value.is_string() {
                    "string"
                } else if value.as_f64().is_some() {
                    "number"
                } else if value.as_bool().is_some() {
                    "boolean"
                } else if value == omni_core::Value::NIL {
                    "null"
                } else {
                    "undefined"
                },
                text: rendered,
                number: value.as_f64(),
                boolean: value.as_bool(),
            },
            cpu_ms: elapsed.as_millis(),
            memory_bytes,
        })
    }

    fn supports_cluster_dispatch(&self) -> bool {
        true
    }

    fn supports_deterministic_verification(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct LuaRunner;

impl ExecutionRunner for LuaRunner {
    fn descriptor(&self) -> LanguageDescriptor {
        LUA_DESCRIPTOR
    }

    fn execute(&self, source: &str, limits: ExecutionLimits) -> Result<RunnerOutput, RunnerError> {
        if limits.memory_limit_bytes == 0 {
            return Err(RunnerError::MemoryLimit);
        }

        let lua = Lua::new_with(StdLib::MATH, LuaOptions::default()).map_err(map_lua_error)?;
        lua.set_memory_limit(limits.memory_limit_bytes)
            .map_err(map_lua_error)?;

        let globals = lua.globals();
        for name in [
            "collectgarbage",
            "dofile",
            "load",
            "loadfile",
            "print",
            "require",
            "warn",
        ] {
            globals.set(name, LuaValue::Nil).map_err(map_lua_error)?;
        }
        let math: mlua::Table = globals.get("math").map_err(map_lua_error)?;
        math.set("random", LuaValue::Nil).map_err(map_lua_error)?;
        math.set("randomseed", LuaValue::Nil)
            .map_err(map_lua_error)?;

        let start = Instant::now();
        let deadline = start + limits.cpu_limit;
        let instruction_count = Rc::new(Cell::new(0u64));
        let hook_instructions = Rc::clone(&instruction_count);
        let peak_memory = Rc::new(Cell::new(lua.used_memory()));
        let hook_peak_memory = Rc::clone(&peak_memory);
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(1),
            move |lua, _debug| {
                let count = hook_instructions.get().saturating_add(1);
                hook_instructions.set(count);
                let current_memory = lua.used_memory();
                hook_peak_memory.set(hook_peak_memory.get().max(current_memory));
                if current_memory > limits.memory_limit_bytes {
                    return Err(LuaError::RuntimeError(MEMORY_LIMIT_MESSAGE.to_string()));
                }
                if count > limits.gas_limit {
                    return Err(LuaError::RuntimeError(LUA_GAS_LIMIT.to_string()));
                }
                if Instant::now() >= deadline {
                    return Err(LuaError::RuntimeError(LUA_CPU_LIMIT.to_string()));
                }
                Ok(VmState::Continue)
            },
        )
        .map_err(map_lua_error)?;

        let result = lua
            .load(source)
            .set_mode(ChunkMode::Text)
            .eval::<LuaValue>();
        let elapsed = start.elapsed();
        let current_memory = lua.used_memory();
        peak_memory.set(peak_memory.get().max(current_memory));

        if elapsed >= limits.cpu_limit {
            return Err(RunnerError::CpuLimit);
        }
        if current_memory > limits.memory_limit_bytes {
            return Err(RunnerError::MemoryLimit);
        }

        let value = result.map_err(map_lua_error)?;
        let output = match value {
            LuaValue::Nil => RunnerValue {
                tag: "null",
                text: "nil".to_string(),
                number: None,
                boolean: None,
            },
            LuaValue::Boolean(value) => RunnerValue {
                tag: "boolean",
                text: value.to_string(),
                number: None,
                boolean: Some(value),
            },
            LuaValue::Integer(value) => RunnerValue {
                tag: "number",
                text: value.to_string(),
                number: Some(value as f64),
                boolean: None,
            },
            LuaValue::Number(value) if value.is_finite() => RunnerValue {
                tag: "number",
                text: value.to_string(),
                number: Some(value),
                boolean: None,
            },
            LuaValue::Number(_) => {
                return Err(RunnerError::Runtime(
                    "Lua returned a non-finite number".to_string(),
                ));
            }
            LuaValue::String(value) => {
                if value.as_bytes().len() > limits.max_output_bytes {
                    return Err(RunnerError::OutputLimit);
                }
                let text = value
                    .to_str()
                    .map_err(|_| {
                        RunnerError::UnsupportedResult(
                            "Lua returned a string that is not valid UTF-8".to_string(),
                        )
                    })?
                    .to_owned();
                RunnerValue {
                    tag: "string",
                    text,
                    number: None,
                    boolean: None,
                }
            }
            other => {
                return Err(RunnerError::UnsupportedResult(format!(
                    "Lua returned an unsupported {} result",
                    other.type_name()
                )));
            }
        };
        if output.text.len() > limits.max_output_bytes {
            return Err(RunnerError::OutputLimit);
        }

        Ok(RunnerOutput {
            value: output,
            cpu_ms: elapsed.as_millis(),
            memory_bytes: peak_memory.get() as u64,
        })
    }

    fn supports_cluster_dispatch(&self) -> bool {
        false
    }

    fn supports_deterministic_verification(&self) -> bool {
        false
    }
}

#[derive(Default)]
struct WasmRunner;

impl ExecutionRunner for WasmRunner {
    fn descriptor(&self) -> LanguageDescriptor {
        WASM_DESCRIPTOR
    }

    fn execute(&self, source: &str, limits: ExecutionLimits) -> Result<RunnerOutput, RunnerError> {
        if limits.memory_limit_bytes == 0 {
            return Err(RunnerError::MemoryLimit);
        }

        let mut config = WasmConfig::default();
        config.consume_fuel(true);
        config.set_stack_limits(
            WasmStackLimits::new(1_024, 64 * 1024, 128)
                .map_err(|error| RunnerError::Runtime(error.to_string()))?,
        );
        let engine = WasmEngine::new(&config);
        let module = WasmModule::new(&engine, source.as_bytes())
            .map_err(|error| RunnerError::Compile(error.to_string()))?;
        if module.imports().next().is_some() {
            return Err(RunnerError::UnsupportedResult(
                "WebAssembly imports are disabled; modules must be self-contained".to_string(),
            ));
        }

        let table_limit = limits.memory_limit_bytes / std::mem::size_of::<u128>();
        let state = WasmStoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_limit_bytes)
                .memories(1)
                .tables(1)
                .table_elements(table_limit)
                .instances(1)
                .trap_on_grow_failure(true)
                .build(),
            peak_memory_bytes: 0,
            memory_limit_exceeded: false,
        };
        let mut store = WasmStore::new(&engine, state);
        store.limiter(|state| state);
        store
            .set_fuel(limits.gas_limit)
            .map_err(|error| RunnerError::Runtime(error.to_string()))?;

        let linker = WasmLinker::new(&engine);
        let instance = linker
            .instantiate(&mut store, &module)
            .and_then(|instance| instance.start(&mut store))
            .map_err(|error| map_wasm_runtime_error(error, &store))?;
        let started = Instant::now();
        let function = instance.get_func(&store, "run").ok_or_else(|| {
            RunnerError::UnsupportedResult(
                "WebAssembly module must export a no-argument function named 'run'".to_string(),
            )
        })?;

        let value = if let Ok(function) = function.typed::<(), i32>(&store) {
            let value = function
                .call(&mut store, ())
                .map_err(|error| map_wasm_runtime_error(error, &store))?;
            wasm_number_value(value.to_string(), value as f64)
        } else if let Ok(function) = function.typed::<(), i64>(&store) {
            let value = function
                .call(&mut store, ())
                .map_err(|error| map_wasm_runtime_error(error, &store))?;
            wasm_number_value(value.to_string(), value as f64)
        } else if let Ok(function) = function.typed::<(), f32>(&store) {
            let value = function
                .call(&mut store, ())
                .map_err(|error| map_wasm_runtime_error(error, &store))?;
            if !value.is_finite() {
                return Err(RunnerError::Runtime(
                    "WebAssembly returned a non-finite number".to_string(),
                ));
            }
            wasm_number_value(value.to_string(), value as f64)
        } else if let Ok(function) = function.typed::<(), f64>(&store) {
            let value = function
                .call(&mut store, ())
                .map_err(|error| map_wasm_runtime_error(error, &store))?;
            if !value.is_finite() {
                return Err(RunnerError::Runtime(
                    "WebAssembly returned a non-finite number".to_string(),
                ));
            }
            wasm_number_value(value.to_string(), value)
        } else if let Ok(function) = function.typed::<(), ()>(&store) {
            function
                .call(&mut store, ())
                .map_err(|error| map_wasm_runtime_error(error, &store))?;
            RunnerValue {
                tag: "null",
                text: "null".to_string(),
                number: None,
                boolean: None,
            }
        } else {
            return Err(RunnerError::UnsupportedResult(
                "WebAssembly 'run' must take no parameters and return at most one i32, i64, f32, f64, or no value"
                    .to_string(),
            ));
        };

        let elapsed = started.elapsed();
        if elapsed >= limits.cpu_limit {
            return Err(RunnerError::CpuLimit);
        }
        if value.text.len() > limits.max_output_bytes {
            return Err(RunnerError::OutputLimit);
        }

        Ok(RunnerOutput {
            value,
            cpu_ms: elapsed.as_millis(),
            memory_bytes: store.data().peak_memory_bytes as u64,
        })
    }

    fn supports_cluster_dispatch(&self) -> bool {
        false
    }

    fn supports_deterministic_verification(&self) -> bool {
        false
    }
}

fn wasm_number_value(text: String, number: f64) -> RunnerValue {
    RunnerValue {
        tag: "number",
        text,
        number: Some(number),
        boolean: None,
    }
}

fn map_wasm_runtime_error(error: wasmi::Error, store: &WasmStore<WasmStoreState>) -> RunnerError {
    if store.data().memory_limit_exceeded {
        return RunnerError::MemoryLimit;
    }
    let message = error.to_string();
    if message.to_ascii_lowercase().contains("fuel") {
        RunnerError::GasLimit
    } else {
        RunnerError::Runtime(message)
    }
}

struct WasmStoreState {
    limits: StoreLimits,
    peak_memory_bytes: usize,
    memory_limit_exceeded: bool,
}

impl ResourceLimiter for WasmStoreState {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        let permitted = self.limits.memory_growing(current, desired, maximum);
        match &permitted {
            Ok(true) => self.peak_memory_bytes = self.peak_memory_bytes.max(desired),
            Ok(false) | Err(_) => self.memory_limit_exceeded = true,
        }
        permitted
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        let permitted = self.limits.table_growing(current, desired, maximum);
        match &permitted {
            Ok(true) => {}
            Ok(false) | Err(_) => self.memory_limit_exceeded = true,
        }
        permitted
    }

    fn instances(&self) -> usize {
        self.limits.instances()
    }

    fn tables(&self) -> usize {
        self.limits.tables()
    }

    fn memories(&self) -> usize {
        self.limits.memories()
    }
}

fn map_lua_error(error: LuaError) -> RunnerError {
    match error {
        LuaError::SyntaxError { message, .. } => RunnerError::Compile(message),
        LuaError::MemoryError(_) | LuaError::MemoryControlNotAvailable => RunnerError::MemoryLimit,
        LuaError::RuntimeError(message) if message.contains(LUA_CPU_LIMIT) => RunnerError::CpuLimit,
        LuaError::RuntimeError(message) if message.contains(LUA_GAS_LIMIT) => RunnerError::GasLimit,
        LuaError::RuntimeError(message) if message.contains(MEMORY_LIMIT_MESSAGE) => {
            RunnerError::MemoryLimit
        }
        other => RunnerError::Runtime(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(gas_limit: u64, cpu: Duration, memory_bytes: usize) -> ExecutionLimits {
        ExecutionLimits {
            gas_limit,
            cpu_limit: cpu,
            memory_limit_bytes: memory_bytes,
            max_output_bytes: 65_536,
        }
    }

    #[test]
    fn registry_describes_available_and_unavailable_runtimes() {
        let registry = RunnerRegistry::default();
        let languages = registry.languages();
        assert_eq!(languages[0].id, "omni");
        assert!(languages[0].available);
        assert_eq!(languages[1].id, "lua");
        assert!(languages[1].available);
        assert!(!languages[2].available);
        assert!(!languages[3].available);
        assert_eq!(languages[4].id, "wasm");
        assert!(languages[4].available);
        assert!(registry.resolve("wasm").is_ok());
    }

    #[test]
    fn lua_runner_evaluates_expressions_and_unicode() {
        let runner = LuaRunner;
        let result = runner
            .execute(
                "return \"বাংলা — 日本語 — 😀\"",
                limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap();
        assert_eq!(result.value.tag, "string");
        assert_eq!(result.value.text, "বাংলা — 日本語 — 😀");

        let result = runner
            .execute(
                "return 40 + 2",
                limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap();
        assert_eq!(result.value.number, Some(42.0));
    }

    #[test]
    fn lua_runner_enforces_instruction_and_memory_limits() {
        let runner = LuaRunner;
        let gas_error = runner
            .execute(
                "while true do end",
                limits(200, Duration::from_secs(1), 128 * 1024 * 1024),
            )
            .unwrap_err();
        assert!(matches!(gas_error, RunnerError::GasLimit));

        let memory_error = runner
            .execute(
                "local values = {}; while true do values[#values + 1] = {1, 2, 3} end",
                limits(10_000_000, Duration::from_secs(1), 256 * 1024),
            )
            .unwrap_err();
        assert!(matches!(memory_error, RunnerError::MemoryLimit));
    }

    #[test]
    fn lua_runner_enforces_output_limit_and_result_types() {
        let runner = LuaRunner;
        let mut constrained = limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024);
        constrained.max_output_bytes = 4;
        let output_error = runner
            .execute("return \"longer\"", constrained)
            .unwrap_err();
        assert!(matches!(output_error, RunnerError::OutputLimit));

        let result_error = runner
            .execute(
                "return {}",
                limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap_err();
        assert!(matches!(result_error, RunnerError::UnsupportedResult(_)));
    }

    #[test]
    fn lua_runner_does_not_expose_host_capabilities() {
        let runner = LuaRunner;
        for source in [
            "return io",
            "return os",
            "return package",
            "return debug",
            "return print",
            "return dofile",
            "return load",
            "return loadfile",
            "return require",
            "return math.random",
            "return math.randomseed",
            "return string",
            "return table",
        ] {
            let result = runner
                .execute(
                    source,
                    limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024),
                )
                .unwrap();
            assert_eq!(result.value.tag, "null", "{source}");
            assert_eq!(result.value.text, "nil", "{source}");
        }
    }

    #[test]
    fn wasm_runner_executes_wat_and_enforces_fuel_and_memory_limits() {
        let runner = WasmRunner;
        let result = runner
            .execute(
                ";; WAT module: export a no-argument run function.\n\
                 (module\n\
                   (func (export \"run\") (result i32)\n\
                     i32.const 42))",
                limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap();
        assert_eq!(result.value.tag, "number");
        assert_eq!(result.value.number, Some(42.0));

        let memory_preset = runner
            .execute(
                "(module\n\
                   (memory (export \"memory\") 1 2)\n\
                   (func (export \"run\") (result i32)\n\
                     memory.size))",
                limits(50_000, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap();
        assert_eq!(memory_preset.value.number, Some(1.0));

        let gas_error = runner
            .execute(
                "(module (func (export \"run\") (loop $again (br $again))))",
                limits(100, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap_err();
        assert!(matches!(gas_error, RunnerError::GasLimit));

        let memory_error = runner
            .execute(
                "(module (memory (export \"memory\") 1 4) \
                 (func (export \"run\") (result i32) i32.const 2 memory.grow drop i32.const 42))",
                limits(10_000, Duration::from_millis(50), 64 * 1024),
            )
            .unwrap_err();
        assert!(matches!(memory_error, RunnerError::MemoryLimit));

        let table_error = runner
            .execute(
                "(module (table 5000 funcref) (func (export \"run\")))",
                limits(10_000, Duration::from_millis(50), 64 * 1024),
            )
            .unwrap_err();
        assert!(matches!(table_error, RunnerError::MemoryLimit));
    }

    #[test]
    fn wasm_runner_rejects_host_imports_and_unsupported_exports() {
        let runner = WasmRunner;
        let import_error = runner
            .execute(
                "(module (import \"env\" \"host\" (func)) (func (export \"run\")))",
                limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap_err();
        assert!(matches!(import_error, RunnerError::UnsupportedResult(_)));

        let signature_error = runner
            .execute(
                "(module (func (export \"run\") (param i32) (result i32) local.get 0))",
                limits(10_000, Duration::from_millis(50), 128 * 1024 * 1024),
            )
            .unwrap_err();
        assert!(matches!(signature_error, RunnerError::UnsupportedResult(_)));
    }
}
