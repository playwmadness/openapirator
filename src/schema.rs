//! JSON Schema (OpenAPI 3.1 dialect) → IR conversion with structural de-duplication.

use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

use crate::ir::{Field, NamedType, TypeDef, TypeId, TypeRef, resolve_in};
use crate::naming;
use crate::spec;

/// Naming context: the chain of PascalCase segments leading to the current schema.
#[derive(Clone, Debug)]
pub struct Ctx {
    chain: Vec<String>,
    last_is_prop: bool,
}

impl Ctx {
    pub fn root(name: &str) -> Ctx {
        Ctx { chain: vec![naming::pascal(name)], last_is_prop: false }
    }

    pub fn prop(&self, name: &str) -> Ctx {
        let mut c = self.clone();
        c.chain.push(naming::pascal(name));
        c.last_is_prop = true;
        c
    }

    pub fn items(&self) -> Ctx {
        let mut c = self.clone();
        let last = c.chain.last_mut().expect("non-empty chain");
        if c.last_is_prop {
            *last = naming::pascal(&naming::singular(last));
        } else {
            last.push_str("Item");
        }
        c
    }

    pub fn values(&self) -> Ctx {
        let mut c = self.clone();
        c.chain.push("Value".to_string());
        c.last_is_prop = false;
        c
    }

    pub fn variant(&self, i: usize) -> Ctx {
        let mut c = self.clone();
        c.chain.push(format!("Variant{i}"));
        c.last_is_prop = false;
        c
    }

    /// Context for a `#/$defs/<name>` entry of the root schema this context belongs to.
    pub fn def(&self, name: &str) -> Ctx {
        Ctx { chain: vec![self.chain[0].clone(), naming::pascal(name)], last_is_prop: false }
    }

    /// Candidate type names, shortest (most local) first.
    pub fn candidates(&self) -> Vec<String> {
        let n = self.chain.len();
        let mut out: Vec<String> = Vec::new();
        for k in 1..=n {
            let cand = naming::escape_type_keyword(self.chain[n - k..].concat());
            if out.last() != Some(&cand) {
                out.push(cand);
            }
        }
        out
    }
}

impl std::fmt::Display for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.chain.join("."))
    }
}

/// Per-media-type conversion scope.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    /// Root schema of the media type (owner of `$defs`).
    pub local_root: &'a Value,
    /// True inside a `multipart/form-data` body: `format: binary` strings become file parts.
    pub multipart: bool,
}

const ANNOTATION_KEYS: &[&str] = &[
    "description", "title", "contentMediaType", "contentEncoding", "example", "examples", "default",
    "deprecated", "$comment", "readOnly", "writeOnly", "format", "$schema", "externalDocs", "xml",
];

fn annotation_only(v: &Value) -> bool {
    match v {
        Value::Bool(true) => true,
        Value::Object(o) => o.keys().all(|k| ANNOTATION_KEYS.contains(&k.as_str()) || k.starts_with("x-")),
        _ => false,
    }
}

fn is_null_schema(v: &Value) -> bool {
    let Some(o) = v.as_object() else { return false };
    let ty_is_null = match o.get("type") {
        Some(Value::String(s)) => s == "null",
        Some(Value::Array(a)) => a.len() == 1 && a[0] == "null",
        _ => false,
    };
    ty_is_null && o.keys().all(|k| k == "type" || ANNOTATION_KEYS.contains(&k.as_str()))
}

pub struct Converter<'a> {
    doc: &'a Value,
    pub types: Vec<NamedType>,
    keys: HashMap<String, TypeId>,
    ref_cache: HashMap<String, TypeRef>,
}

impl<'a> Converter<'a> {
    pub fn new(doc: &'a Value) -> Self {
        Converter { doc, types: Vec::new(), keys: HashMap::new(), ref_cache: HashMap::new() }
    }

    pub fn resolve(&self, t: &TypeRef) -> TypeRef {
        resolve_in(&self.types, t)
    }

