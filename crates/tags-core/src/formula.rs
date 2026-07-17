//! Engineering-value formulas (evalexpr): compiled once at engine start,
//! evaluated per update with the decoded `raw` value in scope.
//!
//! - `raw` — the decoded numeric value (f64) of THIS tag's register;
//! - `tag("other.name")` — read-at-eval access to another tag's CURRENT
//!   typed value (no recompute cascade: the value is whatever the engine
//!   last published for that tag; Bad/Absent reads evaluate to an error and
//!   the update degrades quality instead of publishing garbage);
//! - the full evalexpr builtin set (math, comparisons, if, min/max, ...).
//!
//! The inverse (`write_formula`) receives `value` (the engineering value an
//! OPC UA client wrote) and must produce the raw numeric to encode.

use std::sync::Arc;

use evalexpr::{
    build_operator_tree, ContextWithMutableFunctions, ContextWithMutableVariables, EvalexprError,
    Function, HashMapContext, Node, Value,
};

use crate::value::TypedValue;

#[derive(Debug, thiserror::Error)]
pub enum FormulaError {
    #[error("formula parse error: {0}")]
    Parse(#[from] EvalexprError),
    #[error("formula did not evaluate to a number: {0:?}")]
    NotNumeric(String),
    #[error("input value is not numeric")]
    NonNumericInput,
}

/// Read-at-eval provider for `tag("name")`. Implemented by the typed store.
pub trait TagLookup: Send + Sync + 'static {
    /// The tag's current numeric value, if it exists, is numeric and usable.
    fn numeric_value(&self, name: &str) -> Option<f64>;
}

/// A compiled formula. Cheap to evaluate; `Send + Sync`.
pub struct Formula {
    node: Node,
    text: String,
    lookup: Option<Arc<dyn TagLookup>>,
}

impl std::fmt::Debug for Formula {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Formula").field("text", &self.text).finish()
    }
}

/// Probe lookup for the compile-time trial eval: every tag "exists" with a
/// benign 0.0, so `tag("x")` never fails during the syntax probe.
struct ProbeLookup;
impl TagLookup for ProbeLookup {
    fn numeric_value(&self, _name: &str) -> Option<f64> {
        Some(0.0)
    }
}

impl Formula {
    /// Compile + trial-evaluate with `var_name = 0.0`. evalexpr's parser is
    /// lenient ("raw + " builds fine and only fails at eval time), so the
    /// probe eval is what actually gives fail-fast-at-boot semantics.
    pub fn compile(
        text: &str,
        var_name: &str,
        lookup: Option<Arc<dyn TagLookup>>,
    ) -> Result<Self, FormulaError> {
        let f = Self {
            node: build_operator_tree(text)?,
            text: text.to_string(),
            lookup,
        };
        // Trial eval with a stub lookup (the real store is empty at boot).
        f.eval_with(var_name, 0.0, Some(&(Arc::new(ProbeLookup) as Arc<dyn TagLookup>)))?;
        Ok(f)
    }

    /// Evaluate with `input` bound to `var_name` ("raw" for reads, "value"
    /// for the write inverse). Returns the numeric result.
    pub fn eval(&self, var_name: &str, input: f64) -> Result<f64, FormulaError> {
        self.eval_with(var_name, input, self.lookup.as_ref())
    }

    fn eval_with(
        &self,
        var_name: &str,
        input: f64,
        lookup: Option<&Arc<dyn TagLookup>>,
    ) -> Result<f64, FormulaError> {
        let mut ctx = HashMapContext::new();
        ctx.set_value(var_name.into(), Value::Float(input))?;
        // `tag()` is always defined so a formula referencing it never hits
        // "unknown function"; without a lookup it reports the tag as unknown.
        let lookup = lookup.map(Arc::clone);
        ctx.set_function(
            "tag".into(),
            Function::new(move |arg| {
                let name = arg.as_string()?;
                lookup
                    .as_ref()
                    .and_then(|l| l.numeric_value(&name))
                    .map(Value::Float)
                    .ok_or_else(|| {
                        EvalexprError::CustomMessage(format!(
                            "tag(\"{name}\"): unknown tag or non-numeric/absent value"
                        ))
                    })
            }),
        )?;
        let out = self.node.eval_with_context(&ctx)?;
        match out {
            Value::Float(f) => Ok(f),
            Value::Int(i) => Ok(i as f64),
            Value::Boolean(b) => Ok(f64::from(u8::from(b))),
            other => Err(FormulaError::NotNumeric(format!("{other:?}"))),
        }
    }
}

