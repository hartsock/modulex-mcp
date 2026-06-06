//! `modulex_py` — embed the modulex routine engine in Python.
//!
//! The inverse-embedding trick: Python hosts the engine (this extension
//! module), so libpython never links into the Rust binaries. Python step
//! handlers register **in-process** and run inside routines exactly like
//! builtins — including over MCP via [`Engine::serve_stdio`].
//!
//! ```python
//! import modulex_py
//! engine = modulex_py.Engine.from_config()      # standard search order
//!
//! @engine.step("standup-notes")                  # in-proc Python handler
//! def standup(spec: dict, ctx: dict) -> dict:
//!     return {"success": True, "output": "- shipped the leash"}
//!
//! report = engine.run_routine("morning", dry_run=True)
//! print(report.to_text())
//!
//! engine.serve_stdio()                           # MCP, Python steps included
//! ```
//!
//! Handler contract: `fn(spec: dict, ctx: dict) -> dict | str | None`.
//! `spec` carries `name`/`type`/`timeout`/`repos`/`params`; `ctx` carries
//! `generation`/`dry_run`/`shared`. A dict return uses the plugin-protocol
//! response shape (`success`/`skipped`/`output`/`error`/`repo_results`/
//! `data`); a str return is the section body; `None` means success with no
//! output. A raised exception becomes a failed step (soft — the routine
//! continues).
//!
//! NOTE: in-proc handlers are trusted code — you wrote them, they run in
//! your interpreter. The leash applies to everything they reach through the
//! engine (subprocess step types), not to arbitrary Python they execute.
//! For untrusted/isolated plugins use the subprocess plugin protocol
//! (`type = "python"`) instead.

#![forbid(unsafe_code)]

use std::sync::{Arc, OnceLock};

use modulex_core::{
    steps::builtin_registry, Config, Engine as CoreEngine, GrantedCaveats, Report as CoreReport,
    RunContext, RunOptions, StepHandler, StepRegistry, StepResult,
};
use modulex_mcp::Server;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict, PyList, PyTuple};

/// The one shared tokio runtime bridging async engine calls to Python's
/// synchronous boundary.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build the modulex tokio runtime")
    })
}

/// A Python-callable registered as a step handler.
struct PyStep {
    type_name: &'static str,
    callback: Py<PyAny>,
}

#[async_trait::async_trait]
impl StepHandler for PyStep {
    fn type_name(&self) -> &'static str {
        self.type_name
    }

    fn description(&self) -> &'static str {
        "Python-registered in-process step handler"
    }

    fn data_schema(&self) -> serde_json::Value {
        // Passthrough: the Python handler owns its payload.
        serde_json::json!({
            "description": "handler-defined payload (Python dict `data` field)"
        })
    }

    fn required_programs(&self, _spec: &modulex_core::StepSpec) -> Vec<String> {
        vec![] // in-proc; spawns nothing through the engine by itself
    }

    async fn run(&self, spec: &modulex_core::StepSpec, cx: &RunContext) -> StepResult {
        let spec_json = serde_json::json!({
            "name": spec.name,
            "type": spec.step_type,
            "timeout": spec.timeout,
            "repos": spec.repos,
            "params": serde_json::to_value(&spec.params).unwrap_or(serde_json::Value::Null),
        });
        let ctx_json = serde_json::json!({
            "generation": cx.generation,
            "dry_run": cx.dry_run,
            "shared": {
                "repos": cx.config.shared.repos,
                "identity": {
                    "username": cx.config.identity.username,
                    "gitlab_host": cx.config.identity.gitlab_host,
                },
            },
        });

        // The engine runs on the shared runtime with the GIL detached (see
        // run_routine), so attaching from this worker thread is safe.
        // block_in_place keeps a long-running handler from starving the
        // multi-thread runtime.
        let outcome = tokio::task::block_in_place(|| {
            Python::attach(|py| -> PyResult<serde_json::Value> {
                let spec_obj = json_to_py(py, &spec_json)?;
                let ctx_obj = json_to_py(py, &ctx_json)?;
                let ret = self.callback.bind(py).call1((spec_obj, ctx_obj))?;
                if ret.is_none() {
                    return Ok(serde_json::json!({}));
                }
                if let Ok(text) = ret.extract::<String>() {
                    return Ok(serde_json::json!({ "output": text }));
                }
                py_any_to_json(&ret)
            })
        });

        match outcome {
            Ok(response) => map_response(spec, &response),
            // A Python exception is a failed step, not a dead routine.
            Err(e) => StepResult::fail(&spec.name, &spec.step_type, e.to_string()),
        }
    }
}