    pub fn convert(&mut self, schema: &Value, ctx: &Ctx, scope: &Scope) -> Result<TypeRef> {
        self.convert_slot(schema, ctx, scope, None)
    }

    /// Convert a `$ref` string (used for `components.schemas` entries).
    pub fn convert_ref(&mut self, r: &str, ctx: &Ctx, scope: &Scope) -> Result<TypeRef> {
        let is_component = r.starts_with("#/components/schemas/");
        let key = if is_component { r.to_string() } else { format!("{:p}|{r}", scope.local_root) };
        if let Some(t) = self.ref_cache.get(&key) {
            return Ok(t.clone());
        }
        let target = spec::resolve_ref(self.doc, scope.local_root, r)?;
        let name = spec::unescape_token(r.rsplit('/').next().unwrap_or(r));
        let pid = self.alloc_placeholder();
        self.ref_cache.insert(key.clone(), TypeRef::Named(pid));
        let ctx2 = if is_component { Ctx::root(&name) } else { ctx.def(&name) };
        let scope2 = if is_component { Scope { local_root: target, multipart: scope.multipart } } else { *scope };
        let t = self.convert_slot(target, &ctx2, &scope2, Some(pid))?;
        if self.types[pid].placeholder {
            if t == TypeRef::Named(pid) {
                bail!("unresolvable recursive $ref: {r}");
            }
            self.types[pid].alias = Some(t.clone());
        }
        self.ref_cache.insert(key, t.clone());
        Ok(t)
    }

    fn alloc_placeholder(&mut self) -> TypeId {
        self.types.push(NamedType {
            def: TypeDef::Untagged { variants: Vec::new() },
            candidates: Vec::new(),
            description: None,
            name: String::new(),
            alias: None,
            placeholder: true,
        });
        self.types.len() - 1
    }

    fn convert_slot(&mut self, schema: &Value, ctx: &Ctx, scope: &Scope, slot: Option<TypeId>) -> Result<TypeRef> {
        let obj = match schema {
            Value::Object(o) => o,
            Value::Bool(_) => return Ok(TypeRef::Any),
            _ => bail!("schema at {ctx} is not an object"),
        };
        if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
            return self.convert_ref(r, ctx, scope);
        }
        let description = obj.get("description").and_then(Value::as_str).map(str::to_string);

