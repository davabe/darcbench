//! DARCBench Canonical JSON (DCJ/1).
//!
//! A signature is only meaningful if signer and verifier agree byte-for-byte on
//! what was signed. DCJ/1 defines that agreement:
//!
//! 1. Object keys are emitted in ascending lexicographic order of their UTF-8
//!    bytes. (`serde_json::Map` is a `BTreeMap` in this build - the
//!    `preserve_order` feature is deliberately *not* enabled anywhere in the
//!    workspace, and a test in this module asserts the ordering behaviour.)
//! 2. No insignificant whitespace.
//! 3. Numbers are emitted exactly as `serde_json` 1.x emits them: integers
//!    without a fractional part, floats via the shortest representation that
//!    round-trips to the same `f64`.
//! 4. Non-finite floats (`NaN`, `±Infinity`) are not representable and cause
//!    canonicalisation to fail rather than be silently coerced to `null`.
//! 5. **A conforming implementation MUST parse decimal numbers with correct
//!    rounding to nearest-even.** This is normative, and it is the rule that is
//!    easiest to get wrong: `serde_json` only guarantees it with the
//!    `float_roundtrip` feature enabled, which this workspace enables in the
//!    root `Cargo.toml`. Without it, writing `1234.5678901234567` and reading
//!    it back yields a value one ULP away, so re-canonicalising a bundle that
//!    was written to disk produces different bytes and its own signature no
//!    longer verifies. `signature_survives_a_disk_roundtrip` in
//!    `crate::bundle` is the regression test for exactly this.
//!
//! DCJ/1 is deliberately **not** advertised as RFC 8785 (JCS). It agrees with
//! JCS on key ordering and whitespace but does not implement JCS's number
//! serialisation algorithm in full. Claiming compliance we have not verified
//! would be worse than documenting the difference.

use serde::Serialize;

pub const CANONICALIZATION: &str = "DCJ/1";

#[derive(Debug, thiserror::Error)]
pub enum CanonicalError {
    #[error("value could not be serialised: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("value contains a non-finite number, which DCJ/1 cannot represent")]
    NonFinite,
}

/// Serialises `value` to canonical bytes.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let json = serde_json::to_value(value)?;
    reject_non_finite(&json)?;
    Ok(serde_json::to_vec(&json)?)
}

/// Walks a `Value` rejecting any non-finite number.
///
/// Note the ordering hazard: `serde_json` converts a non-finite `f64` to `Null`
/// *during* `to_value`, so by the time this runs the evidence is usually
/// already gone. That is why [`assert_finite`] exists and is called on raw
/// floats at the point measurements enter a bundle. This walk is the backstop
/// for `Value`s built by other means.
fn reject_non_finite(value: &serde_json::Value) -> Result<(), CanonicalError> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return Err(CanonicalError::NonFinite);
                }
            }
            Ok(())
        }
        serde_json::Value::Array(items) => items.iter().try_for_each(reject_non_finite),
        serde_json::Value::Object(map) => map.values().try_for_each(reject_non_finite),
        _ => Ok(()),
    }
}

/// Returns `Err` if any supplied float is non-finite.
///
/// Called at the boundary where measurements enter a bundle, so a broken
/// measurement fails loudly instead of becoming a `null` a verifier would
/// happily sign.
pub fn assert_finite(values: &[f64]) -> Result<(), CanonicalError> {
    if values.iter().all(|v| v.is_finite()) {
        Ok(())
    } else {
        Err(CanonicalError::NonFinite)
    }
}

/// SHA-256 of the canonical bytes, prefixed for self-description.
pub fn canonical_digest<T: Serialize>(value: &T) -> Result<String, CanonicalError> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "sha256:{}",
        hex::encode(Sha256::digest(canonical_json(value)?))
    ))
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_emitted_in_sorted_order() {
        let value = serde_json::json!({
            "zebra": 1,
            "alpha": 2,
            "Mango": 3,
            "_underscore": 4,
        });
        let bytes = canonical_json(&value).expect("canonical");
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(text, r#"{"Mango":3,"_underscore":4,"alpha":2,"zebra":1}"#);
    }

    #[test]
    fn nested_objects_are_also_sorted() {
        let value = serde_json::json!({ "b": { "z": 1, "a": 2 }, "a": [ { "y": 1, "x": 2 } ] });
        let text = String::from_utf8(canonical_json(&value).expect("c")).expect("utf8");
        assert_eq!(text, r#"{"a":[{"x":2,"y":1}],"b":{"a":2,"z":1}}"#);
    }

    #[test]
    fn output_has_no_insignificant_whitespace() {
        let text =
            String::from_utf8(canonical_json(&serde_json::json!({"a": [1, 2, 3]})).expect("c"))
                .expect("utf8");
        assert!(!text.contains(' '));
        assert!(!text.contains('\n'));
    }

    #[test]
    fn identical_logical_values_canonicalise_identically() {
        // Different insertion order must produce identical bytes; otherwise a
        // signature would depend on how a struct happened to be built.
        let a = serde_json::json!({"one": 1, "two": 2, "three": 3});
        let b = serde_json::json!({"three": 3, "one": 1, "two": 2});
        assert_eq!(
            canonical_json(&a).expect("a"),
            canonical_json(&b).expect("b")
        );
        assert_eq!(
            canonical_digest(&a).expect("a"),
            canonical_digest(&b).expect("b")
        );
    }

    #[test]
    fn digest_changes_when_any_value_changes() {
        let a = canonical_digest(&serde_json::json!({"score": 1000.0})).expect("a");
        let b = canonical_digest(&serde_json::json!({"score": 1000.1})).expect("b");
        assert_ne!(a, b);
        assert!(a.starts_with("sha256:"));
    }

    #[test]
    fn non_finite_floats_are_rejected_not_coerced() {
        assert!(assert_finite(&[1.0, 2.0]).is_ok());
        assert!(matches!(
            assert_finite(&[1.0, f64::NAN]),
            Err(CanonicalError::NonFinite)
        ));
        assert!(matches!(
            assert_finite(&[f64::INFINITY]),
            Err(CanonicalError::NonFinite)
        ));
    }

    #[test]
    fn floats_round_trip_through_canonical_form() {
        let value = serde_json::json!({"throughput": 1234.5678901234567_f64});
        let text = String::from_utf8(canonical_json(&value).expect("c")).expect("utf8");
        let back: serde_json::Value = serde_json::from_str(&text).expect("parse");
        assert_eq!(back["throughput"].as_f64(), value["throughput"].as_f64());
    }

    #[test]
    fn unicode_keys_sort_by_utf8_bytes() {
        let value = serde_json::json!({"é": 1, "z": 2, "a": 3});
        let text = String::from_utf8(canonical_json(&value).expect("c")).expect("utf8");
        // 'a' (0x61) < 'z' (0x7A) < 'é' (0xC3 0xA9)
        assert!(text.find(r#""a""#) < text.find(r#""z""#));
        assert!(text.find(r#""z""#) < text.find(r#""é""#));
    }
}
