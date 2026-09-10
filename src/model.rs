//! Builds the [`Model`] (types + operations + names) from an OpenAPI document.

use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use crate::ir::*;
use crate::naming;
use crate::schema::{Converter, Ctx, Scope};
use crate::spec;

/// Names that generated code defines in `mod.rs` or that the prelude makes ambiguous.
const RESERVED_TYPE_NAMES: &[&str] = &[
    "Api",
    "ApiBuilder",
    "FilePart",
    "String",
    "Vec",
    "Option",
    "Box",
    "Result",
    "Self",
    "HashMap",
    "Value",
    "Send",
    "Sync",
    "Some",
    "None",
    "Ok",
    "Err",
    "Default",
    "Clone",
    "Copy",
    "Debug",
    "Serialize",
    "Deserialize",
    "PartialEq",
    "Eq",
    "Hash",
    "Display",
];

/// Method names on `Api` that operations must not shadow.
pub const RESERVED_API_METHODS: &[&str] = &[
    "new",
    "builder",
    "with_bearer_token",
    "client",
    "base_url",
    "bearer_token",
    "set_bearer_token",
    "request",
    "send",
    "send_json",
    "send_text",
    "tags",
    "types",
];

/// Build the IR. `dedup_types` merges structurally identical inline schemas into one type.
pub fn build(doc: &Value, dedup_types: bool) -> Result<Model> {
    let mut conv = Converter::new(doc, dedup_types);
    let doc_scope = Scope {
        local_root: doc,
        multipart: false,
    };

    // Components first so they get their declared names.
    if let Some(schemas) = doc
        .pointer("/components/schemas")
        .and_then(Value::as_object)
    {
        for name in schemas.keys() {
            let r = format!(
                "#/components/schemas/{}",
                name.replace('~', "~0").replace('/', "~1")
            );
            conv.convert_ref(&r, &Ctx::root(name), &doc_scope)
                .with_context(|| format!("components.schemas.{name}"))?;
        }
    }

    let mut operations: Vec<Operation> = Vec::new();
    let mut error_schemas: Vec<TypeRef> = Vec::new();

    for rop in spec::operations(doc)? {
        let op = rop.op.as_object().expect("operation is an object");
        let operation_id = op
            .get("operationId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!("{}_{}", rop.method.to_lowercase(), naming::snake(&rop.path))
            });
        let where_ = format!("{} {}", rop.method, rop.path);
        let op_pascal = naming::pascal(&operation_id);

        let tags = op.get("tags").and_then(Value::as_array);
        let tag = tags
            .and_then(|t| t.first())
            .and_then(Value::as_str)
            .map(str::to_string);
        if tags.is_some_and(|t| t.len() > 1) {
            log::warn!("{where_}: multiple tags, using the first one");
        }

        let mut doc_lines: Vec<String> = Vec::new();
        if let Some(s) = op.get("summary").and_then(Value::as_str) {
            doc_lines.push(s.trim().to_string());
        }
        if let Some(d) = op.get("description").and_then(Value::as_str) {
            if !doc_lines.is_empty() {
                doc_lines.push(String::new());
            }
            doc_lines.push(d.trim().to_string());
        }
        let doc_text = if doc_lines.is_empty() {
            None
        } else {
            Some(doc_lines.join("\n"))
        };

        // Parameters.
        let mut path_params: Vec<Param> = Vec::new();
        let mut query_params: Vec<Param> = Vec::new();
        for p in &rop.params {
            let p = spec::deref(doc, doc, p)?;
            let name = p
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let loc = p.get("in").and_then(Value::as_str).unwrap_or_default();
            let required =
                loc == "path" || p.get("required").and_then(Value::as_bool).unwrap_or(false);
            let empty = Value::Object(Default::default());
            let schema = match p.get("schema") {
                Some(s) => s,
                None => p
                    .get("content")
                    .and_then(spec::pick_content)
                    .and_then(|(_, c)| c.get("schema"))
                    .unwrap_or(&empty),
            };
            let ctx = Ctx::root(&operation_id).prop(&name);
            let ty = conv
                .convert(
                    schema,
                    &ctx,
                    &Scope {
                        local_root: schema,
                        multipart: false,
                    },
                )
                .with_context(|| format!("{where_}: parameter {name}"))?;
            let ty = if required { ty } else { ty.optional() };
            let param = Param {
                name: String::new(),
                json_name: name.clone(),
                ty,
                required,
                description: p
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            match loc {
                "path" => path_params.push(param),
                "query" => query_params.push(param),
                other => log::warn!(
                    "{where_}: `in: {other}` parameter `{name}` is not supported and was skipped"
                ),
            }
        }
        // Order path params as they appear in the template; synthesize missing ones.
        let mut ordered: Vec<Param> = Vec::new();
        for placeholder in template_placeholders(&rop.path) {
            match path_params.iter().position(|p| p.json_name == placeholder) {
                Some(i) => ordered.push(path_params.remove(i)),
                None => {
                    log::warn!(
                        "{where_}: path placeholder `{{{placeholder}}}` has no parameter definition, assuming string"
                    );
                    ordered.push(Param {
                        name: String::new(),
                        json_name: placeholder,
                        ty: TypeRef::String,
                        required: true,
                        description: None,
                    });
                }
            }
        }
        if !path_params.is_empty() {
            log::warn!("{where_}: path parameters not present in the template were dropped");
        }
        let path_params = ordered;

        // Request body.
        let mut body = None;
        if let Some(rb) = op.get("requestBody") {
            let rb = spec::deref(doc, doc, rb)?;
            match rb.get("content").and_then(spec::pick_content) {
                Some((ct, media)) => {
                    let empty = Value::Object(Default::default());
                    let schema = media.get("schema").unwrap_or(&empty);
                    let ctx = Ctx::root(&format!("{op_pascal}Body"));
                    body = Some(match spec::media_rank(ct) {
                        0 => Body::Json(conv.convert(
                            schema,
                            &ctx,
                            &Scope {
                                local_root: schema,
                                multipart: false,
                            },
                        )?),
                        1 => {
                            let t = conv.convert(
                                schema,
                                &ctx,
                                &Scope {
                                    local_root: schema,
                                    multipart: true,
                                },
                            )?;
                            match conv.resolve(&t) {
                                TypeRef::Named(id)
                                    if matches!(
                                        conv.types[id].def,
                                        TypeDef::Struct {
                                            multipart: true,
                                            ..
                                        }
                                    ) =>
                                {
                                    Body::Multipart(id)
                                }
                                _ => bail!(
                                    "{where_}: multipart/form-data body must be an object schema with properties"
                                ),
                            }
                        }
                        2 => Body::Form(conv.convert(
                            schema,
                            &ctx,
                            &Scope {
                                local_root: schema,
                                multipart: false,
                            },
                        )?),
                        _ => Body::Raw,
                    });
                }
                None => body = Some(Body::Raw),
            }
        }

        // Responses.
        let mut response = Response::Raw;
        if let Some(responses) = op.get("responses").and_then(Value::as_object) {
            let mut codes: Vec<&String> = responses
                .keys()
                .filter(|c| {
                    c.len() == 3
                        && c.starts_with('2')
                        && c[1..].chars().all(|ch| ch.is_ascii_digit())
                })
                .collect();
            codes.sort();
            for wildcard in ["2XX", "2xx", "default"] {
                if let Some(k) = responses.keys().find(|k| k.as_str() == wildcard) {
                    codes.push(k);
                }
            }
            if let Some(code) = codes.first() {
                let resp = spec::deref(doc, doc, &responses[code.as_str()])?;
                if let Some((ct, media)) = resp.get("content").and_then(spec::pick_content) {
                    let ctx = Ctx::root(&format!("{op_pascal}Response"));
                    match spec::media_rank(ct) {
                        0 => {
                            let empty = Value::Object(Default::default());
                            let schema = media.get("schema").unwrap_or(&empty);
                            response = Response::Json(conv.convert(
                                schema,
                                &ctx,
                                &Scope {
                                    local_root: schema,
                                    multipart: false,
                                },
                            )?);
                        }
                        3 => response = Response::Text,
                        _ => response = Response::Raw,
                    }
                }
            }
            for (code, resp) in responses {
                if code.starts_with('2') || code == "default" {
                    continue;
                }
                let resp = spec::deref(doc, doc, resp)?;
                if let Some((ct, media)) = resp.get("content").and_then(spec::pick_content)
                    && spec::media_rank(ct) == 0
                    && let Some(schema) = media.get("schema")
                {
                    let ctx = Ctx::root(&format!("{op_pascal}Error{code}"));
                    let t = conv.convert(
                        schema,
                        &ctx,
                        &Scope {
                            local_root: schema,
                            multipart: false,
                        },
                    )?;
                    error_schemas.push(conv.resolve(&t));
                }
            }
        }

        operations.push(Operation {
            operation_id,
            name: String::new(),
            tag,
            method: rop.method.clone(),
            path: rop.path.clone(),
            path_params,
            query_params,
            body,
            response,
            doc: doc_text,
        });
    }

    // Compare structurally so the typed error payload survives `dedup_types = false`, where an
    // inline copy of the error schema is a distinct (but wire-compatible) type.
    let error_type = match error_schemas.first() {
        Some(first)
            if matches!(first, TypeRef::Named(_))
                && error_schemas
                    .iter()
                    .all(|t| conv.structural_key(t) == conv.structural_key(first)) =>
        {
            first.clone()
        }
        Some(_) => {
            log::warn!(
                "error responses use several schemas; the typed error payload is serde_json::Value"
            );
            TypeRef::Any
        }
        None => TypeRef::Any,
    };

    let default_base_url = doc
        .pointer("/servers/0/url")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(str::to_string);

    let mut model = Model {
        title: doc
            .pointer("/info/title")
            .and_then(Value::as_str)
            .unwrap_or("API")
            .to_string(),
        version: doc
            .pointer("/info/version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        types: conv.types,
        operations,
        tags: Vec::new(),
        untagged: Vec::new(),
        error_type,
        default_base_url,
    };
    assign_names(&mut model);
    Ok(model)
}

fn template_placeholders(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

fn assign_names(model: &mut Model) {
    // Types.
    let live: Vec<usize> = (0..model.types.len())
        .filter(|&i| model.types[i].is_live())
        .collect();
    let cands: Vec<Vec<String>> = live
        .iter()
        .map(|&i| model.types[i].candidates.clone())
        .collect();
    let reserved: HashSet<String> = RESERVED_TYPE_NAMES.iter().map(|s| s.to_string()).collect();
    let names = naming::resolve_unique(&cands, &reserved);
    for (&i, name) in live.iter().zip(names) {
        model.types[i].name = name;
    }

    // Tags (in order of first appearance).
    let mut tag_order: Vec<String> = Vec::new();
    for op in &model.operations {
        if let Some(t) = &op.tag
            && !tag_order.contains(t)
        {
            tag_order.push(t.clone());
        }
    }
    let mut api_taken: HashSet<String> =
        RESERVED_API_METHODS.iter().map(|s| s.to_string()).collect();
    let mut struct_taken: HashSet<String> = reserved.clone();
    let mut tags: Vec<Tag> = Vec::new();
    for raw in tag_order {
        let base = naming::ident(&raw);
        let accessor = if api_taken.contains(&base) {
            naming::unique(
                &format!("{}_api", naming::strip_raw(&base)),
                "_",
                &mut api_taken,
            )
        } else {
            naming::unique(&base, "_", &mut api_taken)
        };
        let struct_name = naming::unique(
            &format!("{}Api", naming::type_ident(&raw)),
            "",
            &mut struct_taken,
        );
        tags.push(Tag {
            raw,
            module: naming::strip_raw(&accessor).to_string(),
            accessor,
            struct_name,
            ops: Vec::new(),
        });
    }

    // Operation method names: untagged ones live on `Api` and yield to tag accessors.
    let mut per_tag_taken: HashMap<String, HashSet<String>> = HashMap::new();
    for (i, op) in model.operations.iter_mut().enumerate() {
        let base = naming::ident(&op.operation_id);
        match &op.tag {
            Some(t) => {
                let taken = per_tag_taken.entry(t.clone()).or_default();
                op.name = naming::unique(&base, "_", taken);
                tags.iter_mut()
                    .find(|tag| &tag.raw == t)
                    .expect("tag exists")
                    .ops
                    .push(i);
            }
            None => {
                op.name = if api_taken.contains(&base) {
                    naming::unique(
                        &format!("{}_op", naming::strip_raw(&base)),
                        "_",
                        &mut api_taken,
                    )
                } else {
                    naming::unique(&base, "_", &mut api_taken)
                };
                model.untagged.push(i);
            }
        }
        // Argument names.
        let mut arg_taken: HashSet<String> = ["self", "body", "req", "path", "query"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for p in op.path_params.iter_mut().chain(op.query_params.iter_mut()) {
            p.name = naming::unique(&naming::ident(&p.json_name), "_", &mut arg_taken);
        }
    }
    model.tags = tags;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn untagged_op_yields_to_tag_and_reserved_names() {
        let doc = json!({
            "openapi": "3.1.0", "info": {"title": "t", "version": "1"},
            "paths": {
                "/a": {"get": {"operationId": "datasets", "responses": {"200": {"description": "ok"}}}},
                "/b": {"get": {"operationId": "new", "responses": {"200": {"description": "ok"}}}},
                "/c/{id}": {"get": {"operationId": "get_c", "tags": ["datasets"], "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}], "responses": {"200": {"description": "ok"}}}},
                "/d": {"get": {"operationId": "get_c", "tags": ["datasets"], "responses": {"200": {"description": "ok"}}}}
            }
        });
        let m = build(&doc, false).unwrap();
        assert_eq!(m.tags.len(), 1);
        assert_eq!(m.tags[0].accessor, "datasets");
        assert_eq!(m.tags[0].struct_name, "DatasetsApi");
        assert_eq!(m.operations[0].name, "datasets_op");
        assert_eq!(m.operations[1].name, "new_op");
        assert_eq!(m.operations[2].name, "get_c");
        assert_eq!(m.operations[3].name, "get_c_2");
        assert_eq!(m.untagged, vec![0, 1]);
    }

    #[test]
    fn placeholders() {
        assert_eq!(
            template_placeholders("/{org}/datasets/{datasetId}/x"),
            vec!["org", "datasetId"]
        );
    }
}
