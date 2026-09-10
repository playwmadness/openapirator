//! Intermediate representation shared between the schema converter and the code generator.

pub type TypeId = usize;

/// A reference to a Rust type in the generated module.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeRef {
    String,
    Int,
    Float,
    Bool,
    /// `serde_json::Value`
    Any,
    Option(Box<TypeRef>),
    Vec(Box<TypeRef>),
    /// `HashMap<String, T>`
    Map(Box<TypeRef>),
    Named(TypeId),
    /// A multipart file part (`FilePart`). Only valid inside multipart request bodies.
    Upload,
}

impl TypeRef {
    /// Wrap in `Option` unless it already is one.
    pub fn optional(self) -> TypeRef {
        if matches!(self, TypeRef::Option(_)) {
            self
        } else {
            TypeRef::Option(Box::new(self))
        }
    }

    pub fn is_option(&self) -> bool {
        matches!(self, TypeRef::Option(_))
    }
}

#[derive(Clone, Debug)]
pub struct Field {
    pub json_name: String,
    pub ty: TypeRef,
    pub required: bool,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub enum TypeDef {
    Struct {
        fields: Vec<Field>,
        /// Schema of `additionalProperties`, emitted as a flattened map.
        extra: Option<TypeRef>,
        /// True for the root object of a `multipart/form-data` request body.
        multipart: bool,
    },
    StringEnum {
        values: Vec<String>,
    },
    /// `anyOf` / `oneOf`: an untagged enum.
    Untagged {
        variants: Vec<TypeRef>,
    },
}

#[derive(Clone, Debug)]
pub struct NamedType {
    pub def: TypeDef,
    /// Candidate names, shortest first (see `naming::resolve_unique`).
    pub candidates: Vec<String>,
    pub description: Option<String>,
    /// Final Rust identifier, assigned after all types are collected.
    pub name: String,
    /// Set when this slot turned out to be a duplicate of (or a non-named alias for) another type.
    pub alias: Option<TypeRef>,
    /// True while a `$ref` target is still being converted.
    pub placeholder: bool,
}

impl NamedType {
    pub fn is_live(&self) -> bool {
        !self.placeholder && self.alias.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    pub json_name: String,
    /// Rust argument identifier.
    pub name: String,
    pub ty: TypeRef,
    pub required: bool,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Body {
    /// `application/json`, sent with `.json(&body)`.
    Json(TypeRef),
    /// `application/x-www-form-urlencoded`, sent with `.form(&body)`.
    Form(TypeRef),
    /// `multipart/form-data`; the id points at a struct with `multipart: true`.
    Multipart(TypeId),
    /// Anything else: caller supplies a `reqwest::Body`.
    Raw,
}

#[derive(Clone, Debug)]
pub enum Response {
    Json(TypeRef),
    Text,
    /// No usable 2xx content description: the raw `reqwest::Response` is returned.
    Raw,
}

#[derive(Clone, Debug)]
pub struct Operation {
    pub operation_id: String,
    /// Rust method identifier.
    pub name: String,
    pub tag: Option<String>,
    /// Upper-case HTTP method, e.g. `GET`.
    pub method: String,
    pub path: String,
    /// In path-template order.
    pub path_params: Vec<Param>,
    pub query_params: Vec<Param>,
    pub body: Option<Body>,
    pub response: Response,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Tag {
    pub raw: String,
    /// Accessor method name on `Api` (may be a raw identifier).
    pub accessor: String,
    /// Name of the generated group struct, e.g. `DatasetsApi`.
    pub struct_name: String,
    /// Module / file stem, e.g. `datasets`.
    pub module: String,
    /// Indices into `Model::operations`.
    pub ops: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub title: String,
    pub version: String,
    pub types: Vec<NamedType>,
    pub operations: Vec<Operation>,
    pub tags: Vec<Tag>,
    /// Indices of untagged operations (exposed directly on `Api`).
    pub untagged: Vec<usize>,
    /// Type used for the typed payload of non-2xx responses.
    pub error_type: TypeRef,
    pub default_base_url: Option<String>,
}

impl Model {
    /// Follow alias chains so that `Named(id)` always points at a live type.
    pub fn resolve(&self, t: &TypeRef) -> TypeRef {
        resolve_in(&self.types, t)
    }
}

pub fn resolve_in(types: &[NamedType], t: &TypeRef) -> TypeRef {
    let mut cur = t.clone();
    for _ in 0..64 {
        match &cur {
            TypeRef::Named(id) => match &types[*id].alias {
                Some(a) => cur = a.clone(),
                None => return cur,
            },
            _ => return cur,
        }
    }
    cur
}