/// Map a plugin-protocol-shaped response object onto a [`StepResult`]
/// (same field semantics as the subprocess plugin protocol).
fn map_response(spec: &modulex_core::StepSpec, response: &serde_json::Value) -> StepResult {
    use serde_json::Value;
    let mut result = StepResult::ok(
        &spec.name,
        &spec.step_type,
        response.get("output").and_then(Value::as_str).unwrap_or(""),
    );
    result.success = response
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    result.skipped = response
        .get("skipped")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    result.error = response
        .get("error")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    result.data = response.get("data").filter(|d| !d.is_null()).cloned();
    result
}

/// A finished routine report.
#[pyclass(frozen)]
struct Report {
    /// Monotonic run identity.
    #[pyo3(get)]
    generation: u64,
    /// True iff every non-skipped step succeeded.
    #[pyo3(get)]
    success: bool,
    /// The one-line accounting summary.
    #[pyo3(get)]
    summary: String,
    text: String,
    json: String,
}

#[pymethods]
impl Report {
    /// The report as human-readable markdown.
    fn to_text(&self) -> &str {
        &self.text
    }

    /// The report as compact JSON.
    fn to_json(&self) -> &str {
        &self.json
    }

    /// The report as a Python dict.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let value: serde_json::Value =
            serde_json::from_str(&self.json).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        json_to_py(py, &value)
    }

    fn __repr__(&self) -> String {
        format!("<modulex Report gen={} {}>", self.generation, self.summary)
    }
}

impl Report {
    fn from_core(report: &CoreReport) -> Self {
        Self {
            generation: report.generation,
            success: report.success,
            summary: report.summary.clone(),
            text: report.to_text(),
            json: report.to_json(),
        }
    }
}

/// The engine: load config, optionally register Python steps, then run
/// routines or serve MCP. Step registration must happen BEFORE the first
/// run/serve (the leash grant is resolved once, at build).
#[pyclass]
struct Engine {
    building: Option<(Config, StepRegistry)>,
    built: Option<Arc<CoreEngine>>,
}

impl Engine {
    fn ensure_built(&mut self) -> PyResult<Arc<CoreEngine>> {
        if let Some(engine) = &self.built {
            return Ok(engine.clone());
        }
        let (config, registry) = self
            .building
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("engine state poisoned"))?;
        let declared = config.declared_programs(&registry);
        let granted = GrantedCaveats::load(config.caveats.as_ref(), declared)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        eprintln!("{}", granted.banner());
        let engine = Arc::new(CoreEngine::new(config, registry, granted.caveats));
        self.built = Some(engine.clone());
        Ok(engine)
    }

    fn register(&mut self, name: &str, callback: Py<PyAny>) -> PyResult<()> {
        let Some((_, registry)) = self.building.as_mut() else {
            return Err(PyRuntimeError::new_err(
                "register steps before the first run_routine()/serve_stdio() — \
                 the leash grant is resolved once, at engine build",
            ));
        };
        // Step type names are few and registered once: leaking keeps the
        // StepHandler trait's &'static str contract.
        let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
        registry.register(Arc::new(PyStep {
            type_name: leaked,
            callback,
        }));
        Ok(())
    }
}

