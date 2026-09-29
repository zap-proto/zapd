//! `import zapd` — the ZAP router embedded in a Python process.
//!
//! ```python
//! import zapd
//! zapd.embed()                                   # stand for router
//! me = zapd.Node("agent:hanzo-mcp/dgx/42")       # take a seat, keep it
//! me.nodes()                                     # [{"id", "role", "brand", "caps", "attrs"}, ...]
//! me.call("browser:chromium/dgx/default", body)  # -> bytes
//! zapd.pair()                                    # the browser's pairing code
//! ```
//!
//! Every call releases the GIL while it waits; the router and the node run on
//! the crate's own threads, never on Python's.

use std::io::ErrorKind;
use std::time::Duration;

use pyo3::exceptions::{
    PyConnectionError, PyLookupError, PyOSError, PyPermissionError, PyTimeoutError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

fn err(e: std::io::Error) -> PyErr {
    let msg = e.to_string();
    match e.kind() {
        ErrorKind::TimedOut => PyTimeoutError::new_err(msg),
        ErrorKind::NotFound => PyLookupError::new_err(msg),
        ErrorKind::ConnectionReset => PyConnectionError::new_err(msg),
        ErrorKind::PermissionDenied => PyPermissionError::new_err(msg),
        _ => PyOSError::new_err(msg),
    }
}

/// Stand for this user's router. Returns at once; idempotent.
#[pyfunction]
fn embed() {
    // A Python host has no tracing subscriber; the router's few lines (who was
    // elected, which door refused what) go to stderr. `ZAP_LOG` filters them.
    let filter = tracing_subscriber::EnvFilter::try_from_env("ZAP_LOG")
        .unwrap_or_else(|_| "zapd=info".into());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
    zapd::embed();
}

/// This user's pairing code (minted on first use); `reset=True` rotates the key.
#[pyfunction]
#[pyo3(signature = (reset = false))]
fn pair(reset: bool) -> PyResult<String> {
    let p = if reset {
        zapd::pair::reset()
    } else {
        zapd::pair::load()
    };
    p.map(|p| p.code()).map_err(err)
}

#[pyfunction]
fn host() -> String {
    zapd::host()
}

#[pyfunction]
fn socket_path() -> String {
    zapd::socket_path().display().to_string()
}

/// This process's seat on the router. Reconnects by itself when the router
/// moves to another process.
#[pyclass(frozen)]
struct Node(zapd::Node);

#[pymethods]
impl Node {
    #[new]
    #[pyo3(signature = (id, role = "consumer", brand = "hanzo", caps = Vec::new()))]
    fn new(id: &str, role: &str, brand: &str, caps: Vec<String>) -> PyResult<Self> {
        let role = match role {
            "provider" => zapd::frame::ROLE_PROVIDER,
            "consumer" => zapd::frame::ROLE_CONSUMER,
            _ => {
                return Err(PyValueError::new_err(format!(
                    "role must be provider or consumer, not {role:?}"
                )))
            }
        };
        Ok(Node(zapd::Node::join(id, role, brand, &caps)))
    }

    #[getter]
    fn id(&self) -> String {
        self.0.id()
    }

    /// Route `payload` to node `to`; return its reply.
    #[pyo3(signature = (to, payload, timeout = 30.0))]
    fn call<'py>(
        &self,
        py: Python<'py>,
        to: &str,
        payload: &[u8],
        timeout: f64,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let payload = payload.to_vec();
        let out = py
            .detach(|| zapd::block_on(self.0.call(to, payload, Duration::from_secs_f64(timeout))))
            .map_err(err)?;
        Ok(PyBytes::new(py, &out))
    }

    /// Every node on this user's router: `[{"id", "role", "brand", "caps", "attrs"}]`.
    #[pyo3(signature = (timeout = 2.0))]
    fn nodes<'py>(&self, py: Python<'py>, timeout: f64) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let nodes = py
            .detach(|| zapd::block_on(self.0.nodes(Duration::from_secs_f64(timeout))))
            .map_err(err)?;
        nodes
            .into_iter()
            .map(|n| {
                let d = PyDict::new(py);
                d.set_item("id", n.id)?;
                d.set_item(
                    "role",
                    match n.desc.role {
                        zapd::frame::ROLE_PROVIDER => "provider",
                        zapd::frame::ROLE_CONSUMER => "consumer",
                        _ => "router",
                    },
                )?;
                d.set_item("brand", n.desc.brand)?;
                d.set_item("caps", n.desc.caps)?;
                d.set_item(
                    "attrs",
                    n.desc
                        .attrs
                        .into_iter()
                        .collect::<std::collections::HashMap<_, _>>(),
                )?;
                Ok(d)
            })
            .collect()
    }
}

#[pymodule(name = "zapd")]
fn zapd_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(embed, m)?)?;
    m.add_function(wrap_pyfunction!(pair, m)?)?;
    m.add_function(wrap_pyfunction!(host, m)?)?;
    m.add_function(wrap_pyfunction!(socket_path, m)?)?;
    m.add_class::<Node>()?;
    Ok(())
}
