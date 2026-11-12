//! Tool schemas written out in full. schemars puts nested types in `$defs` and points to them
//! with `$ref`, and an optional one becomes `anyOf: [{$ref}, {type: null}]`. Some clients don't
//! follow those, and their agents never see the field: Imaginarium's didn't see
//! update_milestone's `review` (2026-11-12). Optional fields stay optional by not being required.

use serde_json::{Map, Value};

/// How deep a type may nest itself before its `$ref` is left as it is.
const MAX_DEPTH: usize = 8;

pub fn inline(schema: &Map<String, Value>) -> Map<String, Value> {
    let defs = schema.get("$defs").or_else(|| schema.get("definitions")).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut out = match walk(&Value::Object(schema.clone()), &defs, 0) {
        Value::Object(o) => o,
        _ => schema.clone(),
    };
    if !out.contains_key("$defs") && !out.contains_key("definitions") {
        return out;
    }
    // Keep the definitions only if something still points to them (a type nested too deep).
    if !Value::Object(out.clone()).to_string().contains("\"$ref\"") {
        out.remove("$defs");
        out.remove("definitions");
    }
    out
}

fn walk(v: &Value, defs: &Map<String, Value>, depth: usize) -> Value {
    match v {
        Value::Object(o) => {
            if let Some(name) = o.get("$ref").and_then(Value::as_str).and_then(|r| r.rsplit('/').next())
                && depth < MAX_DEPTH
                && let Some(Value::Object(def)) = defs.get(name)
            {
                // The definition, with this place's own keys (its description) over it.
                let mut merged = def.clone();
                for (k, x) in o.iter().filter(|(k, _)| k.as_str() != "$ref") {
                    merged.insert(k.clone(), x.clone());
                }
                return walk(&Value::Object(merged), defs, depth + 1);
            }
            let mut out = Map::new();
            for (k, x) in o {
                if k == "$defs" || k == "definitions" {
                    out.insert(k.clone(), x.clone());
                    continue;
                }
                out.insert(k.clone(), walk(x, defs, depth));
            }
            // "X or null" is just X: leaving a field out already means none.
            if let Some(Value::Array(any)) = out.get("anyOf").cloned() {
                let non_null: Vec<&Value> = any.iter().filter(|x| x.get("type").and_then(Value::as_str) != Some("null")).collect();
                if non_null.len() == 1 && any.len() == 2 {
                    out.remove("anyOf");
                    if let Value::Object(inner) = non_null[0] {
                        for (k, x) in inner {
                            out.entry(k.clone()).or_insert_with(|| x.clone());
                        }
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, defs, depth)).collect()),
        x => x.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refs_and_optional_types_are_written_out() {
        let s = json!({
            "type": "object",
            "properties": {
                "milestone": {"type": "string"},
                "review": {"anyOf": [{"$ref": "#/$defs/ReviewInfo"}, {"type": "null"}], "description": "A review."},
                "tag": {"type": ["string", "null"]},
            },
            "required": ["milestone"],
            "$defs": {"ReviewInfo": {"type": "object", "properties": {"reports": {"type": "array", "items": {"type": "string"}}}}}
        });
        let out = Value::Object(inline(s.as_object().unwrap()));
        assert_eq!(out["properties"]["review"]["type"], "object", "{out}");
        assert_eq!(out["properties"]["review"]["description"], "A review.");
        assert!(out["properties"]["review"]["properties"]["reports"].is_object());
        assert!(out.get("$defs").is_none() && !out.to_string().contains("$ref"), "{out}");
        assert_eq!(out["properties"]["tag"]["type"], json!(["string", "null"]));
        // A type that nests itself keeps its definitions past the depth limit.
        let r = json!({"$ref": "#/$defs/N", "$defs": {"N": {"type": "object", "properties": {"next": {"$ref": "#/$defs/N"}}}}});
        let out = Value::Object(inline(r.as_object().unwrap()));
        assert!(out.get("$defs").is_some(), "{out}");
    }
}