#[pymethods]
impl Engine {
    /// Load configuration: an explicit path, or the standard search order
    /// (`$MODULEX_CONFIG` → `./modulex.toml` → `~/.modulex/config.toml`).
    #[staticmethod]
    #[pyo3(signature = (path=None))]
    fn from_config(path: Option<String>) -> PyResult<Self> {
        let config = match path {
            Some(path) => Config::from_path(std::path::Path::new(&path)),
            None => Config::load().map(|(config, _)| config),
        }
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self {
            building: Some((config, builtin_registry())),
            built: None,
        })
    }

    /// Register `callback` as the handler for step type `name`.
    /// `callback(spec: dict, ctx: dict) -> dict | str | None`.
    fn register_step(&mut self, name: String, callback: Py<PyAny>) -> PyResult<()> {
        self.register(&name, callback)
    }

    /// Decorator sugar over [`Engine::register_step`]:
    /// `@engine.step("standup-notes")`.
    fn step<'py>(
        slf: Py<Engine>,
        py: Python<'py>,
        name: String,
    ) -> PyResult<Bound<'py, PyCFunction>> {
        PyCFunction::new_closure(
            py,
            None,
            None,
            move |args: &Bound<'_, PyTuple>,
                  _kwargs: Option<&Bound<'_, PyDict>>|
                  -> PyResult<Py<PyAny>> {
                let py = args.py();
                let func = args.get_item(0)?;
                slf.bind(py)
                    .borrow_mut()
                    .register(&name, func.clone().unbind())?;
                Ok(func.unbind())
            },
        )
    }

    /// Run a routine; returns its [`Report`].
    #[pyo3(signature = (routine, dry_run=false, only=None, skip=None))]
    fn run_routine(
        &mut self,
        py: Python<'_>,
        routine: String,
        dry_run: bool,
        only: Option<Vec<String>>,
        skip: Option<Vec<String>>,
    ) -> PyResult<Report> {
        let engine = self.ensure_built()?;
        let opts = RunOptions {
            only: only.unwrap_or_default(),
            skip: skip.unwrap_or_default(),
            dry_run,
        };
        // Detach the GIL: Python step handlers re-attach from runtime
        // workers, which would deadlock if this thread kept holding it.
        let report = py
            .detach(|| runtime().block_on(engine.run_routine(&routine, opts)))
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Report::from_core(&report))
    }

    /// Serve MCP on stdio with this engine (Python steps included). Blocks
    /// until stdin closes.
    fn serve_stdio(&mut self, py: Python<'_>) -> PyResult<()> {
        let engine = self.ensure_built()?;
        let server = Server::with_engine(engine);
        py.detach(|| runtime().block_on(server.run_stdio()))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    /// Configured routine names.
    fn routines(&mut self) -> PyResult<Vec<String>> {
        Ok(self
            .ensure_built()?
            .list_routines()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect())
    }

    /// Registered step types (builtins + Python registrations).
    fn step_types(&mut self) -> PyResult<Vec<String>> {
        if let Some((_, registry)) = &self.building {
            return Ok(registry.type_names());
        }
        Ok(self.ensure_built()?.step_types())
    }
}

/// Convert a `serde_json::Value` to a Python object.
fn json_to_py<'py>(py: Python<'py>, value: &serde_json::Value) -> PyResult<Bound<'py, PyAny>> {
    use serde_json::Value;
    match value {
        Value::Null => Ok(py.None().into_bound(py)),
        Value::Bool(b) => Ok(b.into_pyobject(py)?.to_owned().into_any()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.into_any())
            } else if let Some(u) = n.as_u64() {
                Ok(u.into_pyobject(py)?.into_any())
            } else {
                let f = n.as_f64().expect("JSON number is i64, u64, or f64");
                Ok(f.into_pyobject(py)?.into_any())
            }
        }
        Value::String(s) => Ok(s.into_pyobject(py)?.into_any()),
        Value::Array(arr) => {
            let list = PyList::empty(py);
            for item in arr {
                list.append(json_to_py(py, item)?)?;
            }
            Ok(list.into_any())
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, json_to_py(py, v)?)?;
            }
            Ok(dict.into_any())
        }
    }
}

/// Convert an arbitrary Python value to a `serde_json::Value`.
fn py_any_to_json(obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    use serde_json::Value;

    if obj.is_none() {
        return Ok(Value::Null);
    }
    // bool before int: Python bool is a subclass of int.
    if let Ok(b) = obj.extract::<bool>() {
        return Ok(Value::Bool(b));
    }
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(Value::from(i));
    }
    if let Ok(u) = obj.extract::<u64>() {
        return Ok(Value::from(u));
    }
    if let Ok(f) = obj.extract::<f64>() {
        return Ok(serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null));
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(Value::String(s));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = serde_json::Map::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            let key = k
                .extract::<String>()
                .map_err(|_| PyValueError::new_err("dict keys must be strings"))?;
            map.insert(key, py_any_to_json(&v)?);
        }
        return Ok(Value::Object(map));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        let mut arr = Vec::with_capacity(list.len());
        for item in list.iter() {
            arr.push(py_any_to_json(&item)?);
        }
        return Ok(Value::Array(arr));
    }
    if let Ok(seq) = obj.try_iter() {
        let mut arr = Vec::new();
        for item in seq {
            arr.push(py_any_to_json(&item?)?);
        }
        return Ok(Value::Array(arr));
    }

    Err(PyValueError::new_err(format!(
        "cannot convert Python value of type {} to JSON",
        obj.get_type().name()?
    )))
}

/// The `modulex_py` extension module.
#[pymodule]
#[pyo3(name = "modulex_py")]
fn modulex_py_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Engine>()?;
    m.add_class::<Report>()?;
    m.add("PLUGIN_PROTOCOL", modulex_core::steps::python::PROTOCOL)?;
    Ok(())
}
