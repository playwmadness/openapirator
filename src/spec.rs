//! Thin accessors over the raw OpenAPI document (`serde_json::Value`).

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

pub fn unescape_token(tok: &str) -> String {
    tok.replace("~1", "/").replace("~0", "~")
}

/// Resolve a `$ref`. Supports `#/...` pointers into the document root and, for `#/$defs/...`,
/// the root schema of the current media type (`local_root`).
pub fn resolve_ref<'a>(doc: &'a Value, local_root: &'a Value, r: &str) -> Result<&'a Value> {
    let Some(pointer) = r.strip_prefix('#') else {
        bail!("external $ref is not supported: {r}");
    };
    let base = if pointer.starts_with("/$defs/") && local_root.pointer(pointer).is_some() {
        local_root
    } else {
        doc
    };
    base.pointer(pointer)
        .with_context(|| format!("unresolvable $ref: {r}"))
}

/// If `v` is a `{"$ref": ...}` object, follow it (repeatedly); otherwise return `v`.
pub fn deref<'a>(doc: &'a Value, local_root: &'a Value, v: &'a Value) -> Result<&'a Value> {
    let mut cur = v;
    for _ in 0..32 {
        match cur.get("$ref").and_then(Value::as_str) {
            Some(r) => cur = resolve_ref(doc, local_root, r)?,
            None => return Ok(cur),
        }
    }
    bail!("$ref chain too deep")
}

pub struct RawOp<'a> {
    pub method: String,
    pub path: String,
    pub op: &'a Value,
    /// Path-level parameters overridden by operation-level ones (same `name` + `in`).
    pub params: Vec<&'a Value>,
}

/// All operations in document order.
pub fn operations(doc: &Value) -> Result<Vec<RawOp<'_>>> {
    let mut out = Vec::new();
    let Some(paths) = doc.get("paths").and_then(Value::as_object) else {
        return Ok(out);
    };
    for (path, item) in paths {
        let item = deref(doc, doc, item)?;
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        let path_params: Vec<&Value> = item_obj
            .get("parameters")
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        for (key, op) in item_obj {
            if !HTTP_METHODS.contains(&key.as_str()) || !op.is_object() {
                continue;
            }
            let op_params: Vec<&Value> = op
                .get("parameters")
                .and_then(Value::as_array)
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            let mut params: Vec<&Value> = Vec::new();
            let ident = |p: &Value| -> Result<(String, String)> {
                let p = deref(doc, doc, p)?;
                Ok((
                    p.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    p.get("in")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ))
            };
            let op_idents: Vec<(String, String)> =
                op_params.iter().map(|p| ident(p)).collect::<Result<_>>()?;
            for p in &path_params {
                if !op_idents.contains(&ident(p)?) {
                    params.push(p);
                }
            }
            params.extend(op_params);
            out.push(RawOp {
                method: key.to_uppercase(),
                path: path.clone(),
                op,
                params,
            });
        }
    }
    Ok(out)
}

/// Media types in preference order: JSON, multipart, url-encoded form, text, anything else.
pub fn media_rank(ct: &str) -> u8 {
    let ct = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    if ct == "application/json" || ct.ends_with("+json") || ct.ends_with("/json") {
        0
    } else if ct == "multipart/form-data" {
        1
    } else if ct == "application/x-www-form-urlencoded" {
        2
    } else if ct.starts_with("text/") {
        3
    } else {
        4
    }
}

/// Pick the preferred entry of a `content` map.
pub fn pick_content(content: &Value) -> Option<(&str, &Value)> {
    let obj = content.as_object()?;
    obj.iter()
        .min_by_key(|(ct, _)| media_rank(ct))
        .map(|(ct, v)| (ct.as_str(), v))
}
