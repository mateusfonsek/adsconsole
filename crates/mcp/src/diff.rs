//! Comparison of the expected state with the current one (undo conflict).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Field whose current value differs from the expected one. `path` is a JSON Pointer (`""` = the whole).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDiff {
    pub path: String,
    pub expected: Value,
    pub actual: Value,
}

/// Differences of `actual` relative to `expected`. In objects, only the keys of
/// `expected` count (a missing key is `null`); arrays and scalars are compared
/// whole, and numbers by value (`1 == 1.0`).
pub fn diff(expected: &Value, actual: &Value) -> Vec<FieldDiff> {
    let mut out = Vec::new();
    walk(String::new(), expected, actual, &mut out);
    out
}

fn walk(path: String, expected: &Value, actual: &Value, out: &mut Vec<FieldDiff>) {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for (key, value) in e {
                let child = format!("{path}/{}", escape(key));
                walk(child, value, a.get(key).unwrap_or(&Value::Null), out);
            }
        }
        _ if same(expected, actual) => {}
        _ => out.push(FieldDiff {
            path,
            expected: expected.clone(),
            actual: actual.clone(),
        }),
    }
}

/// Equality with numbers compared as `f64`.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// Escape of a JSON Pointer segment (RFC 6901).
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn diff_rules() {
        assert!(
            diff(
                &json!({"name":"a","budget":25.4}),
                &json!({"name":"a","budget":25.40,"extra":1})
            )
            .is_empty()
        );
        assert_eq!(
            diff(&json!({"name":"a"}), &json!({"name":"b"}))[0].path,
            "/name"
        );
        assert_eq!(
            diff(&json!({"t":{"geo":[1]}}), &json!({"t":{"geo":[2]}}))[0].path,
            "/t/geo"
        );
        assert_eq!(
            diff(&json!({"name":"a"}), &json!({}))[0].actual,
            Value::Null
        );
        assert_eq!(diff(&json!({"name":"a"}), &Value::Null).len(), 1);
        assert_eq!(diff(&json!({"name":"a"}), &Value::Null)[0].path, "");
    }

    #[test]
    fn diff_numbers_arrays_and_escaping() {
        assert!(
            diff(
                &json!({"n":1,"l":[1,{"x":2}]}),
                &json!({"n":1.0,"l":[1.0,{"x":2.0}]})
            )
            .is_empty()
        );
        assert_eq!(diff(&json!({"l":[1,2]}), &json!({"l":[1]}))[0].path, "/l");
        assert_eq!(
            diff(&json!({"a/b":{"c~":1}}), &json!({"a/b":{"c~":2}}))[0].path,
            "/a~1b/c~0"
        );
        let d = diff(&json!({"t":{"geo":[1]}}), &json!({"t":null}));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "/t");
        assert!(diff(&Value::Null, &Value::Null).is_empty());
    }
}