/// Compile-check every formula of a config WITHOUT starting the engine (B5):
/// the GUI merges the result into its validation panel so the operator sees
/// formula problems at edit time, not at server start.
///
/// Takes `(tag_name, formula, write_formula)` per tag and returns one
/// human-readable problem string per failing formula. Read formulas compile
/// against the variable `raw`, write formulas against `value` — exactly the
/// [`Formula::compile`] calls the engine and the write path perform at boot.
pub fn check_config_formulas<'a, I>(entries: I) -> Vec<String>
where
    I: IntoIterator<Item = (&'a str, Option<&'a str>, Option<&'a str>)>,
{
    let mut problems = Vec::new();
    for (tag, formula, write_formula) in entries {
        if let Some(text) = formula {
            if let Err(e) = Formula::compile(text, "raw", None) {
                problems.push(format!("tag `{tag}`: formula: {e}"));
            }
        }
        if let Some(text) = write_formula {
            if let Err(e) = Formula::compile(text, "value", None) {
                problems.push(format!("tag `{tag}`: write_formula: {e}"));
            }
        }
    }
    problems
}

/// The engineering transform of one tag: either a compiled formula or the
/// linear `raw * scale + offset` fallback.
#[derive(Debug)]
pub enum Transform {
    Linear { scale: f64, offset: f64 },
    Expr(Formula),
}

impl Transform {
    /// Build from the resolved register metadata. `formula` wins over
    /// scale/offset (validation warns about shadowing).
    pub fn from_meta(
        formula: Option<&str>,
        scale: f64,
        offset: f64,
        lookup: Option<Arc<dyn TagLookup>>,
    ) -> Result<Self, FormulaError> {
        match formula {
            Some(text) => Ok(Transform::Expr(Formula::compile(text, "raw", lookup)?)),
            None => Ok(Transform::Linear { scale, offset }),
        }
    }

    /// Whether this transform changes the value's type to Float. Identity
    /// linear transforms keep the native decoded type.
    pub fn is_identity(&self) -> bool {
        matches!(self, Transform::Linear { scale, offset } if *scale == 1.0 && *offset == 0.0)
    }

    /// Apply to a decoded typed value. Non-numeric values (Text/Bytes) pass
    /// through untouched — formulas only make sense for numerics.
    pub fn apply(&self, decoded: TypedValue) -> Result<TypedValue, FormulaError> {
        if self.is_identity() {
            return Ok(decoded);
        }
        match &decoded {
            TypedValue::Text(_) | TypedValue::Bytes(_) | TypedValue::Absent => Ok(decoded),
            v => {
                let raw = v.as_f64().ok_or(FormulaError::NonNumericInput)?;
                let out = match self {
                    Transform::Linear { scale, offset } => raw * scale + offset,
                    Transform::Expr(f) => f.eval("raw", raw)?,
                };
                Ok(TypedValue::Float(out))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_float_eq(v: TypedValue, expect: f64) {
        match v {
            TypedValue::Float(f) => assert!((f - expect).abs() < 1e-9, "{f} != {expect}"),
            other => panic!("expected Float({expect}), got {other:?}"),
        }
    }

    #[test]
    fn linear_transform_scales_and_keeps_identity_native() {
        let t = Transform::from_meta(None, 0.1, -5.0, None).unwrap();
        assert_float_eq(t.apply(TypedValue::UInt(237)).unwrap(), 18.7);

        let id = Transform::from_meta(None, 1.0, 0.0, None).unwrap();
        assert!(id.is_identity());
        // Identity keeps the native type (a u16 status stays UInt).
        assert_eq!(id.apply(TypedValue::UInt(7)).unwrap(), TypedValue::UInt(7));
        assert_eq!(id.apply(TypedValue::Bool(true)).unwrap(), TypedValue::Bool(true));
    }

    #[test]
    fn expression_formula_evaluates_with_raw() {
        let t = Transform::from_meta(Some("raw * 60"), 1.0, 0.0, None).unwrap();
        assert_eq!(t.apply(TypedValue::Float(2.5)).unwrap(), TypedValue::Float(150.0));

        // Booleans coerce to 0/1 as input; int results come back as Float.
        let t = Transform::from_meta(Some("if(raw > 100, 1, 0)"), 1.0, 0.0, None).unwrap();
        assert_eq!(t.apply(TypedValue::UInt(150)).unwrap(), TypedValue::Float(1.0));
    }

    #[test]
    fn parse_errors_fail_at_compile_time() {
        // evalexpr's parser is lenient — the probe eval catches these anyway.
        for bad in ["raw + ", "(raw", "raw raw", "raw +* 2", "unknown_var * 2", "nosuchfn(raw)"] {
            assert!(
                Formula::compile(bad, "raw", None).is_err(),
                "{bad:?} must fail at compile/probe"
            );
        }
        assert!(Transform::from_meta(Some("raw +"), 1.0, 0.0, None).is_err());
    }

    #[test]
    fn text_passes_through_untouched() {
        let t = Transform::from_meta(Some("raw * 2"), 1.0, 0.0, None).unwrap();
        assert_eq!(
            t.apply(TypedValue::Text("SN-1".into())).unwrap(),
            TypedValue::Text("SN-1".into())
        );
    }

    struct FakeLookup;
    impl TagLookup for FakeLookup {
        fn numeric_value(&self, name: &str) -> Option<f64> {
            match name {
                "phase.a" => Some(10.0),
                "phase.b" => Some(20.0),
                _ => None,
            }
        }
    }

    #[test]
    fn cross_tag_access_reads_current_values() {
        let lookup: Arc<dyn TagLookup> = Arc::new(FakeLookup);
        let t =
            Transform::from_meta(Some(r#"raw + tag("phase.a") + tag("phase.b")"#, ), 1.0, 0.0, Some(lookup))
                .unwrap();
        assert_eq!(t.apply(TypedValue::Float(5.0)).unwrap(), TypedValue::Float(35.0));
    }

    #[test]
    fn unknown_tag_in_formula_is_an_eval_error() {
        let lookup: Arc<dyn TagLookup> = Arc::new(FakeLookup);
        let t = Transform::from_meta(Some(r#"tag("nope") + raw"#), 1.0, 0.0, Some(lookup)).unwrap();
        assert!(t.apply(TypedValue::Float(1.0)).is_err());
    }

    /// Every builtin documented in the GUI formula help must actually compile,
    /// so the help never lists a function evalexpr does not provide.
    #[test]
    fn documented_builtins_compile() {
        for expr in [
            "if(raw > 0, 1, 0)",
            "min(raw, 100)",
            "max(raw, 0)",
            "math::abs(raw)",
            "math::sqrt(raw)",
            "math::pow(raw, 2)",
            "math::ln(raw)",
            "math::log(raw, 10)",
            "math::log2(raw)",
            "math::exp(raw)",
            "math::sin(raw)",
            "math::cos(raw)",
            "round(raw)",
            "floor(raw)",
            "ceil(raw)",
            "raw % 10",
            "raw ^ 2",
            "raw * 2 + 1 == 3",
        ] {
            assert!(
                Formula::compile(expr, "raw", None).is_ok(),
                "documented builtin/expr must compile: {expr:?}"
            );
        }
    }

    #[test]
    fn write_inverse_uses_value_variable() {
        let f = Formula::compile("value / 60", "value", None).unwrap();
        assert_eq!(f.eval("value", 150.0).unwrap(), 2.5);
    }

    #[test]
    fn check_config_formulas_reports_each_broken_formula_with_its_tag() {
        let problems = check_config_formulas([
            // Clean: both sides compile (incl. tag() cross-references).
            ("speed", Some(r#"raw * 60 + tag("phase.a")"#), Some("value / 60")),
            // No formulas at all: nothing to check.
            ("plain", None, None),
            // Broken read formula ("raw +" only fails at the probe eval).
            ("bad_read", Some("raw +"), None),
            // Broken write formula: `raw` is not in scope on the write side.
            ("bad_write", None, Some("raw * 2")),
            // Both broken: two problems for one tag.
            ("both", Some("(raw"), Some("value +*")),
        ]);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].contains("bad_read") && problems[0].contains("formula:"));
        assert!(problems[1].contains("bad_write") && problems[1].contains("write_formula:"));
        assert!(problems[2].contains("both") && problems[2].contains("formula:"));
        assert!(problems[3].contains("both") && problems[3].contains("write_formula:"));
    }
}
