//! Validates decoded arguments against the original schema. Never changes a value the model
//! wrote: a call either passes or is rejected. The one addition is [`fill_omitted_nullable`].

use serde_json::{Map, Number, Value};

use crate::schema::{Format, Kind, Object, Range, Ty};

/// `Err((path, reason))` on the first violation.
///
/// A required field may be omitted when `null` is a valid value for it and the schema names no
/// other default (see `Field::omittable`). Strict-mode schemas
/// (OpenAI structured outputs, the OpenAI Agents SDK) list every property as required and
/// express "optional" as "nullable"; the compact signature gives such a field no `?`, so a model
/// that leaves it out has done what the prompt asked.
pub(crate) fn object(
    args: &Map<String, Value>,
    obj: &Object,
    path: &str,
) -> Result<(), (String, String)> {
    for f in obj.fields.iter().filter(|f| f.required && !f.omittable()) {
        if !args.contains_key(&f.name) {
            return Err((
                path.to_string(),
                format!("missing required field `{}`", f.name),
            ));
        }
    }
    for (key, value) in args {
        let child = format!("{path}.{key}");
        match obj.field(key) {
            Some(f) => ty(value, &f.ty, &child)?,
            None if obj.closed => return Err((child, "unexpected field".into())),
            None => {}
        }
    }
    Ok(())
}

/// Adds `null` for every required field the model omitted that the schema allows to be null,
/// so the call carries every key its strict-mode schema lists. Recurses into nested objects and
/// array items. Call only on arguments that already passed [`object`].
pub(crate) fn fill_omitted_nullable(args: &mut Map<String, Value>, obj: &Object) {
    for f in &obj.fields {
        if let Some(v) = args.get_mut(&f.name) {
            fill_ty(v, &f.ty);
        } else if f.omittable() {
            args.insert(f.name.clone(), Value::Null);
        }
    }
}

fn fill_ty(v: &mut Value, t: &Ty) {
    match (&t.kind, v) {
        (Kind::Object(obj), Value::Object(map)) => fill_omitted_nullable(map, obj),
        (Kind::Array(items), Value::Array(list)) => {
            for item in list {
                fill_ty(item, items);
            }
        }
        _ => {}
    }
}

fn ty(v: &Value, t: &Ty, path: &str) -> Result<(), (String, String)> {
    // An enum's members alone decide validity: `{"type":["string","null"],"enum":["a"]}`
    // rejects null (JSON Schema applies `type` and `enum` independently).
    if v.is_null() && t.nullable && !matches!(t.kind, Kind::Enum(_)) {
        return Ok(());
    }
    let fail = |reason: String| Err((path.to_string(), reason));
    match &t.kind {
        Kind::Any => Ok(()),
        Kind::Str(format) => match v.as_str() {
            Some(s) => match format {
                Some(f) if !format_ok(*f, s) => fail(format!("not a valid {}", f.schema_name())),
                _ => Ok(()),
            },
            None => fail("expected a string".into()),
        },
        Kind::Int(r) => match v.as_number() {
            Some(n) if is_integer(n) => check_range(n, r).or_else(fail),
            _ => fail("expected an integer".into()),
        },
        Kind::Num(r) => match v.as_number() {
            Some(n) => check_range(n, r).or_else(fail),
            None => fail("expected a number".into()),
        },
        Kind::Bool => match v.is_boolean() {
            true => Ok(()),
            false => fail("expected a boolean".into()),
        },
        Kind::Enum(values) => match values.contains(v) {
            true => Ok(()),
            false => fail(format!("{v} is not one of the allowed values")),
        },
        Kind::Array(items) => match v.as_array() {
            Some(list) => {
                for (i, item) in list.iter().enumerate() {
                    ty(item, items, &format!("{path}[{i}]"))?;
                }
                Ok(())
            }
            None => fail("expected an array".into()),
        },
        Kind::AnyObject => match v.is_object() {
            true => Ok(()),
            false => fail("expected an object".into()),
        },
        Kind::Object(obj) => match v.as_object() {
            Some(map) => object(map, obj, path),
            None => fail("expected an object".into()),
        },
    }
}

/// JSON Schema integers include floats with no fractional part (`30.0`).
fn is_integer(n: &Number) -> bool {
    n.is_i64()
        || n.is_u64()
        || n.as_f64()
            .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

fn check_range(n: &Number, r: &Range) -> Result<(), String> {
    let value = n.as_f64().unwrap_or(f64::NAN);
    if let Some(min) = r.min.as_ref().and_then(Number::as_f64)
        && (value.is_nan() || value < min)
    {
        return Err(format!("{n} is below the minimum {min}"));
    }
    if let Some(max) = r.max.as_ref().and_then(Number::as_f64)
        && (value.is_nan() || value > max)
    {
        return Err(format!("{n} is above the maximum {max}"));
    }
    Ok(())
}

/// Formats with an unambiguous syntax are checked; `email` and `uri` are hints to the model
/// only (strict validation of either rejects real-world values), as the crate docs state.
fn format_ok(f: Format, s: &str) -> bool {
    match f {
        Format::DateTime => is_date_time(s),
        Format::Date => is_date(s.as_bytes()),
        Format::Time => is_time(s.as_bytes()),
        Format::Uuid => is_uuid(s.as_bytes()),
        Format::Email | Format::Uri => true,
    }
}

/// RFC 3339 `date-time`: `full-date "T" full-time`.
pub(crate) fn is_date_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() > 11 && matches!(b[10], b'T' | b't') && is_date(&b[..10]) && is_time(&b[11..])
}

fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0, |acc, d| acc * 10 + u32::from(d - b'0')))
}

/// RFC 3339 `full-date`: `YYYY-MM-DD`, with real month lengths.
fn is_date(b: &[u8]) -> bool {
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let (Some(y), Some(m), Some(d)) = (digits(&b[..4]), digits(&b[5..7]), digits(&b[8..])) else {
        return false;
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d)
}

/// RFC 3339 `full-time`: `HH:MM:SS[.frac](Z|±HH:MM)`.
fn is_time(b: &[u8]) -> bool {
    if b.len() < 9 || b[2] != b':' || b[5] != b':' {
        return false;
    }
    let (Some(h), Some(mi), Some(s)) = (digits(&b[..2]), digits(&b[3..5]), digits(&b[6..8])) else {
        return false;
    };
    if h > 23 || mi > 59 || s > 60 {
        return false;
    }
    let mut rest = &b[8..];
    if let Some(frac) = rest.strip_prefix(b".") {
        let n = frac.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return false;
        }
        rest = &frac[n..];
    }
    match rest {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', oh @ ..] if oh.len() == 5 && oh[2] == b':' => {
            matches!((digits(&oh[..2]), digits(&oh[3..])), (Some(h), Some(m)) if h <= 23 && m <= 59)
        }
        _ => false,
    }
}

fn is_uuid(b: &[u8]) -> bool {
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_time_accepts_rfc3339_offsets_and_fractions() {
        for ok in [
            "2026-10-05T15:00:00+05:30",
            "2026-10-05T15:00:00Z",
            "2024-02-29T23:59:60.123-08:00",
        ] {
            assert!(is_date_time(ok), "{ok}");
        }
    }

    #[test]
    fn date_time_rejects_partial_or_impossible_values() {
        for bad in [
            "2026-10-05",
            "2026-10-05 15:00",
            "2026-10-05T15:00:00",
            "2026-02-29T10:00:00Z",
            "2026-13-01T10:00:00Z",
            "2026-10-05T24:00:00Z",
            "next monday",
        ] {
            assert!(!is_date_time(bad), "{bad}");
        }
    }
}
