//! Newtypes around PyO3 types which allow easier interfacing with
//! Timely or other Rust libraries we use.
use crate::try_unwrap;

use pyo3::basic::CompareOp;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::*;
use serde::ser::Error;
use std::fmt;

/// Represents a Python object flowing through a Timely dataflow.
///
/// As soon as you need to manipulate this object, convert it into a
/// [`PyObject`] or bind it into a [`Bound`]. This should only exist
/// within the dataflow.
///
/// A newtype for [`Py`]<[`PyAny`]> so we can
/// extend it with traits that Timely needs. See
/// <https://github.com/Ixrec/rust-orphan-rules> for why we need a
/// newtype and what they are.
#[derive(Clone)]
pub(crate) struct TdPyAny(PyObject);

impl TdPyAny {
    pub(crate) fn bind<'py>(&self, py: Python<'py>) -> &Bound<'py, PyAny> {
        self.0.bind(py)
    }
}

impl From<TdPyAny> for PyObject {
    fn from(x: TdPyAny) -> Self {
        x.0
    }
}

impl From<PyObject> for TdPyAny {
    fn from(x: PyObject) -> Self {
        Self(x)
    }
}

impl From<Bound<'_, PyAny>> for TdPyAny {
    fn from(x: Bound<'_, PyAny>) -> Self {
        Self(x.unbind())
    }
}

/// Allows you to debug print Python objects using their repr.
impl std::fmt::Debug for TdPyAny {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s: PyResult<String> = Python::with_gil(|py| {
            let self_ = self.bind(py);
            let binding = self_.repr()?;
            let repr = binding.to_str()?;
            Ok(String::from(repr))
        });
        f.write_str(&s.map_err(|_| std::fmt::Error {})?)
    }
}

/// Serialize Python objects flowing through Timely that cross
/// process bounds as pickled bytes.
impl serde::Serialize for TdPyAny {
    // We can't do better than isolating the Result<_, PyErr> part and
    // the explicitly converting.  1. `?` automatically trys to
    // convert using From<ReturnedError> for OuterError. But orphan
    // rule means we can't implement it since we don't own either py
    // or serde error types.  2. Using the newtype trick isn't worth
    // it, since you'd have to either wrap all the PyErr when they're
    // generated, or you implement From twice, once to get MyPyErr and
    // once to get serde::Err. And then you're calling .into()
    // explicitly since the last line isn't a `?` anyway.
    //
    // There's the separate problem if we could even implement the
    // Froms. We aren't allowed to "capture" generic types in an inner
    // `impl<S>` https://doc.rust-lang.org/error-index.html#E0401 and
    // we can't move them top-level since we don't know the concrete
    // type of S, and you're not allowed to have unconstrained generic
    // parameters https://doc.rust-lang.org/error-index.html#E0207 .
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Python::with_gil(|py| {
            let x = self.bind(py);
            let pickle = py.import_bound("pickle").map_err(S::Error::custom)?;
            let binding = pickle
                .call_method1("dumps", (x,))
                .map_err(S::Error::custom)?;
            let bytes = binding.downcast::<PyBytes>().map_err(S::Error::custom)?;
            serializer
                .serialize_bytes(bytes.as_bytes())
                .map_err(S::Error::custom)
        })
    }
}

pub(crate) struct PickleVisitor;

impl<'de> serde::de::Visitor<'de> for PickleVisitor {
    type Value = TdPyAny;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a pickled byte array")
    }

    fn visit_bytes<'py, E>(self, bytes: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let x: Result<TdPyAny, PyErr> = Python::with_gil(|py| {
            let pickle = py.import_bound("pickle")?;
            let x = pickle.call_method1("loads", (bytes,))?.unbind().into();
            Ok(x)
        });
        x.map_err(E::custom)
    }
}

/// Deserialize Python objects flowing through Timely that cross
/// process bounds from pickled bytes.
impl<'de> serde::Deserialize<'de> for TdPyAny {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_bytes(PickleVisitor)
    }
}

// Rust tests that interact with the Python interpreter don't work
// well under pyenv-virtualenv. This test executes under the global pyenv
// version, instead of the configured virtual environment.
// Disabling this test for aarch64, as it fails in CI.
#[cfg(not(target_arch = "aarch64"))]
#[test]
fn test_serde() {
    use serde_test::assert_tokens;
    use serde_test::Token;

    pyo3::prepare_freethreaded_python();

    let pyobj: TdPyAny =
        Python::with_gil(|py| PyString::new_bound(py, "hello").into_any().unbind().into());

    // Python < 3.8 serializes strings differently than python >= 3.8.
    // We get the current python version here so we can assert based on that.
    let (major, minor) = Python::with_gil(|py| {
        let sys = PyModule::import_bound(py, "sys").unwrap();
        let version = sys.getattr("version_info").unwrap();
        let major: i32 = version.getattr("major").unwrap().extract().unwrap();
        let minor: i32 = version.getattr("minor").unwrap().extract().unwrap();
        (major, minor)
    });

    // We only support python 3...
    assert_eq!(major, 3);

    let expected = if minor < 8 {
        Token::Bytes(&[128, 3, 88, 5, 0, 0, 0, 104, 101, 108, 108, 111, 113, 0, 46])
    } else {
        Token::Bytes(&[
            128, 4, 149, 9, 0, 0, 0, 0, 0, 0, 0, 140, 5, 104, 101, 108, 108, 111, 148, 46,
        ])
    };
    // This does a round-trip.
    assert_tokens(&pyobj, &[expected]);
}

/// Re-use Python's value semantics in Rust code.
impl PartialEq for TdPyAny {
    fn eq(&self, other: &Self) -> bool {
        Python::with_gil(|py| {
            // Don't use Py.eq or PyAny.eq since it only checks
            // pointer identity.
            let self_ = self.bind(py);
            let other = other.bind(py);
            try_unwrap!(self_
                .rich_compare(other, CompareOp::Eq)?
                .as_gil_ref()
                .is_truthy())
        })
    }
}

/// A Python object that is callable.
///
/// To actually call, you must [`bind`] it and use the bound interface
/// in order to not need to have a dual `TdPyX` vs `TdBoundX`.
pub(crate) struct TdPyCallable(PyObject);

/// Have PyO3 do type checking to ensure we only make from callable
/// objects.
impl<'py> FromPyObject<'py> for TdPyCallable {
    fn extract_bound(ob: &Bound<'py, PyAny>) -> PyResult<Self> {
        if ob.is_callable() {
            let py = ob.py();
            Ok(Self(ob.as_unbound().clone_ref(py)))
        } else {
            let msg = if let Ok(type_name) = ob.get_type().name() {
                format!("'{type_name}' object is not callable")
            } else {
                "object is not callable".to_string()
            };
            Err(PyTypeError::new_err(msg))
        }
    }
}

impl fmt::Debug for TdPyCallable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s: PyResult<String> = Python::with_gil(|py| {
            let name: String = self.0.bind(py).getattr("__name__")?.extract()?;
            Ok(name)
        });
        f.write_str(&s.map_err(|_| std::fmt::Error {})?)
    }
}

impl TdPyCallable {
    pub(crate) fn bind<'py>(&self, py: Python<'py>) -> &Bound<'py, PyAny> {
        self.0.bind(py)
    }
}

// This is a trait that can be implemented by any parent class.
// The function returns one of the possible subclasses instances.
pub(crate) trait PyConfigClass<S> {
    fn downcast(&self, py: Python) -> PyResult<S>;
}
