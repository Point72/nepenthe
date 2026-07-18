//! Conditional dependency selectors.
//!
//! A dependency list may contain rattler-build-style `if:` blocks that
//! contribute their `then` (or `else`) entries only when a condition holds for
//! the build cell being resolved. The condition is a [minijinja] expression
//! evaluated against the cell's axes — the selected `python` version and
//! `variant` — so a manifest can, for example, drop a package on a Python that
//! has no build for it yet (`if: python != "3.13"`) or pick an
//! accelerator-specific dependency (`if: variant == "gpu"`).
//!
//! Only `python` and `variant` are exposed: a resolved cell's dependency set is
//! solved for every one of its platforms, so `platform` is not a single value
//! at resolve time and is deliberately absent from the condition context.
//!
//! Version-aware comparison is available through the `cmp(version, spec)`
//! function, which matches a version against a conda version spec — e.g.
//! `cmp(python, ">=3.12")` — since minijinja's bare `>=` compares strings
//! lexicographically (`"3.9" > "3.13"`).

use std::fmt;
use std::str::FromStr;

use minijinja::{context, Environment};
use rattler_conda_types::{ParseStrictness, Version, VersionSpec};

/// The build-cell axes an `if:` condition may reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// The selected Python version (e.g. `3.13`), or `None` when the
    /// environment has no Python axis. Exposed to conditions as `python`
    /// (an empty string when absent).
    pub python: Option<String>,
    /// The selected build variant (e.g. `gpu`), or `None` when the environment
    /// has no variant axis. Exposed to conditions as `variant` (an empty string
    /// when absent).
    pub variant: Option<String>,
}

/// Failure while evaluating a conditional `if:` expression: a compile error in
/// the expression, an evaluation error, or a bad argument to `cmp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConditionError {
    /// The offending condition source.
    pub condition: String,
    /// The underlying minijinja message.
    pub message: String,
}

impl fmt::Display for ConditionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid condition {:?}: {}",
            self.condition, self.message
        )
    }
}

impl std::error::Error for ConditionError {}

/// Evaluate `condition` against `cell`, returning whether it is truthy.
///
/// `condition` is a minijinja expression over `python` and `variant` (both
/// strings) plus the `cmp(version, spec)` helper. Any parse or evaluation
/// failure is reported as a [`ConditionError`] rather than silently treated as
/// false, so a typo surfaces instead of quietly dropping a dependency.
pub fn evaluate(condition: &str, cell: &Cell) -> Result<bool, ConditionError> {
    let mut env = Environment::empty();
    env.add_function("cmp", cmp);
    let err = |e: minijinja::Error| ConditionError {
        condition: condition.to_string(),
        message: e.to_string(),
    };
    let expr = env.compile_expression(condition).map_err(err)?;
    let ctx = context! {
        python => cell.python.clone().unwrap_or_default(),
        variant => cell.variant.clone().unwrap_or_default(),
    };
    Ok(expr.eval(ctx).map_err(err)?.is_true())
}

/// `cmp(version, spec)`: whether `version` satisfies the conda version `spec`
/// (e.g. `cmp(python, ">=3.12")`, `cmp(python, "3.11.*")`).
fn cmp(actual: String, spec: String) -> Result<bool, minijinja::Error> {
    let invalid = |what: &str, value: &str, e: &dyn fmt::Display| {
        minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("cmp: invalid {what} {value:?}: {e}"),
        )
    };
    let version = Version::from_str(&actual).map_err(|e| invalid("version", &actual, &e))?;
    let spec = VersionSpec::from_str(&spec, ParseStrictness::Lenient)
        .map_err(|e| invalid("version spec", &spec, &e))?;
    Ok(spec.matches(&version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(python: &str, variant: &str) -> Cell {
        Cell {
            python: (!python.is_empty()).then(|| python.to_string()),
            variant: (!variant.is_empty()).then(|| variant.to_string()),
        }
    }

    #[test]
    fn equality_on_python_and_variant() {
        let c = cell("3.13", "gpu");
        assert!(evaluate("python == \"3.13\"", &c).unwrap());
        assert!(!evaluate("python == \"3.12\"", &c).unwrap());
        assert!(evaluate("python != \"3.12\"", &c).unwrap());
        assert!(evaluate("variant == \"gpu\"", &c).unwrap());
        assert!(!evaluate("variant == \"gpu\" and python != \"3.13\"", &c).unwrap());
    }

    #[test]
    fn membership_and_boolean_ops() {
        let c = cell("3.11", "");
        assert!(evaluate("python in [\"3.11\", \"3.12\"]", &c).unwrap());
        assert!(!evaluate("python in [\"3.12\", \"3.13\"]", &c).unwrap());
        assert!(evaluate("not (python == \"3.13\")", &c).unwrap());
        // absent variant is an empty string
        assert!(evaluate("variant == \"\"", &c).unwrap());
    }

    #[test]
    fn cmp_is_version_aware() {
        let c = cell("3.9", "");
        // lexicographically "3.9" > "3.13", but version-wise it is not
        assert!(evaluate("python > \"3.13\"", &c).unwrap());
        assert!(!evaluate("cmp(python, \">=3.13\")", &c).unwrap());
        assert!(evaluate("cmp(python, \"<3.13\")", &c).unwrap());
        assert!(evaluate("cmp(python, \">=3.9\")", &c).unwrap());
    }

    #[test]
    fn parse_error_is_reported() {
        let err = evaluate("python ==", &cell("3.11", "")).unwrap_err();
        assert_eq!(err.condition, "python ==");
        assert!(!err.message.is_empty());
    }
}
