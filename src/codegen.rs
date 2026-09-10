//! Emits the generated Rust module from a [`Model`].

use anyhow::{Result, bail};
use std::collections::HashSet;
use std::fmt::Write as _;

use crate::ir::*;
use crate::naming;

/// Crates the generated client needs: (crate, version requirement, features).
pub const DEPENDENCIES: &[(&str, &str, &[&str])] = &[
    ("reqwest", "0.13", &["json", "multipart", "stream", "query", "form"]),
    ("serde", "1", &["derive"]),
    ("serde_json", "1", &[]),
    ("tokio", "1", &["rt-multi-thread", "macros"]),
];

/// A POSIX shell script of `cargo add` commands that installs [`DEPENDENCIES`] into the crate
/// in the current directory. Its lines can equally be pasted into a terminal one by one.
pub fn dependencies_script() -> String {
    let mut s = String::from(
        "#!/bin/sh\n\
         # Adds the dependencies of the generated API client to the crate in the current\n\
         # directory. Run it from the consuming crate's root with `sh DEPENDENCIES.sh`, or paste\n\
         # the commands into a terminal.\n\
         #\n\
         # reqwest's default features may be turned off afterwards (e.g. to switch from rustls to\n\
         # `native-tls`) as long as the features listed here stay enabled. The client is async and\n\
         # expects a tokio runtime; adjust the tokio features to what your binary needs.\n\
         set -eu\n",
    );
    for (name, version, features) in DEPENDENCIES {
        s.push_str(&format!("cargo add {name}@{version}"));
        if !features.is_empty() {
            s.push_str(&format!(" --features {}", features.join(",")));
        }
        s.push('\n');
    }
    s
}

/// Code generation switches that do not change the IR.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Derive `Default` for every struct whose fields all implement `Default` (strings,
    /// numbers, booleans, `Vec`, maps, `Option`, and other such structs), not only for structs
    /// made of `Option` fields. Enums and `FilePart` fields never qualify.
    pub derive_default_when_possible: bool,
}

/// (file name, contents) pairs to write into the output module directory.
pub fn generate(model: &Model, opts: &Options) -> Result<Vec<(String, String)>> {
    let mut files = vec![
        ("mod.rs".to_string(), gen_mod(model)?),
        ("types.rs".to_string(), gen_types(model, opts)?),
    ];
    if !model.tags.is_empty() {
        files.push(("tags/mod.rs".to_string(), gen_tags_mod(model)));
        for tag in &model.tags {
            files.push((format!("tags/{}.rs", tag.module), gen_tag(model, tag)?));
        }
    }
    Ok(files)
}

/// How to spell things from the file currently being generated.
struct Emit<'a> {
    /// Prefix for named types, e.g. `types::`.
    types: &'a str,
    /// Prefix for helpers defined in `mod.rs`, e.g. `super::`.
    root: &'a str,
    /// Expression evaluating to `&Api`.
    api: &'a str,
}

fn rust_type(model: &Model, t: &TypeRef, e: &Emit) -> String {
    match model.resolve(t) {
        TypeRef::String => "String".into(),
        TypeRef::Int => "i64".into(),
        TypeRef::Float => "f64".into(),
        TypeRef::Bool => "bool".into(),
        TypeRef::Any => "serde_json::Value".into(),
        TypeRef::Option(x) => format!("Option<{}>", rust_type(model, &x, e)),
        TypeRef::Vec(x) => format!("Vec<{}>", rust_type(model, &x, e)),
        TypeRef::Map(x) => format!(
            "std::collections::HashMap<String, {}>",
            rust_type(model, &x, e)
        ),
        TypeRef::Named(id) => format!("{}{}", e.types, model.types[id].name),
        TypeRef::Upload => format!("{}FilePart", e.root),
    }
}

fn contains_upload(model: &Model, t: &TypeRef) -> bool {
    match model.resolve(t) {
        TypeRef::Upload => true,
        TypeRef::Option(x) | TypeRef::Vec(x) | TypeRef::Map(x) => contains_upload(model, &x),
        _ => false,
    }
}

fn is_string_enum(model: &Model, t: &TypeRef) -> bool {
    matches!(model.resolve(t), TypeRef::Named(id) if matches!(model.types[id].def, TypeDef::StringEnum { .. }))
}

