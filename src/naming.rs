//! Identifier sanitising and conflict resolution.

use heck::{ToSnakeCase, ToUpperCamelCase};
use std::collections::{BTreeMap, HashSet};

const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

/// Keywords that cannot be written as raw identifiers.
const NO_RAW: &[&str] = &["self", "Self", "super", "crate", "_"];

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { ' ' })
        .collect()
}

/// `UpperCamelCase` type-name fragment. Never empty, never starts with a digit.
pub fn pascal(s: &str) -> String {
    let mut out = clean(s).to_upper_camel_case();
    if out.is_empty() {
        out = "Unnamed".to_string();
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, 'N');
    }
    out
}

/// `snake_case` fragment. Never empty, never starts with a digit.
pub fn snake(s: &str) -> String {
    let mut out = clean(s).to_snake_case();
    if out.is_empty() {
        out = "unnamed".to_string();
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Escape a keyword so it can be used as an identifier.
pub fn escape_keyword(s: &str) -> String {
    if NO_RAW.contains(&s) {
        format!("{s}_")
    } else if KEYWORDS.contains(&s) {
        format!("r#{s}")
    } else {
        s.to_string()
    }
}

/// A valid snake_case Rust identifier (fields, arguments, methods, modules).
pub fn ident(s: &str) -> String {
    escape_keyword(&snake(s))
}

/// A valid UpperCamelCase Rust identifier (types, variants).
pub fn type_ident(s: &str) -> String {
    escape_type_keyword(pascal(s))
}

/// Append `_` if an already PascalCase name happens to be a keyword (`Self`).
pub fn escape_type_keyword(p: String) -> String {
    if KEYWORDS.contains(&p.as_str()) {
        format!("{p}_")
    } else {
        p
    }
}

/// Strip a `r#` prefix (for file names and serde renames).
pub fn strip_raw(s: &str) -> &str {
    s.strip_prefix("r#").unwrap_or(s)
}

/// Very small English singulariser used for array item names.
pub fn singular(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    if lower.ends_with("ies") && s.len() > 3 {
        format!("{}y", &s[..s.len() - 3])
    } else if (lower.ends_with("sses") || lower.ends_with("xes") || lower.ends_with("shes") || lower.ends_with("ches"))
        && s.len() > 4
    {
        s[..s.len() - 2].to_string()
    } else if lower.ends_with('s') && !lower.ends_with("ss") && s.len() > 1 {
        s[..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Enum variant name for a string enum value.
pub fn variant_name(value: &str) -> String {
    if value.trim().is_empty() {
        return "Empty".to_string();
    }
    type_ident(value)
}

/// `base`, then `base{sep}2`, `base{sep}3`, ... until not in `taken`. Inserts the result into `taken`.
pub fn unique(base: &str, sep: &str, taken: &mut HashSet<String>) -> String {
    if !taken.contains(base) {
        taken.insert(base.to_string());
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}{sep}{n}");
        if !taken.contains(&cand) {
            taken.insert(cand.clone());
            return cand;
        }
        n += 1;
    }
}

/// Global name resolution. Every entry has a list of candidate names, shortest first.
///
/// Level by level, a candidate is granted when exactly one still-unnamed entry wants it and it is
/// not already taken. Contenders move on to their next candidate; entries that exhausted their
/// list keep asking for the last one and finally receive a numeric suffix.
pub fn resolve_unique(candidates: &[Vec<String>], reserved: &HashSet<String>) -> Vec<String> {
    let mut assigned: Vec<Option<String>> = vec![None; candidates.len()];
    let mut taken: HashSet<String> = reserved.clone();
    let mut level = 0usize;
    loop {
        let pending: Vec<usize> = (0..candidates.len()).filter(|i| assigned[*i].is_none()).collect();
        if pending.is_empty() {
            break;
        }
        let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for &i in &pending {
            let list = &candidates[i];
            let cand = list.get(level.min(list.len().saturating_sub(1))).map(String::as_str).unwrap_or("Unnamed");
            groups.entry(cand).or_default().push(i);
        }
        let mut progressed = false;
        for (name, idxs) in &groups {
            if idxs.len() == 1 && !taken.contains(*name) {
                assigned[idxs[0]] = Some((*name).to_string());
                taken.insert((*name).to_string());
                progressed = true;
            }
        }
        let all_exhausted = pending.iter().all(|&i| level + 1 >= candidates[i].len());
        if !progressed && all_exhausted {
            for &i in &pending {
                let base = candidates[i].last().cloned().unwrap_or_else(|| "Unnamed".to_string());
                assigned[i] = Some(unique(&base, "", &mut taken));
            }
            break;
        }
        level += 1;
    }
    assigned.into_iter().map(|a| a.unwrap()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents() {
        assert_eq!(ident("datasetId"), "dataset_id");
        assert_eq!(ident("TODO_update_generation"), "todo_update_generation");
        assert_eq!(ident("type"), "r#type");
        assert_eq!(ident("self"), "self_");
        assert_eq!(ident("123abc"), "_123abc");
        assert_eq!(ident(""), "unnamed");
        assert_eq!(type_ident("Agricultural and Biological Sciences"), "AgriculturalAndBiologicalSciences");
        assert_eq!(type_ident("EN"), "En");
        assert_eq!(type_ident("image/svg+xml"), "ImageSvgXml");
        assert_eq!(type_ident("__schema0"), "Schema0");
        assert_eq!(type_ident("3d"), "N3d");
        assert_eq!(type_ident("Self"), "Self_");
    }

    #[test]
    fn singulars() {
        assert_eq!(singular("Languages"), "Language");
        assert_eq!(singular("Categories"), "Category");
        assert_eq!(singular("Status"), "Statu"); // known limitation, still a valid name
        assert_eq!(singular("Boxes"), "Box");
        assert_eq!(singular("Type"), "Type");
        assert_eq!(singular("Class"), "Class");
    }

    #[test]
    fn resolution_prefers_short_unique_names() {
        let cands = vec![
            vec!["State".into(), "GenerationState".into(), "ItemGenerationState".into()],
            vec!["State".into(), "RunState".into()],
            vec!["Kind".into(), "ItemKind".into()],
            vec!["State".into(), "RunState".into()],
        ];
        let reserved: HashSet<String> = ["Kind".to_string()].into_iter().collect();
        let names = resolve_unique(&cands, &reserved);
        assert_eq!(names, vec!["GenerationState", "RunState", "ItemKind", "RunState2"]);
    }

    #[test]
    fn unique_suffixes() {
        let mut taken = HashSet::new();
        assert_eq!(unique("list", "_", &mut taken), "list");
        assert_eq!(unique("list", "_", &mut taken), "list_2");
        assert_eq!(unique("list", "_", &mut taken), "list_3");
    }
}
