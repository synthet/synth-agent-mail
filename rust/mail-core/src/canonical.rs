//! The bytes the sender's Ed25519 key covers: `JSON.stringify` of the value with object keys sorted
//! (`src/peer/wire.ts` `canonicalize`, `src/fabric/canonical.ts`).
//!
//! This writes its own key order instead of relying on `serde_json::Map` order, because another
//! crate in the build can switch on serde_json's `preserve_order` feature.

use serde_json::Value;

use crate::js::utf16_cmp;

pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        // Envelope numbers are integers, which serde_json and JSON.stringify print the same way.
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => out.push_str(&string(text)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| utf16_cmp(a, b));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&string(key));
                out.push(':');
                write(item, out);
            }
            out.push('}');
        }
    }
}

/// serde_json escapes `"`, `\` and U+0000..U+001F exactly as `JSON.stringify` does (short forms
/// `\b \t \n \f \r`, otherwise lowercase `\u00xx`) and leaves everything else, U+2028 included, as is.
fn string(text: &str) -> String {
    serde_json::to_string(text).expect("a str always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_escapes_like_json_stringify() {
        let value =
            json!({ "b": 1, "a": ["x\n\u{1}\"", true, null], "B": { "z": 0, "y": "\u{2028}" } });
        assert_eq!(
            canonical(&value),
            "{\"B\":{\"y\":\"\u{2028}\",\"z\":0},\"a\":[\"x\\n\\u0001\\\"\",true,null],\"b\":1}"
        );
    }
}