fn doc_comment(out: &mut String, indent: &str, text: &str) {
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            let _ = writeln!(out, "{indent}///");
        } else {
            let _ = writeln!(out, "{indent}/// {line}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// types.rs
// ---------------------------------------------------------------------------------------------

/// Whether each type gets `#[derive(Default)]`, indexed by `TypeId`.
fn default_derives(model: &Model, opts: &Options) -> Vec<bool> {
    fn ty_ok(model: &Model, t: &TypeRef, opts: &Options, memo: &mut Vec<Option<bool>>, stack: &mut Vec<TypeId>) -> bool {
        match model.resolve(t) {
            TypeRef::String | TypeRef::Int | TypeRef::Float | TypeRef::Bool | TypeRef::Any => true,
            TypeRef::Option(_) | TypeRef::Vec(_) | TypeRef::Map(_) => true,
            TypeRef::Upload => false,
            TypeRef::Named(id) => named_ok(model, id, opts, memo, stack),
        }
    }
    fn named_ok(model: &Model, id: TypeId, opts: &Options, memo: &mut Vec<Option<bool>>, stack: &mut Vec<TypeId>) -> bool {
        if let Some(v) = memo[id] {
            return v;
        }
        if stack.contains(&id) {
            // A struct that contains itself by value cannot have a finite default.
            return false;
        }
        let v = match &model.types[id].def {
            TypeDef::Struct { fields, .. } if opts.derive_default_when_possible => {
                stack.push(id);
                let ok = fields.iter().all(|f| ty_ok(model, &f.ty, opts, memo, stack));
                stack.pop();
                ok
            }
            TypeDef::Struct { fields, .. } => fields.iter().all(|f| f.ty.is_option()),
            TypeDef::StringEnum { .. } | TypeDef::Untagged { .. } => false,
        };
        memo[id] = Some(v);
        v
    }
    let mut memo = vec![None; model.types.len()];
    (0..model.types.len())
        .map(|id| named_ok(model, id, opts, &mut memo, &mut Vec::new()))
        .collect()
}

fn gen_types(model: &Model, opts: &Options) -> Result<String> {
    let defaults = default_derives(model, opts);
    let e = Emit {
        types: "",
        root: "super::",
        api: "",
    };
    let mut out = String::new();
    out.push_str(
        "//! Request and response types.\n//!\n//! Generated by openapirator. Do not edit.\n\n",
    );
    out.push_str("#![allow(dead_code, unused_imports, clippy::all)]\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");

    for (id, nt) in model.types.iter().enumerate() {
        if !nt.is_live() {
            continue;
        }
        if let Some(d) = &nt.description {
            doc_comment(&mut out, "", d);
        }
        match &nt.def {
            TypeDef::Struct {
                fields,
                extra,
                multipart,
            } => {
                if *multipart {
                    gen_multipart_struct(model, &mut out, nt, fields, defaults[id], &e)?;
                } else {
                    gen_struct(model, &mut out, id, nt, fields, extra.as_ref(), defaults[id], &e)?;
                }
            }
            TypeDef::StringEnum { values } => gen_string_enum(&mut out, nt, values),
            TypeDef::Untagged { variants } => gen_untagged(model, &mut out, id, nt, variants, &e),
        }
        out.push('\n');
    }
    Ok(out)
}

fn field_names(fields: &[Field]) -> Vec<String> {
    let mut taken: HashSet<String> = HashSet::new();
    fields
        .iter()
        .map(|f| naming::unique(&naming::ident(&f.json_name), "_", &mut taken))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn gen_struct(
    model: &Model,
    out: &mut String,
    self_id: TypeId,
    nt: &NamedType,
    fields: &[Field],
    extra: Option<&TypeRef>,
    derive_default: bool,
    e: &Emit,
) -> Result<()> {
    let names = field_names(fields);
    let mut derives = vec!["Serialize", "Deserialize", "Clone", "Debug", "PartialEq"];
    if derive_default {
        derives.push("Default");
    }
    let _ = writeln!(out, "#[derive({})]", derives.join(", "));
    let _ = writeln!(out, "pub struct {} {{", nt.name);
    for (f, name) in fields.iter().zip(&names) {
        if contains_upload(model, &f.ty) {
            bail!(
                "type {}: file uploads are only supported as direct fields of a multipart request body",
                nt.name
            );
        }
        if let Some(d) = &f.description {
            doc_comment(out, "    ", d);
        }
        let mut attrs: Vec<String> = Vec::new();
        if naming::strip_raw(name) != f.json_name {
            attrs.push(format!("rename = {:?}", f.json_name));
        }
        if f.ty.is_option() {
            attrs.push("default".into());
            attrs.push("skip_serializing_if = \"Option::is_none\"".into());
        }
        if !attrs.is_empty() {
            let _ = writeln!(out, "    #[serde({})]", attrs.join(", "));
        }
        let ty = boxed_if_recursive(model, &f.ty, self_id, e);
        let _ = writeln!(out, "    pub {name}: {ty},");
    }
    if let Some(extra) = extra {
        let mut taken: HashSet<String> = names.iter().cloned().collect();
        let name = naming::unique("additional_properties", "_", &mut taken);
        out.push_str("    /// Properties not covered by the named fields.\n");
        out.push_str("    #[serde(flatten)]\n");
        let _ = writeln!(
            out,
            "    pub {name}: std::collections::HashMap<String, {}>,",
            rust_type(model, extra, e)
        );
    }
    out.push_str("}\n");
    Ok(())
}

/// `Box<T>` when a field/variant refers directly back to the type being defined.
fn boxed_if_recursive(model: &Model, t: &TypeRef, self_id: TypeId, e: &Emit) -> String {
    let resolved = model.resolve(t);
    let direct = match &resolved {
        TypeRef::Named(id) => *id == self_id,
        TypeRef::Option(x) => matches!(model.resolve(x), TypeRef::Named(id) if id == self_id),
        _ => false,
    };
    if direct {
        match resolved {
            TypeRef::Option(x) => format!("Option<Box<{}>>", rust_type(model, &x, e)),
            other => format!("Box<{}>", rust_type(model, &other, e)),
        }
    } else {
        rust_type(model, t, e)
    }
}

fn gen_multipart_struct(
    model: &Model,
    out: &mut String,
    nt: &NamedType,
    fields: &[Field],
    derive_default: bool,
    e: &Emit,
) -> Result<()> {
    let names = field_names(fields);
    out.push_str("/// `multipart/form-data` request body. Binary fields take a [`FilePart`](super::FilePart),\n");
    out.push_str("/// which can wrap in-memory bytes, a stream, or a file on disk.\n");
    if derive_default {
        out.push_str("#[derive(Debug, Default)]\n");
    } else {
        out.push_str("#[derive(Debug)]\n");
    }
    let _ = writeln!(out, "pub struct {} {{", nt.name);
    for (f, name) in fields.iter().zip(&names) {
        if let Some(d) = &f.description {
            doc_comment(out, "    ", d);
        }
        let _ = writeln!(out, "    pub {name}: {},", rust_type(model, &f.ty, e));
    }
    out.push_str("}\n\n");
    let _ = writeln!(out, "impl {} {{", nt.name);
    out.push_str("    /// Build the multipart form. Array fields are repeated once per element.\n");
    out.push_str("    pub fn into_form(self) -> reqwest::multipart::Form {\n");
    out.push_str("        let mut form = reqwest::multipart::Form::new();\n");
    for (f, name) in fields.iter().zip(&names) {
        form_push(
            model,
            out,
            &format!("self.{name}"),
            &f.json_name,
            &f.ty,
            0,
            "        ",
        )?;
    }
    out.push_str("        form\n    }\n}\n");
    Ok(())
}

fn form_push(
    model: &Model,
    out: &mut String,
    var: &str,
    json_name: &str,
    ty: &TypeRef,
    depth: usize,
    indent: &str,
) -> Result<()> {
    let name = format!("{json_name:?}");
    match model.resolve(ty) {
        TypeRef::Upload => {
            let _ = writeln!(out, "{indent}form = form.part({name}, {var}.into_part());");
        }
        TypeRef::Option(inner) => {
            let v = format!("v{depth}");
            let _ = writeln!(out, "{indent}if let Some({v}) = {var} {{");
            form_push(
                model,
                out,
                &v,
                json_name,
                &inner,
                depth + 1,
                &format!("{indent}    "),
            )?;
            let _ = writeln!(out, "{indent}}}");
        }
        TypeRef::Vec(inner) => {
            let v = format!("v{depth}");
            let _ = writeln!(out, "{indent}for {v} in {var} {{");
            form_push(
                model,
                out,
                &v,
                json_name,
                &inner,
                depth + 1,
                &format!("{indent}    "),
            )?;
            let _ = writeln!(out, "{indent}}}");
        }
        TypeRef::String => {
            let _ = writeln!(out, "{indent}form = form.text({name}, {var});");
        }
        TypeRef::Int | TypeRef::Float | TypeRef::Bool => {
            let _ = writeln!(out, "{indent}form = form.text({name}, {var}.to_string());");
        }
        t if is_string_enum(model, &t) => {
            let _ = writeln!(out, "{indent}form = form.text({name}, {var}.as_str());");
        }
        _ => {
            let _ = writeln!(
                out,
                "{indent}form = form.text({name}, serde_json::to_string(&{var}).unwrap_or_default());"
            );
        }
    }
    Ok(())
}

fn gen_string_enum(out: &mut String, nt: &NamedType, values: &[String]) {
    let mut taken: HashSet<String> = HashSet::new();
    let variants: Vec<String> = values
        .iter()
        .map(|v| naming::unique(&naming::variant_name(v), "", &mut taken))
        .collect();
    out.push_str("#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]\n");
    let _ = writeln!(out, "pub enum {} {{", nt.name);
    for (v, name) in values.iter().zip(&variants) {
        let _ = writeln!(out, "    #[serde(rename = {v:?})]\n    {name},");
    }
    out.push_str("}\n\n");
    let _ = writeln!(out, "impl {} {{", nt.name);
    out.push_str("    pub fn as_str(&self) -> &'static str {\n        match self {\n");
    for (v, name) in values.iter().zip(&variants) {
        let _ = writeln!(out, "            Self::{name} => {v:?},");
    }
    out.push_str("        }\n    }\n}\n\n");
    let _ = writeln!(out, "impl std::fmt::Display for {} {{", nt.name);
    out.push_str("    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n        f.write_str(self.as_str())\n    }\n}\n");
}

fn variant_base_name(model: &Model, t: &TypeRef) -> String {
    match model.resolve(t) {
        TypeRef::String => "String".into(),
        TypeRef::Int => "Integer".into(),
        TypeRef::Float => "Number".into(),
        TypeRef::Bool => "Boolean".into(),
        TypeRef::Any => "Any".into(),
        TypeRef::Upload => "File".into(),
        TypeRef::Option(x) => format!("Optional{}", variant_base_name(model, &x)),
        TypeRef::Vec(_) => "Array".into(),
        TypeRef::Map(_) => "Object".into(),
        TypeRef::Named(id) => model.types[id].name.clone(),
    }
}

fn gen_untagged(
    model: &Model,
    out: &mut String,
    self_id: TypeId,
    nt: &NamedType,
    variants: &[TypeRef],
    e: &Emit,
) {
    let mut taken: HashSet<String> = HashSet::new();
    let names: Vec<String> = variants
        .iter()
        .map(|v| naming::unique(&variant_base_name(model, v), "", &mut taken))
        .collect();
    out.push_str(
        "#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]\n#[serde(untagged)]\n",
    );
    let _ = writeln!(out, "pub enum {} {{", nt.name);
    for (v, name) in variants.iter().zip(&names) {
        let _ = writeln!(
            out,
            "    {name}({}),",
            boxed_if_recursive(model, v, self_id, e)
        );
    }
    out.push_str("}\n");
}

// ---------------------------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------------------------

/// Builds the expression converting an argument variable into its wire `String`.
type ToWire = Box<dyn Fn(&str) -> String>;

/// Rust argument type and an expression converting `var` into the wire `String`.
fn param_arg(model: &Model, t: &TypeRef, e: &Emit) -> (String, ToWire) {
    match model.resolve(t) {
        TypeRef::String => ("&str".into(), Box::new(|v| format!("{v}.to_string()"))),
        TypeRef::Int => ("i64".into(), Box::new(|v| format!("{v}.to_string()"))),
        TypeRef::Float => ("f64".into(), Box::new(|v| format!("{v}.to_string()"))),
        TypeRef::Bool => ("bool".into(), Box::new(|v| format!("{v}.to_string()"))),
        t if is_string_enum(model, &t) => (
            rust_type(model, &t, e),
            Box::new(|v| format!("{v}.as_str().to_string()")),
        ),
        TypeRef::Option(inner) => {
            let (inner_ty, _) = param_arg(model, &inner, e);
            (format!("Option<{inner_ty}>"), Box::new(|v| v.to_string()))
        }
        TypeRef::Vec(inner) => {
            let owned = match model.resolve(&inner) {
                TypeRef::String => "String".to_string(),
                other => param_arg(model, &other, e).0,
            };
            (format!("&[{owned}]"), Box::new(|v| v.to_string()))
        }
        _ => ("&str".into(), Box::new(|v| format!("{v}.to_string()"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn query_push(
    model: &Model,
    out: &mut String,
    var: &str,
    json_name: &str,
    t: &TypeRef,
    depth: usize,
    indent: &str,
    e: &Emit,
) {
    match model.resolve(t) {
        TypeRef::Option(inner) => {
            let v = format!("v{depth}");
            let _ = writeln!(out, "{indent}if let Some({v}) = {var} {{");
            query_push(
                model,
                out,
                &v,
                json_name,
                &inner,
                depth + 1,
                &format!("{indent}    "),
                e,
            );
            let _ = writeln!(out, "{indent}}}");
        }
        TypeRef::Vec(inner) => {
            let v = format!("v{depth}");
            let _ = writeln!(out, "{indent}for {v} in {var} {{");
            query_push(
                model,
                out,
                &v,
                json_name,
                &inner,
                depth + 1,
                &format!("{indent}    "),
                e,
            );
            let _ = writeln!(out, "{indent}}}");
        }
        other => {
            let (_, to_string) = param_arg(model, &other, e);
            let _ = writeln!(
                out,
                "{indent}query.push(({json_name:?}, {}));",
                to_string(var)
            );
        }
    }
}

fn gen_operation(model: &Model, out: &mut String, op: &Operation, e: &Emit) -> Result<()> {
    let indent = "    ";
    let mut doc = op.doc.clone().unwrap_or_default();
    if !doc.is_empty() {
        doc.push_str("\n\n");
    }
    let _ = write!(doc, "`{} {}`", op.method, op.path);
    let documented: Vec<&Param> = op
        .path_params
        .iter()
        .chain(&op.query_params)
        .filter(|p| p.description.is_some())
        .collect();
    if !documented.is_empty() {
        doc.push_str("\n\n# Arguments\n");
        for p in documented {
            let _ = write!(
                doc,
                "\n* `{}`{}: {}",
                naming::strip_raw(&p.name),
                if p.required { "" } else { " (optional)" },
                p.description
                    .as_deref()
                    .unwrap_or_default()
                    .replace('\n', " ")
            );
        }
    }
    doc_comment(out, indent, &doc);

    let mut args: Vec<String> = vec!["&self".into()];
    for p in &op.path_params {
        args.push(format!("{}: {}", p.name, param_arg(model, &p.ty, e).0));
    }
    for p in &op.query_params {
        args.push(format!("{}: {}", p.name, param_arg(model, &p.ty, e).0));
    }
    match &op.body {
        Some(Body::Json(t)) | Some(Body::Form(t)) => {
            args.push(format!("body: &{}", rust_type(model, t, e)))
        }
        Some(Body::Multipart(id)) => {
            args.push(format!("body: {}{}", e.types, model.types[*id].name))
        }
        Some(Body::Raw) => args.push("body: impl Into<reqwest::Body>".into()),
        None => {}
    }
    let ret = match &op.response {
        Response::Json(t) => rust_type(model, t, e),
        Response::Text => "String".into(),
        Response::Raw => "reqwest::Response".into(),
    };
    let _ = writeln!(
        out,
        "{indent}pub async fn {}({}) -> Result<{ret}, {}Error> {{",
        op.name,
        args.join(", "),
        e.root
    );

    // Path.
    let mut fmt = String::new();
    let mut fmt_args: Vec<String> = Vec::new();
    let mut rest = op.path.as_str();
    let mut idx = 0;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        fmt.push_str(&rest[..start].replace('{', "{{").replace('}', "}}"));
        fmt.push_str("{}");
        let p = &op.path_params[idx];
        idx += 1;
        let expr = match model.resolve(&p.ty) {
            TypeRef::String => format!("{}encode_path({})", e.root, p.name),
            other => format!(
                "{}encode_path(&{})",
                e.root,
                param_arg(model, &other, e).1(&p.name)
            ),
        };
        fmt_args.push(expr);
        rest = &after[end + 1..];
    }
    fmt.push_str(&rest.replace('{', "{{").replace('}', "}}"));
    if fmt_args.is_empty() {
        let _ = writeln!(out, "{indent}    let path = {fmt:?};");
    } else {
        let _ = writeln!(
            out,
            "{indent}    let path = format!({fmt:?}, {});",
            fmt_args.join(", ")
        );
    }
    let mutable = if op.query_params.is_empty() && op.body.is_none() {
        ""
    } else {
        "mut "
    };
    let _ = writeln!(
        out,
        "{indent}    let {mutable}req = {}.request(reqwest::Method::{}, &path);",
        e.api, op.method
    );

    if !op.query_params.is_empty() {
        let _ = writeln!(
            out,
            "{indent}    let mut query: Vec<(&str, String)> = Vec::new();"
        );
        for p in &op.query_params {
            query_push(
                model,
                out,
                &p.name,
                &p.json_name,
                &p.ty,
                0,
                &format!("{indent}    "),
                e,
            );
        }
        let _ = writeln!(out, "{indent}    req = req.query(&query);");
    }
    match &op.body {
        Some(Body::Json(_)) => {
            let _ = writeln!(out, "{indent}    req = req.json(body);");
        }
        Some(Body::Form(_)) => {
            let _ = writeln!(out, "{indent}    req = req.form(body);");
        }
        Some(Body::Multipart(_)) => {
            let _ = writeln!(out, "{indent}    req = req.multipart(body.into_form());");
        }
        Some(Body::Raw) => {
            let _ = writeln!(out, "{indent}    req = req.body(body);");
        }
        None => {}
    }
    let send = match &op.response {
        Response::Json(_) => "send_json",
        Response::Text => "send_text",
        Response::Raw => "send",
    };
    let _ = writeln!(out, "{indent}    {}.{send}(req).await", e.api);
    let _ = writeln!(out, "{indent}}}\n");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// {tag}.rs
// ---------------------------------------------------------------------------------------------

/// `tags/mod.rs`: declares one submodule per tag and re-exports the group structs.
fn gen_tags_mod(model: &Model) -> String {
    let mut out = String::new();
    out.push_str("//! Operation groups, one module per OpenAPI tag. Obtain a group through the accessor of the\n");
    out.push_str(
        "//! same name on [`Api`](super::Api), e.g. `api.<tag>().<operation>(..)`.\n//!\n",
    );
    out.push_str("//! Generated by openapirator. Do not edit.\n\n");
    for tag in &model.tags {
        let _ = writeln!(out, "pub mod {};", tag.accessor);
    }
    out.push('\n');
    for tag in &model.tags {
        let _ = writeln!(out, "pub use {}::{};", tag.accessor, tag.struct_name);
    }
    out
}

fn gen_tag(model: &Model, tag: &Tag) -> Result<String> {
    let e = Emit {
        types: "types::",
        root: "super::super::",
        api: "self.api",
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "//! Operations tagged `{}`.\n//!\n//! Generated by openapirator. Do not edit.\n",
        tag.raw
    );
    out.push_str("#![allow(dead_code, unused_imports, unused_mut, clippy::all)]\n\n");
    out.push_str("use super::super::{types, Api, Error, FilePart};\n\n");
    let _ = writeln!(
        out,
        "/// Operations tagged `{}`. Obtain via [`Api::{}`].",
        tag.raw,
        naming::strip_raw(&tag.accessor)
    );
    out.push_str("#[derive(Clone, Copy, Debug)]\n");
    let _ = writeln!(
        out,
        "pub struct {}<'a> {{\n    pub(in super::super) api: &'a Api,\n}}\n",
        tag.struct_name
    );
    let _ = writeln!(out, "impl<'a> {}<'a> {{", tag.struct_name);
    for &i in &tag.ops {
        gen_operation(model, &mut out, &model.operations[i], &e)?;
    }
    out.push_str("}\n");
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// mod.rs
// ---------------------------------------------------------------------------------------------

fn gen_mod(model: &Model) -> Result<String> {
    let e = Emit {
        types: "types::",
        root: "",
        api: "self",
    };
    let mut out = format!(
        r#"//! Client for `{}` {}.
//!
//! This file is automatically @generated by {} {}.
//! It is not intended for manual editing.

#![allow(dead_code, unused_imports, unused_mut, clippy::all)]

use std::borrow::Cow;
use std::path::Path;

pub mod types;
"#,
        model.title,
        model.version,
        env!("CARGO_CRATE_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
    if !model.tags.is_empty() {
        out.push_str("pub mod tags;\n");
    }
    out.push('\n');
    let error_payload = rust_type(model, &model.error_type, &e);
    let _ = write!(
        out,
        r#"#[derive(Debug)]
pub struct MissingBaseUrl;

impl std::fmt::Display for MissingBaseUrl {{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{
        f.write_str("no base URL configured and the API specification declares no servers")
    }}
}}

impl std::error::Error for MissingBaseUrl {{}}

/// Error returned by every operation.
#[derive(Debug)]
pub enum Error {{
    /// Errors returned by reqwest.
    Reqwest(reqwest::Error),
    /// Non-2xx status responses.
    Api {{
        status: reqwest::StatusCode,
        /// Raw response body.
        body: String,
        /// The body decoded as the API's error schema, when it parsed.
        error: Option<{error_payload}>,
    }},
}}

impl std::fmt::Display for Error {{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{
        match self {{
            Error::Reqwest(e) => write!(f, "request failed: {{e}}"),
            Error::Api {{ status, body, .. }} => write!(f, "api error {{status}}: {{body}}"),
        }}
    }}
}}

impl std::error::Error for Error {{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {{
        match self {{
            Error::Reqwest(e) => Some(e),
            Error::Api {{ .. }} => None,
        }}
    }}
}}

impl From<reqwest::Error> for Error {{
    fn from(e: reqwest::Error) -> Self {{
        Error::Reqwest(e)
    }}
}}

/// A file to upload in a `multipart/form-data` request.
///
/// Wraps [`reqwest::multipart::Part`]. Construct it from in-memory bytes, from anything that
/// converts into a [`reqwest::Body`] (including a stream via [`reqwest::Body::wrap_stream`]), or
/// directly from a file on disk, which is streamed rather than read into memory:
///
/// ```ignore
/// let a = FilePart::bytes("a.txt", b"hello".to_vec());
/// let b = FilePart::file("/data/big.parquet").await?;
/// let stream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from("chunk"))]);
/// let c = FilePart::body("c.bin", reqwest::Body::wrap_stream(stream));
/// ```
#[derive(Debug)]
pub struct FilePart {{
    part: reqwest::multipart::Part,
}}

impl FilePart {{
    /// A part backed by an in-memory buffer.
    pub fn bytes(file_name: impl Into<Cow<'static, str>>, data: impl Into<Cow<'static, [u8]>>) -> Self {{
        FilePart {{ part: reqwest::multipart::Part::bytes(data).file_name(file_name) }}
    }}

    /// A part backed by any [`reqwest::Body`]: `Vec<u8>`, `bytes::Bytes`, `String`, or a stream
    /// wrapped with [`reqwest::Body::wrap_stream`].
    pub fn body(file_name: impl Into<Cow<'static, str>>, body: impl Into<reqwest::Body>) -> Self {{
        FilePart {{ part: reqwest::multipart::Part::stream(body).file_name(file_name) }}
    }}

    /// Like [`FilePart::body`] with a known content length, which lets reqwest send a
    /// `Content-Length` header instead of a chunked body.
    pub fn body_with_length(file_name: impl Into<Cow<'static, str>>, body: impl Into<reqwest::Body>, length: u64) -> Self {{
        FilePart {{ part: reqwest::multipart::Part::stream_with_length(body, length).file_name(file_name) }}
    }}

    /// A part streamed from a file on disk. The file name and MIME type are derived from the path.
    pub async fn file(path: impl AsRef<Path>) -> std::io::Result<Self> {{
        Ok(FilePart {{ part: reqwest::multipart::Part::file(path).await? }})
    }}

    /// Wrap an already-built [`reqwest::multipart::Part`].
    pub fn from_part(part: reqwest::multipart::Part) -> Self {{
        FilePart {{ part }}
    }}

    /// Override the part's MIME type.
    pub fn mime_str(self, mime: &str) -> Result<Self, Error> {{
        Ok(FilePart {{ part: self.part.mime_str(mime)? }})
    }}

    /// Override the part's file name.
    pub fn file_name(self, file_name: impl Into<Cow<'static, str>>) -> Self {{
        FilePart {{ part: self.part.file_name(file_name) }}
    }}

    pub fn into_part(self) -> reqwest::multipart::Part {{
        self.part
    }}
}}

impl From<reqwest::multipart::Part> for FilePart {{
    fn from(part: reqwest::multipart::Part) -> Self {{
        FilePart {{ part }}
    }}
}}

/// Percent-encode a path segment (everything outside RFC 3986 unreserved characters).
fn encode_path(s: &str) -> String {{
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {{
        match b {{
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{{b:02X}}")),
        }}
    }}
    out
}}

"#
    );

    let (base_url_doc, base_url_fallback) = match &model.default_base_url {
        Some(url) => {
            let _ = writeln!(
                out,
                "/// First `servers` entry of the specification.\npub const DEFAULT_BASE_URL: &str = {url:?};\n"
            );
            (
                "falls back to [`DEFAULT_BASE_URL`]",
                "DEFAULT_BASE_URL.to_string()",
            )
        }
        None => (
            "must be set, because the specification declares no `servers`",
            "return Err(MissingBaseUrl)",
        ),
    };

    let _ = write!(
        out,
        r#"/// The API client. Owns a [`reqwest::Client`], the base URL, and an optional bearer token
/// that is attached to every request as `Authorization: Bearer ...`.
///
/// Build one with [`Api::new`] or [`Api::builder`].
#[derive(Clone, Debug)]
pub struct Api {{
    client: reqwest::Client,
    base_url: String,
    bearer_token: Option<String>,
}}

/// Builder for [`Api`]. Every setting is optional: the [`reqwest::Client`] defaults to
/// `reqwest::Client::new()`, the base URL {base_url_doc}, and without a bearer token no
/// `Authorization` header is sent.
#[derive(Clone, Debug, Default)]
pub struct ApiBuilder {{
    client: Option<reqwest::Client>,
    base_url: Option<String>,
    bearer_token: Option<String>,
}}

impl ApiBuilder {{
    /// Use a custom [`reqwest::Client`] (timeouts, proxies, ...).
    pub fn client(mut self, client: reqwest::Client) -> Self {{
        self.client = Some(client);
        self
    }}

    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {{
        self.base_url = Some(base_url.into());
        self
    }}

    pub fn bearer_token(mut self, bearer_token: impl Into<String>) -> Self {{
        self.bearer_token = Some(bearer_token.into());
        self
    }}

    pub fn build(self) -> Result<Api, MissingBaseUrl> {{
        let base_url = match self.base_url {{
            Some(url) => url,
            None => {base_url_fallback},
        }};
        Ok(Api {{ client: self.client.unwrap_or_default(), base_url, bearer_token: self.bearer_token }})
    }}
}}

impl Api {{
    pub fn builder() -> ApiBuilder {{
        ApiBuilder::default()
    }}

    /// A client for `base_url` with a default [`reqwest::Client`] and no bearer token.
    pub fn new(base_url: impl Into<String>) -> Self {{
        Api {{ client: reqwest::Client::new(), base_url: base_url.into(), bearer_token: None }}
    }}

    /// Attach a bearer token.
    pub fn with_bearer_token(mut self, bearer_token: impl Into<String>) -> Self {{
        self.bearer_token = Some(bearer_token.into());
        self
    }}

    pub fn client(&self) -> &reqwest::Client {{
        &self.client
    }}

    pub fn base_url(&self) -> &str {{
        &self.base_url
    }}

    pub fn bearer_token(&self) -> Option<&str> {{
        self.bearer_token.as_deref()
    }}

    /// Replace the bearer token, or remove it with `None`.
    pub fn set_bearer_token(&mut self, bearer_token: Option<String>) {{
        self.bearer_token = bearer_token;
    }}

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {{
        let url = format!("{{}}{{}}", self.base_url.trim_end_matches('/'), path);
        let req = self.client.request(method, url);
        match &self.bearer_token {{
            Some(token) => req.bearer_auth(token),
            None => req,
        }}
    }}
"#
    );
    if model.default_base_url.is_some() {
        out.push_str(
            r#"}

impl Default for Api {
    fn default() -> Self {
        Api::new(DEFAULT_BASE_URL)
    }
}

impl Api {
"#,
        );
    }

    out.push_str(
        r#"
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, Error> {
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let error = serde_json::from_str(&body).ok();
        Err(Error::Api { status, body, error })
    }

    async fn send_json<T: serde::de::DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, Error> {
        Ok(self.send(req).await?.json().await?)
    }

    async fn send_text(&self, req: reqwest::RequestBuilder) -> Result<String, Error> {
        Ok(self.send(req).await?.text().await?)
    }

"#,
    );
    for tag in &model.tags {
        let _ = writeln!(out, "    /// Operations tagged `{}`.", tag.raw);
        let _ = writeln!(
            out,
            "    pub fn {}(&self) -> tags::{}::{}<'_> {{\n        tags::{}::{} {{ api: self }}\n    }}\n",
            tag.accessor, tag.accessor, tag.struct_name, tag.accessor, tag.struct_name
        );
    }
    for &i in &model.untagged {
        gen_operation(model, &mut out, &model.operations[i], &e)?;
    }
    out.push_str("}\n");
    Ok(out)
}

#[cfg(test)]
mod tests {

    use super::*;

    fn mod_rs_for(servers: serde_json::Value) -> String {
        let doc = serde_json::json!({
            "openapi": "3.1.0", "info": {"title": "t", "version": "1"}, "servers": servers,
            "paths": {"/ping": {"get": {"operationId": "ping", "responses": {"200": {"description": "ok"}}}}}
        });
        let model = crate::model::build(&doc, false).unwrap();
        gen_mod(&model).unwrap()
    }

    #[test]
    fn base_url_falls_back_to_spec_servers() {
        let with = mod_rs_for(serde_json::json!([{"url": "https://api.example.com/v1"}]));
        assert!(
            with.contains("pub const DEFAULT_BASE_URL: &str = \"https://api.example.com/v1\";")
        );
        assert!(with.contains("None => DEFAULT_BASE_URL.to_string(),"));
        assert!(with.contains("impl Default for Api"));

        let without = mod_rs_for(serde_json::json!([]));
        assert!(!without.contains("DEFAULT_BASE_URL"));
        assert!(without.contains("None => return Err(MissingBaseUrl),"));
        assert!(!without.contains("impl Default for Api"));
    }

    #[test]
    fn default_derive_widens_with_option() {
        let doc = serde_json::json!({
            "openapi": "3.1.0", "info": {"title": "t", "version": "1"},
            "paths": {"/a": {"post": {"operationId": "a", "requestBody": {"content": {"application/json": {"schema": {
                "type": "object", "required": ["name", "tags", "kind", "nested"],
                "properties": {
                    "name": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "kind": {"type": "string", "enum": ["x", "y"]},
                    "nested": {"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}
                }}}}}, "responses": {"200": {"description": "ok"}}}}}
        });
        let model = crate::model::build(&doc, false).unwrap();
        let derive_of = |src: &str, name: &str| {
            let at = src.find(&format!("pub struct {name} ")).unwrap_or_else(|| panic!("{name}"));
            src[..at].rsplit("#[derive(").next().unwrap().split(')').next().unwrap().to_string()
        };
        let plain = gen_types(&model, &Options::default()).unwrap();
        assert!(!derive_of(&plain, "ABody").contains("Default"));
        assert!(!derive_of(&plain, "Nested").contains("Default"));

        let wide = gen_types(&model, &Options { derive_default_when_possible: true }).unwrap();
        // `kind` is an enum, so the body still cannot be Default; the nested struct can.
        assert!(!derive_of(&wide, "ABody").contains("Default"));
        assert!(derive_of(&wide, "Nested").contains("Default"));
    }

    #[test]
    fn dependencies_script_is_one_cargo_add_per_crate() {
        let script = dependencies_script();
        assert!(script.starts_with("#!/bin/sh\n"));
        let commands: Vec<&str> =
            script.lines().filter(|l| !l.starts_with('#') && !l.starts_with("set ")).collect();
        assert_eq!(commands.len(), DEPENDENCIES.len());
        for (cmd, (name, version, _)) in commands.iter().zip(DEPENDENCIES) {
            assert!(cmd.starts_with(&format!("cargo add {name}@{version}")), "{cmd}");
        }
        assert!(script.contains("cargo add reqwest@0.13 --features json,multipart,stream,query,form\n"));
        assert!(script.contains("cargo add serde_json@1\n"));
    }
}
