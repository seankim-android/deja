//! Extract every decoded YAML scalar string and mapping key from a flow file.

use crate::yaml::{YamlValue, parse_yaml_documents};

fn visit_strings(node: &YamlValue, out: &mut Vec<String>) {
    match node {
        YamlValue::String(s) => out.push(s.clone()),
        YamlValue::Seq(items) => {
            for item in items {
                visit_strings(item, out);
            }
        }
        YamlValue::Map(entries) => {
            for (key, value) in entries {
                out.push(key.clone());
                visit_strings(value, out);
            }
        }
        YamlValue::Null | YamlValue::Bool(_) | YamlValue::Number(_) => {}
    }
}

/// Every string a YAML file holds once decoded: scalar values and mapping keys in every document.
///
/// Returns `None` when `contents` is not valid YAML (for example a JavaScript file).
#[must_use]
pub fn find_flow_strings(contents: &str) -> Option<Vec<String>> {
    let documents = parse_yaml_documents(contents).ok()?;
    let mut found = Vec::new();
    for doc in &documents {
        visit_strings(doc, &mut found);
    }
    Some(found)
}