        let mut nullable = false;
        let mut types: Vec<String> = match obj.get("type") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        };
        if types.iter().any(|t| t == "null") {
            nullable = true;
            types.retain(|t| t != "null");
        }
        let wrap = |t: TypeRef, nullable: bool| if nullable { t.optional() } else { t };
        let has_own_shape = !types.is_empty()
            || obj.contains_key("properties")
            || obj.contains_key("items")
            || obj.contains_key("enum")
            || obj.contains_key("const");

        if let Some(all) = obj.get("allOf").and_then(Value::as_array) {
            let merged = self.merge_all_of(obj, all, scope)?;
            let t = self.convert_slot(&merged, ctx, scope, slot)?;
            return Ok(wrap(t, nullable));
        }

        let enum_values: Option<Vec<Value>> = match (obj.get("enum"), obj.get("const")) {
            (Some(Value::Array(a)), _) => Some(a.clone()),
            (None, Some(c)) => Some(vec![c.clone()]),
            _ => None,
        };
        if let Some(vals) = enum_values {
            let mut strings = Vec::new();
            let mut has_null = false;
            let mut non_string = false;
            for v in &vals {
                match v {
                    Value::String(s) => strings.push(s.clone()),
                    Value::Null => has_null = true,
                    _ => non_string = true,
                }
            }
            if non_string || strings.is_empty() {
                log::warn!("{ctx}: non-string enum values are not supported, using serde_json::Value");
                return Ok(wrap(TypeRef::Any, nullable || has_null));
            }
            let t = self.intern(TypeDef::StringEnum { values: strings }, ctx, description, slot);
            return Ok(wrap(t, nullable || has_null));
        }

        let combos = obj.get("anyOf").or_else(|| obj.get("oneOf")).and_then(Value::as_array);
        if let Some(members) = combos {
            let mut rest: Vec<&Value> = Vec::new();
            for m in members {
                if is_null_schema(m) {
                    nullable = true;
                } else {
                    rest.push(m);
                }
            }
            if has_own_shape {
                if !rest.iter().all(|m| annotation_only(m)) {
                    log::warn!("{ctx}: anyOf/oneOf next to an explicit type is ignored");
                }
            } else {
                if rest.is_empty() || rest.iter().any(|m| annotation_only(m)) {
                    return Ok(TypeRef::Any.optional());
                }
                if rest.len() == 1 {
                    let t = self.convert_slot(rest[0], ctx, scope, slot)?;
                    return Ok(wrap(t, nullable));
                }
                let mut variants = Vec::new();
                for (i, m) in rest.iter().enumerate() {
                    variants.push(self.convert(m, &ctx.variant(i), scope)?);
                }
                let t = self.intern(TypeDef::Untagged { variants }, ctx, description, slot);
                return Ok(wrap(t, nullable));
            }
        }

        if types.is_empty() {
            if obj.contains_key("properties") || obj.contains_key("additionalProperties") {
                types.push("object".to_string());
            } else if obj.contains_key("items") {
                types.push("array".to_string());
            } else {
                return Ok(wrap(TypeRef::Any, nullable));
            }
        }

        let t = if types.len() == 1 {
            self.convert_typed(obj, &types[0], ctx, scope, slot, description)?
        } else {
            let mut variants = Vec::new();
            for (i, ty) in types.iter().enumerate() {
                variants.push(self.convert_typed(obj, ty, &ctx.variant(i), scope, None, None)?);
            }
            self.intern(TypeDef::Untagged { variants }, ctx, description, slot)
        };
        Ok(wrap(t, nullable))
    }

    fn convert_typed(
        &mut self,
        obj: &Map<String, Value>,
        ty: &str,
        ctx: &Ctx,
        scope: &Scope,
        slot: Option<TypeId>,
        description: Option<String>,
    ) -> Result<TypeRef> {
        Ok(match ty {
            "string" => {
                let binary = obj.get("format").and_then(Value::as_str) == Some("binary")
                    || obj.get("contentEncoding").and_then(Value::as_str) == Some("binary");
                if binary && scope.multipart { TypeRef::Upload } else { TypeRef::String }
            }
            "integer" => TypeRef::Int,
            "number" => TypeRef::Float,
            "boolean" => TypeRef::Bool,
            "null" => TypeRef::Any.optional(),
            "array" => {
                let inner = match obj.get("items") {
                    Some(items) => self.convert(items, &ctx.items(), scope)?,
                    None => TypeRef::Any,
                };
                TypeRef::Vec(Box::new(inner))
            }
            "object" => self.convert_object(obj, ctx, scope, slot, description)?,
            other => {
                log::warn!("{ctx}: unknown type `{other}`, using serde_json::Value");
                TypeRef::Any
            }
        })
    }

    fn convert_object(
        &mut self,
        obj: &Map<String, Value>,
        ctx: &Ctx,
        scope: &Scope,
        slot: Option<TypeId>,
        description: Option<String>,
    ) -> Result<TypeRef> {
        let additional = obj.get("additionalProperties");
        let is_multipart_root = scope.multipart && scope.local_root.as_object().is_some_and(|r| std::ptr::eq(r, obj));
        let Some(props) = obj.get("properties").and_then(Value::as_object) else {
            return Ok(match additional {
                Some(Value::Bool(false)) => {
                    self.intern(TypeDef::Struct { fields: Vec::new(), extra: None, multipart: is_multipart_root }, ctx, description, slot)
                }
                Some(s @ Value::Object(_)) => TypeRef::Map(Box::new(self.convert(s, &ctx.values(), scope)?)),
                _ => TypeRef::Map(Box::new(TypeRef::Any)),
            });
        };
        let required: HashSet<&str> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut fields = Vec::new();
        for (name, s) in props {
            let ty = self.convert(s, &ctx.prop(name), scope)?;
            let req = required.contains(name.as_str());
            fields.push(Field {
                json_name: name.clone(),
                ty: if req { ty } else { ty.optional() },
                required: req,
                description: s.get("description").and_then(Value::as_str).map(str::to_string),
            });
        }
        let extra = match additional {
            Some(s @ Value::Object(_)) => Some(self.convert(s, &ctx.values(), scope)?),
            Some(Value::Bool(true)) => Some(TypeRef::Any),
            _ => None,
        };
        Ok(self.intern(TypeDef::Struct { fields, extra, multipart: is_multipart_root }, ctx, description, slot))
    }

    fn merge_all_of(&self, obj: &Map<String, Value>, members: &[Value], scope: &Scope) -> Result<Value> {
        let mut merged = obj.clone();
        merged.remove("allOf");
        let mut props: Map<String, Value> = merged.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut required: Vec<Value> = merged.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
        for m in members {
            let m = spec::deref(self.doc, scope.local_root, m)?;
            let m = match m.get("allOf").and_then(Value::as_array) {
                Some(inner) => self.merge_all_of(m.as_object().unwrap_or(&Map::new()), inner, scope)?,
                None => m.clone(),
            };
            let Some(mo) = m.as_object() else { continue };
            for (k, v) in mo {
                match k.as_str() {
                    "properties" => {
                        if let Some(p) = v.as_object() {
                            for (pk, pv) in p {
                                props.insert(pk.clone(), pv.clone());
                            }
                        }
                    }
                    "required" => {
                        if let Some(r) = v.as_array() {
                            for rv in r {
                                if !required.contains(rv) {
                                    required.push(rv.clone());
                                }
                            }
                        }
                    }
                    "allOf" | "description" | "title" => {}
                    _ => {
                        merged.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }
        merged.insert("properties".into(), Value::Object(props));
        merged.insert("required".into(), Value::Array(required));
        merged.entry("type".to_string()).or_insert_with(|| Value::String("object".into()));
        Ok(Value::Object(merged))
    }

    /// A recursive `anyOf` of string | number | boolean | array-of-self | object-of-self is just
    /// "any JSON value"; `serde_json::Value` is a better fit than a generated enum.
    fn is_json_value_shape(&self, variants: &[TypeRef], slot: Option<TypeId>) -> bool {
        let mut string = false;
        let mut number = false;
        let mut boolean = false;
        let mut array = false;
        let mut object = false;
        for v in variants {
            match self.resolve(v) {
                TypeRef::String => string = true,
                TypeRef::Float | TypeRef::Int => number = true,
                TypeRef::Bool => boolean = true,
                TypeRef::Any => {}
                TypeRef::Vec(x) if slot.is_some() && self.resolve(&x) == TypeRef::Named(slot.unwrap()) => array = true,
                TypeRef::Map(x) if slot.is_some() && self.resolve(&x) == TypeRef::Named(slot.unwrap()) => object = true,
                _ => return false,
            }
        }
        string && number && boolean && array && object
    }

    fn intern(&mut self, def: TypeDef, ctx: &Ctx, description: Option<String>, slot: Option<TypeId>) -> TypeRef {
        if let TypeDef::Untagged { variants } = &def
            && self.is_json_value_shape(variants, slot)
        {
            return TypeRef::Any;
        }
        let key = self.def_key(&def, slot);
        if let Some(&id) = self.keys.get(&key) {
            return TypeRef::Named(id);
        }
        let nt = NamedType {
            def,
            candidates: ctx.candidates(),
            description,
            name: String::new(),
            alias: None,
            placeholder: false,
        };
        let id = match slot {
            Some(id) if self.types[id].placeholder => {
                self.types[id] = nt;
                id
            }
            _ => {
                self.types.push(nt);
                self.types.len() - 1
            }
        };
        self.keys.insert(key, id);
        TypeRef::Named(id)
    }

    fn ref_key(&self, t: &TypeRef, slot: Option<TypeId>) -> String {
        match self.resolve(t) {
            TypeRef::String => "s".into(),
            TypeRef::Int => "i".into(),
            TypeRef::Float => "f".into(),
            TypeRef::Bool => "b".into(),
            TypeRef::Any => "a".into(),
            TypeRef::Upload => "u".into(),
            TypeRef::Option(x) => format!("o({})", self.ref_key(&x, slot)),
            TypeRef::Vec(x) => format!("v({})", self.ref_key(&x, slot)),
            TypeRef::Map(x) => format!("m({})", self.ref_key(&x, slot)),
            TypeRef::Named(id) => {
                if slot == Some(id) { "SELF".into() } else { format!("n{id}") }
            }
        }
    }

    fn def_key(&self, def: &TypeDef, slot: Option<TypeId>) -> String {
        match def {
            TypeDef::Struct { fields, extra, multipart } => {
                let mut s = String::from(if *multipart { "M{" } else { "S{" });
                for f in fields {
                    s.push_str(&format!("{}{}:{};", f.json_name, if f.required { "!" } else { "?" }, self.ref_key(&f.ty, slot)));
                }
                if let Some(e) = extra {
                    s.push_str(&format!("|{}", self.ref_key(e, slot)));
                }
                s.push('}');
                s
            }
            TypeDef::StringEnum { values } => format!("E{{{}}}", values.join("\u{1}")),
            TypeDef::Untagged { variants } => {
                format!("U{{{}}}", variants.iter().map(|v| self.ref_key(v, slot)).collect::<Vec<_>>().join("|"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conv(doc: &Value, schema: &Value, root: &str) -> (Vec<NamedType>, TypeRef) {
        let mut c = Converter::new(doc);
        let t = c.convert(schema, &Ctx::root(root), &Scope { local_root: schema, multipart: false }).unwrap();
        (c.types, t)
    }

    #[test]
    fn dedups_identical_structures() {
        let doc = json!({});
        let s = json!({"type": "object", "properties": {"kind": {"type": "string", "enum": ["a", "b"]}, "n": {"type": ["integer", "null"]}}, "required": ["kind"]});
        let mut c = Converter::new(&doc);
        let scope = Scope { local_root: &s, multipart: false };
        let a = c.convert(&s, &Ctx::root("FirstBody"), &scope).unwrap();
        let b = c.convert(&s, &Ctx::root("SecondBody"), &scope).unwrap();
        assert_eq!(a, b);
        assert_eq!(c.types.len(), 2); // enum + struct
        assert_eq!(c.types[0].candidates, vec!["Kind", "FirstBodyKind"]);
        assert_eq!(c.types[1].candidates, vec!["FirstBody"]);
        match &c.types[1].def {
            TypeDef::Struct { fields, .. } => {
                assert_eq!(fields[0].ty, TypeRef::Named(0));
                assert_eq!(fields[1].ty, TypeRef::Option(Box::new(TypeRef::Int)));
                assert!(!fields[1].required);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn nullable_any_and_scalar_anyof() {
        let doc = json!({});
        let s = json!({"type": "object", "properties": {
            "at": {"anyOf": [{}, {"type": "null"}]},
            "cell": {"anyOf": [{"type": "string"}, {"type": "number"}, {"type": "boolean"}, {"type": "null"}]},
            "rows": {"type": ["array", "null"], "items": {"type": "object", "additionalProperties": {"type": "string"}}}
        }, "required": ["at", "cell", "rows"]});
        let (types, t) = conv(&doc, &s, "R");
        let TypeRef::Named(id) = t else { panic!() };
        let TypeDef::Struct { fields, .. } = &types[id].def else { panic!() };
        assert_eq!(fields[0].ty, TypeRef::Option(Box::new(TypeRef::Any)));
        assert!(matches!(&fields[1].ty, TypeRef::Option(inner) if matches!(**inner, TypeRef::Named(_))));
        assert_eq!(
            fields[2].ty,
            TypeRef::Option(Box::new(TypeRef::Vec(Box::new(TypeRef::Map(Box::new(TypeRef::String))))))
        );
    }

    #[test]
    fn recursive_defs_dedup_across_roots() {
        let doc = json!({});
        let def = json!({"anyOf": [{"type": "string"}, {"type": "array", "items": {"$ref": "#/$defs/n"}}, {"type": "object", "additionalProperties": {"$ref": "#/$defs/n"}}]});
        let s1 = json!({"type": "object", "properties": {"v": {"$ref": "#/$defs/n"}}, "$defs": {"n": def}});
        let s2 = json!({"type": "object", "properties": {"w": {"$ref": "#/$defs/n"}}, "$defs": {"n": def}});
        let mut c = Converter::new(&doc);
        c.convert(&s1, &Ctx::root("A"), &Scope { local_root: &s1, multipart: false }).unwrap();
        c.convert(&s2, &Ctx::root("B"), &Scope { local_root: &s2, multipart: false }).unwrap();
        let live: Vec<&NamedType> = c.types.iter().filter(|t| t.is_live()).collect();
        // one recursive untagged enum + two structs
        assert_eq!(live.len(), 3, "{:?}", c.types);
        assert_eq!(live[0].candidates, vec!["N", "AN"]);
    }

    #[test]
    fn json_value_shape_becomes_any() {
        let doc = json!({});
        let def = json!({"anyOf": [{"type": "string"}, {"type": "number"}, {"type": "boolean"}, {"type": "null"},
            {"type": "array", "items": {"$ref": "#/$defs/j"}}, {"type": "object", "additionalProperties": {"$ref": "#/$defs/j"}}]});
        let s = json!({"type": "object", "properties": {"v": {"$ref": "#/$defs/j"}}, "required": ["v"], "$defs": {"j": def}});
        let (types, t) = conv(&doc, &s, "A");
        let TypeRef::Named(id) = t else { panic!() };
        let TypeDef::Struct { fields, .. } = &types[id].def else { panic!() };
        assert_eq!(fields[0].ty, TypeRef::Option(Box::new(TypeRef::Any)));
        assert_eq!(types.iter().filter(|t| t.is_live()).count(), 1);
    }

    #[test]
    fn components_ref_and_binary_upload() {
        let doc = json!({"components": {"schemas": {"Error": {"type": "object", "properties": {"error": {"type": "string"}}, "required": ["error"]}}}});
        let mut c = Converter::new(&doc);
        let scope = Scope { local_root: &doc, multipart: false };
        let e1 = c.convert_ref("#/components/schemas/Error", &Ctx::root("Error"), &scope).unwrap();
        let inline = json!({"type": "object", "properties": {"error": {"type": "string"}}, "required": ["error"]});
        let e2 = c.convert(&inline, &Ctx::root("SomeOpError"), &scope).unwrap();
        assert_eq!(c.resolve(&e1), c.resolve(&e2));
        let mp = json!({"type": "object", "properties": {"files": {"type": "array", "items": {"type": "string", "format": "binary"}}}});
        let t = c.convert(&mp, &Ctx::root("UploadBody"), &Scope { local_root: &mp, multipart: true }).unwrap();
        let TypeRef::Named(id) = t else { panic!() };
        let TypeDef::Struct { fields, multipart, .. } = &c.types[id].def else { panic!() };
        assert!(*multipart);
        assert_eq!(fields[0].ty, TypeRef::Option(Box::new(TypeRef::Vec(Box::new(TypeRef::Upload)))));
    }
}
