//! ECMAScript-identical UTF-16 ordering, number formatting, and canonical JSON serialization.

use std::cmp::Ordering;

use crate::yaml::YamlValue;

/// Compare two UTF-8 strings by UTF-16 code unit order, matching JavaScript `<` and `Array.prototype.sort()`.
#[must_use]
pub fn cmp_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// Canonical ECMAScript array index in `0..=4_294_967_294`.
#[must_use]
pub(crate) fn js_array_index(key: &str) -> Option<u32> {
    if key.is_empty() {
        return None;
    }
    if key == "0" {
        return Some(0);
    }
    let bytes = key.as_bytes();
    if !(b'1'..=b'9').contains(&bytes[0]) || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let parsed = key.parse::<u32>().ok()?;
    if parsed == u32::MAX {
        None
    } else {
        Some(parsed)
    }
}

/// Format an IEEE-754 `f64` according to ES262 6.1.6.1.20 (`Number::toString(x, 10)`).
#[must_use]
pub fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    if value.is_sign_negative() {
        let mut out = String::from("-");
        out.push_str(&format_positive_finite(-value));
        return out;
    }
    format_positive_finite(value)
}

/// Format an IEEE-754 `f64` as JavaScript `JSON.stringify` does (`NaN` and `±Infinity` become `"null"`).
#[must_use]
pub fn js_json_number_to_string(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_string();
    }
    js_number_to_string(value)
}

fn format_positive_finite(value: f64) -> String {
    debug_assert!(value.is_finite() && value > 0.0);
    // `serde_json` uses the shortest exact round-trip decimal representation (zmij/ryu),
    // giving the exact shortest decimal significand `(s, k, n)` required by ES262 6.1.6.1.20.
    let raw = serde_json::to_string(&value).expect("finite f64 serializes to JSON");
    let (mantissa, exp) = match raw.find(['e', 'E']) {
        Some(idx) => (
            &raw[..idx],
            raw[idx + 1..]
                .parse::<i32>()
                .expect("valid decimal exponent"),
        ),
        None => (raw.as_str(), 0_i32),
    };

    let dot_pos = mantissa.find('.').unwrap_or(mantissa.len());
    let all_digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let first_nz = all_digits
        .iter()
        .position(|&b| b != b'0')
        .expect("positive number has a non-zero digit");
    let last_nz = all_digits
        .iter()
        .rposition(|&b| b != b'0')
        .expect("positive number has a non-zero digit");
    let sig = std::str::from_utf8(&all_digits[first_nz..=last_nz]).expect("ASCII digits");
    let k = sig.len() as i32;
    let n_exp = (dot_pos as i32) - (first_nz as i32) + exp;

    if k <= n_exp && n_exp <= 21 {
        let mut out = String::with_capacity(n_exp as usize);
        out.push_str(sig);
        for _ in 0..(n_exp - k) {
            out.push('0');
        }
        out
    } else if 0 < n_exp && n_exp <= 21 {
        let split = n_exp as usize;
        format!("{}.{}", &sig[..split], &sig[split..])
    } else if -6 < n_exp && n_exp <= 0 {
        let zeros = (-n_exp) as usize;
        let mut out = String::with_capacity(2 + zeros + sig.len());
        out.push_str("0.");
        for _ in 0..zeros {
            out.push('0');
        }
        out.push_str(sig);
        out
    } else {
        let exp_val = n_exp - 1;
        let sign = if exp_val >= 0 { '+' } else { '-' };
        let abs_exp = exp_val.unsigned_abs();
        if k == 1 {
            format!("{sig}e{sign}{abs_exp}")
        } else {
            format!("{}.{}e{sign}{abs_exp}", &sig[..1], &sig[1..])
        }
    }
}

/// Canonical JSON serialization of a [`YamlValue`] matching TypeScript `canonicalJson` in `flow-assertions.ts`.
///
/// Mapping keys are sorted by UTF-16 code units, numbers use ES262 formatting, and non-finite
/// numbers serialize as `null`.
#[must_use]
pub fn canonical_json(value: &YamlValue) -> String {
    match value {
        YamlValue::Null => "null".to_string(),
        YamlValue::Bool(true) => "true".to_string(),
        YamlValue::Bool(false) => "false".to_string(),
        YamlValue::Number(num) => js_json_number_to_string(*num),
        YamlValue::String(text) => {
            serde_json::to_string(text).expect("UTF-8 string serializes to JSON")
        }
        YamlValue::Seq(items) => {
            let mut out = String::from("[");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json(item));
            }
            out.push(']');
            out
        }
        YamlValue::Map(entries) => {
            let mut sorted: Vec<&(String, YamlValue)> = entries.iter().collect();
            sorted.sort_by(|left, right| cmp_utf16(&left.0, &right.0));
            let mut out = String::from("{");
            for (index, (key, val)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("UTF-8 key serializes to JSON"));
                out.push(':');
                out.push_str(&canonical_json(val));
            }
            out.push('}');
            out
        }
    }
}
