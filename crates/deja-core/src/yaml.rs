//! YAML 1.2 Core Schema loader with `<<` merge expansion, `uniqueKeys` enforcement,
//! cyclic alias rejection, DoS expansion capping, and JavaScript `Object.entries` key order.
//!
//! Designed for bit-for-bit parity with `yaml@2.9.1` (`parseAllDocuments(contents, { merge: true })`
//! followed by `document.toJS()`).

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;

use regex::Regex;
use saphyr_parser::{Event, Parser, ScalarStyle, Tag};

use crate::canonical_json::{js_array_index, js_number_to_string};
use crate::error::CoreError;

/// Upper bound on alias and merge key expansions per YAML stream to defeat billion-laughs payloads.
pub const MAX_ALIAS_EXPANSIONS: usize = 100_000;

/// Upper bound on materialized YAML bytes per YAML stream to defeat aliased large-object OOM payloads.
pub const MAX_MATERIALIZED_BYTES: usize = 16 * 1024 * 1024;

/// Maximum nesting depth when constructing or expanding YAML documents.
const MAX_YAML_DEPTH: usize = 512;

/// JavaScript-compatible YAML 1.2 document value.
#[derive(Debug, Clone, PartialEq)]
pub enum YamlValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Seq(Vec<YamlValue>),
    Map(Vec<(String, YamlValue)>),
}

impl YamlValue {
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_map(&self) -> Option<&[(String, YamlValue)]> {
        match self {
            Self::Map(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_seq(&self) -> Option<&[YamlValue]> {
        match self {
            Self::Seq(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&YamlValue> {
        self.as_map()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

#[derive(Debug, Clone)]
enum RawNode {
    Scalar {
        value: String,
        style: ScalarStyle,
        tag: Option<Tag>,
    },
    Seq(Vec<Rc<RawNode>>),
    Map(Vec<(Rc<RawNode>, Rc<RawNode>)>),
    Alias(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum UniqueKey {
    Null,
    Bool(bool),
    /// Normalized IEEE-754 bits: `-0.0` -> `+0.0`, all `NaN`s canonicalized to one bit pattern.
    Number(u64),
    String(String),
    /// Styled collection rendering matching `yaml@2.9.1` (keeps `[foo]` vs `['foo']` distinct while
    /// colliding identical `[foo]` keys and staying distinct from scalar strings).
    Collection(String),
}

fn null_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:~|[Nn]ull|NULL)?$").expect("valid null regex"))
}

fn bool_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:[Tt]rue|TRUE|[Ff]alse|FALSE)$").expect("valid bool regex"))
}

fn oct_int_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^0o[0-7]+$").expect("valid oct int regex"))
}

fn dec_int_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[-+]?[0-9]+$").expect("valid dec int regex"))
}

fn hex_int_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^0x[0-9a-fA-F]+$").expect("valid hex int regex"))
}

fn float_special_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:[-+]?\.(?:inf|Inf|INF)|\.nan|\.NaN|\.NAN)$")
            .expect("valid float special regex")
    })
}

fn float_exp_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)[eE][-+]?[0-9]+$")
            .expect("valid float exp regex")
    })
}

fn float_dec_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[-+]?(?:\.[0-9]+|[0-9]+\.[0-9]*)$").expect("valid float dec regex")
    })
}

/// Convert a non-negative base-8 or base-16 digit sequence to `f64` with strictly one
/// IEEE-754 round-to-nearest-ties-to-even rounding step (no double rounding).
fn parse_radix_f64(digits: &str, radix: u32) -> f64 {
    debug_assert!(radix == 8 || radix == 16);
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return 0.0;
    }

    let bits_per_digit: u32 = if radix == 16 { 4 } else { 3 };
    let mut chars = trimmed.chars();
    let first_val = chars
        .next()
        .and_then(|c| c.to_digit(radix))
        .expect("validated non-zero radix digit");
    let first_bits: u32 = 32 - first_val.leading_zeros();
    let rest_digits = trimmed.len() - 1;

    // Any positive integer with >= 1025 bits overflows finite IEEE-754 f64 (max finite < 2^1024).
    if rest_digits > 400 {
        return f64::INFINITY;
    }
    let total_bits: u32 = first_bits + (rest_digits as u32) * bits_per_digit;
    let mut exp: i32 = (total_bits as i32) - 1;
    if exp > 1023 {
        return f64::INFINITY;
    }

    // Stream exact bits MSB-first into: top 53 significand bits, 54th guard bit, and sticky bit.
    let mut sig: u64 = 0;
    let mut bits_collected: u32 = 0;
    let mut guard = false;
    let mut have_guard = false;
    let mut sticky = false;

    for shift in (0..first_bits).rev() {
        let bit = u64::from((first_val >> shift) & 1);
        sig = (sig << 1) | bit;
        bits_collected += 1;
    }
    for ch in chars {
        let d = ch.to_digit(radix).expect("validated radix digit");
        if have_guard {
            if d != 0 {
                sticky = true;
                break;
            }
            continue;
        }
        for shift in (0..bits_per_digit).rev() {
            let bit = u64::from((d >> shift) & 1);
            if bits_collected < 53 {
                sig = (sig << 1) | bit;
                bits_collected += 1;
            } else if !have_guard {
                guard = bit != 0;
                have_guard = true;
            } else if bit != 0 {
                sticky = true;
            }
        }
    }

    if bits_collected < 53 {
        sig <<= 53 - bits_collected;
    } else if guard && (sticky || (sig & 1) == 1) {
        sig += 1;
        if sig == (1_u64 << 53) {
            sig >>= 1;
            exp += 1;
            if exp > 1023 {
                return f64::INFINITY;
            }
        }
    }

    let biased_exp = (exp + 1023) as u64;
    let mantissa = sig & ((1_u64 << 52) - 1);
    f64::from_bits((biased_exp << 52) | mantissa)
}

fn parse_yaml_int(raw: &str) -> Option<f64> {
    if oct_int_regex().is_match(raw) {
        Some(parse_radix_f64(&raw[2..], 8))
    } else if hex_int_regex().is_match(raw) {
        Some(parse_radix_f64(&raw[2..], 16))
    } else if dec_int_regex().is_match(raw) {
        raw.parse::<f64>().ok()
    } else {
        None
    }
}

fn parse_yaml_float(raw: &str) -> Option<f64> {
    if float_special_regex().is_match(raw) {
        if raw.ends_with("nan") || raw.ends_with("NaN") || raw.ends_with("NAN") {
            Some(f64::NAN)
        } else if raw.starts_with('-') {
            Some(f64::NEG_INFINITY)
        } else {
            Some(f64::INFINITY)
        }
    } else if float_exp_regex().is_match(raw) || float_dec_regex().is_match(raw) {
        raw.parse::<f64>().ok()
    } else {
        None
    }
}

fn core_schema_tag(tag: &Tag) -> Option<&str> {
    if tag.is_yaml_core_schema() {
        Some(tag.suffix.as_str())
    } else if tag.handle.is_empty() {
        tag.suffix.strip_prefix("tag:yaml.org,2002:")
    } else {
        None
    }
}

fn validate_yaml_tag(tag: Option<&Tag>) -> Result<(), CoreError> {
    if let Some(tag) = tag
        && let Some(suffix) = core_schema_tag(tag)
        && !matches!(
            suffix,
            "str" | "null" | "bool" | "int" | "float" | "seq" | "map" | "merge"
        )
    {
        return Err(CoreError::YamlParse(format!(
            "unsupported YAML tag: tag:yaml.org,2002:{suffix}"
        )));
    }
    Ok(())
}

fn resolve_scalar(
    raw: &str,
    style: ScalarStyle,
    tag: Option<&Tag>,
) -> Result<YamlValue, CoreError> {
    validate_yaml_tag(tag)?;
    if let Some(tag) = tag {
        let Some(suffix) = core_schema_tag(tag) else {
            return Ok(YamlValue::String(raw.to_string()));
        };
        return Ok(match suffix {
            "str" => YamlValue::String(raw.to_string()),
            "null" => {
                if null_regex().is_match(raw) {
                    YamlValue::Null
                } else {
                    YamlValue::String(raw.to_string())
                }
            }
            "bool" => {
                if bool_regex().is_match(raw) {
                    YamlValue::Bool(raw.starts_with(['t', 'T']))
                } else {
                    YamlValue::String(raw.to_string())
                }
            }
            "int" => parse_yaml_int(raw)
                .map(YamlValue::Number)
                .unwrap_or_else(|| YamlValue::String(raw.to_string())),
            "float" => parse_yaml_float(raw)
                .map(YamlValue::Number)
                .unwrap_or_else(|| YamlValue::String(raw.to_string())),
            _ => YamlValue::String(raw.to_string()),
        });
    }

    if style != ScalarStyle::Plain {
        return Ok(YamlValue::String(raw.to_string()));
    }

    if null_regex().is_match(raw) {
        return Ok(YamlValue::Null);
    }
    if bool_regex().is_match(raw) {
        return Ok(YamlValue::Bool(raw.starts_with(['t', 'T'])));
    }
    if let Some(num) = parse_yaml_int(raw) {
        return Ok(YamlValue::Number(num));
    }
    if let Some(num) = parse_yaml_float(raw) {
        return Ok(YamlValue::Number(num));
    }
    Ok(YamlValue::String(raw.to_string()))
}

fn is_merge_key(node: &RawNode) -> bool {
    match node {
        RawNode::Scalar { value, style, tag } if value == "<<" => match tag {
            None => *style == ScalarStyle::Plain,
            Some(t) => core_schema_tag(t) == Some("merge"),
        },
        _ => false,
    }
}

fn normalize_number_bits(n: f64) -> u64 {
    if n.is_nan() {
        f64::NAN.to_bits()
    } else if n == 0.0 {
        0.0_f64.to_bits()
    } else {
        n.to_bits()
    }
}

fn is_safe_plain_flow_scalar(s: &str) -> bool {
    !s.is_empty()
        && s.trim() == s
        && !s.bytes().any(|b| {
            matches!(
                b,
                b',' | b':'
                    | b'['
                    | b']'
                    | b'{'
                    | b'}'
                    | b'"'
                    | b'\''
                    | b'#'
                    | b'\n'
                    | b'\r'
                    | b'\t'
            )
        })
}

fn format_collection_key(value: &YamlValue) -> String {
    match value {
        YamlValue::Null => "null".to_string(),
        YamlValue::Bool(b) => b.to_string(),
        YamlValue::Number(n) => js_number_to_string(*n),
        YamlValue::String(s) => {
            if is_safe_plain_flow_scalar(s) {
                s.clone()
            } else {
                serde_json::to_string(s).expect("UTF-8 string serializes for collection key")
            }
        }
        YamlValue::Seq(items) => {
            if items.is_empty() {
                "[]".to_string()
            } else {
                let inner: Vec<String> = items.iter().map(format_collection_key).collect();
                format!("[ {} ]", inner.join(", "))
            }
        }
        YamlValue::Map(entries) => {
            if entries.is_empty() {
                "{}".to_string()
            } else {
                let inner: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| {
                        let rendered_key = if is_safe_plain_flow_scalar(k) {
                            k.clone()
                        } else {
                            serde_json::to_string(k).expect("UTF-8 key serializes")
                        };
                        format!("{rendered_key}: {}", format_collection_key(v))
                    })
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
        }
    }
}

fn sort_js_object_entries(entries: Vec<(String, YamlValue)>) -> Vec<(String, YamlValue)> {
    let mut array_indices: Vec<(u32, String, YamlValue)> = Vec::new();
    let mut other_keys: Vec<(String, YamlValue)> = Vec::with_capacity(entries.len());

    for (key, val) in entries {
        if let Some(idx) = js_array_index(&key) {
            array_indices.push((idx, key, val));
        } else {
            other_keys.push((key, val));
        }
    }

    array_indices.sort_by_key(|(idx, _, _)| *idx);
    let mut ordered = Vec::with_capacity(array_indices.len() + other_keys.len());
    for (_, key, val) in array_indices {
        ordered.push((key, val));
    }
    ordered.extend(other_keys);
    ordered
}

struct DocBuilder {
    anchors: HashMap<usize, Rc<RawNode>>,
}

impl DocBuilder {
    fn new() -> Self {
        Self {
            anchors: HashMap::new(),
        }
    }

    fn parse_node<'input>(
        &mut self,
        parser: &mut Parser<'input, saphyr_parser::StrInput<'input>>,
        first_event: Event<'input>,
        depth: usize,
    ) -> Result<Rc<RawNode>, CoreError> {
        if depth > MAX_YAML_DEPTH {
            return Err(CoreError::YamlParse(
                "YAML nesting depth exceeds limit".to_string(),
            ));
        }
        match first_event {
            Event::Scalar(value, style, anchor_id, tag) => {
                validate_yaml_tag(tag.as_deref())?;
                let node = Rc::new(RawNode::Scalar {
                    value: value.into_owned(),
                    style,
                    tag: tag.map(|t| t.into_owned()),
                });
                if anchor_id > 0 {
                    self.anchors.insert(anchor_id, Rc::clone(&node));
                }
                Ok(node)
            }
            Event::Alias(anchor_id) => {
                if anchor_id == 0 {
                    return Err(CoreError::YamlParse("undefined YAML alias".to_string()));
                }
                Ok(Rc::new(RawNode::Alias(anchor_id)))
            }
            Event::SequenceStart(anchor_id, tag) => {
                validate_yaml_tag(tag.as_deref())?;
                let mut items = Vec::new();
                loop {
                    let (ev, _) = next_event(parser)?;
                    match ev {
                        Event::SequenceEnd => break,
                        other => items.push(self.parse_node(parser, other, depth + 1)?),
                    }
                }
                let node = Rc::new(RawNode::Seq(items));
                if anchor_id > 0 {
                    self.anchors.insert(anchor_id, Rc::clone(&node));
                }
                Ok(node)
            }
            Event::MappingStart(anchor_id, tag) => {
                validate_yaml_tag(tag.as_deref())?;
                let mut pairs = Vec::new();
                loop {
                    let (key_ev, _) = next_event(parser)?;
                    if matches!(key_ev, Event::MappingEnd) {
                        break;
                    }
                    let key_node = self.parse_node(parser, key_ev, depth + 1)?;
                    let (val_ev, _) = next_event(parser)?;
                    let val_node = self.parse_node(parser, val_ev, depth + 1)?;
                    pairs.push((key_node, val_node));
                }
                let node = Rc::new(RawNode::Map(pairs));
                if anchor_id > 0 {
                    self.anchors.insert(anchor_id, Rc::clone(&node));
                }
                Ok(node)
            }
            other => Err(CoreError::YamlParse(format!(
                "unexpected YAML event while parsing node: {other:?}"
            ))),
        }
    }
}

#[derive(Default)]
struct EvalBudget {
    expansions: usize,
    materialized_bytes: usize,
}

struct Evaluator<'a> {
    anchors: &'a HashMap<usize, Rc<RawNode>>,
    active_aliases: HashSet<usize>,
    budget: &'a mut EvalBudget,
}

impl<'a> Evaluator<'a> {
    fn new(anchors: &'a HashMap<usize, Rc<RawNode>>, budget: &'a mut EvalBudget) -> Self {
        Self {
            anchors,
            active_aliases: HashSet::new(),
            budget,
        }
    }

    fn format_collection_key_styled(&self, raw: &RawNode, val: &YamlValue, depth: usize) -> String {
        if depth > MAX_YAML_DEPTH {
            return format_collection_key(val);
        }
        match raw {
            RawNode::Alias(anchor_id) => {
                if let Some(target) = self.anchors.get(anchor_id) {
                    self.format_collection_key_styled(target, val, depth + 1)
                } else {
                    format_collection_key(val)
                }
            }
            RawNode::Scalar { style, .. } => match val {
                YamlValue::Null => "null".to_string(),
                YamlValue::Bool(b) => b.to_string(),
                YamlValue::Number(n) => js_number_to_string(*n),
                YamlValue::String(s) => match style {
                    ScalarStyle::DoubleQuoted => {
                        serde_json::to_string(s).expect("UTF-8 string serializes")
                    }
                    ScalarStyle::SingleQuoted => format!("'{}'", s.replace('\'', "''")),
                    ScalarStyle::Plain | ScalarStyle::Literal | ScalarStyle::Folded => {
                        if is_safe_plain_flow_scalar(s) {
                            s.clone()
                        } else {
                            serde_json::to_string(s).expect("UTF-8 string serializes")
                        }
                    }
                },
                YamlValue::Seq(_) | YamlValue::Map(_) => format_collection_key(val),
            },
            RawNode::Seq(raw_items) => {
                let YamlValue::Seq(val_items) = val else {
                    return format_collection_key(val);
                };
                if val_items.is_empty() {
                    return "[]".to_string();
                }
                if raw_items.len() != val_items.len() {
                    return format_collection_key(val);
                }
                let inner: Vec<String> = raw_items
                    .iter()
                    .zip(val_items.iter())
                    .map(|(r, v)| self.format_collection_key_styled(r, v, depth + 1))
                    .collect();
                format!("[ {} ]", inner.join(", "))
            }
            RawNode::Map(raw_pairs) => {
                let YamlValue::Map(val_entries) = val else {
                    return format_collection_key(val);
                };
                if val_entries.is_empty() {
                    return "{}".to_string();
                }
                if raw_pairs.iter().any(|(k, _)| is_merge_key(k))
                    || raw_pairs.len() != val_entries.len()
                {
                    return format_collection_key(val);
                }
                let inner: Vec<String> = raw_pairs
                    .iter()
                    .zip(val_entries.iter())
                    .map(|((rk, rv), (vk, vv))| {
                        let rendered_k = match rk.as_ref() {
                            RawNode::Scalar { style, .. } => match style {
                                ScalarStyle::DoubleQuoted => {
                                    serde_json::to_string(vk).expect("UTF-8 key serializes")
                                }
                                ScalarStyle::SingleQuoted => {
                                    format!("'{}'", vk.replace('\'', "''"))
                                }
                                _ => {
                                    if is_safe_plain_flow_scalar(vk) {
                                        vk.clone()
                                    } else {
                                        serde_json::to_string(vk).expect("UTF-8 key serializes")
                                    }
                                }
                            },
                            _ => {
                                if is_safe_plain_flow_scalar(vk) {
                                    vk.clone()
                                } else {
                                    serde_json::to_string(vk).expect("UTF-8 key serializes")
                                }
                            }
                        };
                        format!(
                            "{rendered_k}: {}",
                            self.format_collection_key_styled(rv, vv, depth + 1)
                        )
                    })
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
        }
    }

    fn unique_and_property_key(&self, raw: &RawNode, value: &YamlValue) -> (UniqueKey, String) {
        match value {
            YamlValue::Null => (UniqueKey::Null, String::new()),
            YamlValue::Bool(true) => (UniqueKey::Bool(true), "true".to_string()),
            YamlValue::Bool(false) => (UniqueKey::Bool(false), "false".to_string()),
            YamlValue::Number(n) => (
                UniqueKey::Number(normalize_number_bits(*n)),
                js_number_to_string(*n),
            ),
            YamlValue::String(s) => (UniqueKey::String(s.clone()), s.clone()),
            YamlValue::Seq(_) | YamlValue::Map(_) => {
                let rendered = self.format_collection_key_styled(raw, value, 0);
                (UniqueKey::Collection(rendered.clone()), rendered)
            }
        }
    }

    fn charge_expansion(&mut self, amount: usize) -> Result<(), CoreError> {
        self.budget.expansions = self.budget.expansions.saturating_add(amount);
        if self.budget.expansions > MAX_ALIAS_EXPANSIONS {
            return Err(CoreError::YamlParse(format!(
                "YAML alias/merge expansion exceeds limit of {MAX_ALIAS_EXPANSIONS}"
            )));
        }
        Ok(())
    }

    fn charge_bytes(&mut self, bytes: usize) -> Result<(), CoreError> {
        self.budget.materialized_bytes = self.budget.materialized_bytes.saturating_add(bytes);
        if self.budget.materialized_bytes > MAX_MATERIALIZED_BYTES {
            return Err(CoreError::YamlParse(format!(
                "YAML materialized size exceeds limit of {MAX_MATERIALIZED_BYTES} bytes"
            )));
        }
        Ok(())
    }

    fn eval(&mut self, node: &RawNode, depth: usize) -> Result<YamlValue, CoreError> {
        if depth > MAX_YAML_DEPTH {
            return Err(CoreError::YamlParse(
                "YAML evaluation depth exceeds limit".to_string(),
            ));
        }
        let in_alias = !self.active_aliases.is_empty();
        match node {
            RawNode::Scalar { value, style, tag } => {
                if in_alias {
                    self.charge_expansion(1)?;
                }
                self.charge_bytes(value.len().saturating_add(std::mem::size_of::<YamlValue>()))?;
                resolve_scalar(value, *style, tag.as_ref())
            }
            RawNode::Alias(anchor_id) => {
                if !self.active_aliases.insert(*anchor_id) {
                    return Err(CoreError::YamlParse(
                        "YAML contains a cyclic alias".to_string(),
                    ));
                }
                self.charge_expansion(1)?;
                let target = self
                    .anchors
                    .get(anchor_id)
                    .ok_or_else(|| CoreError::YamlParse("undefined YAML alias".to_string()))?;
                let result = self.eval(target, depth + 1);
                self.active_aliases.remove(anchor_id);
                result
            }
            RawNode::Seq(items) => {
                if in_alias {
                    self.charge_expansion(items.len().max(1))?;
                }
                self.charge_bytes(items.len().saturating_mul(std::mem::size_of::<YamlValue>()))?;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval(item, depth + 1)?);
                }
                Ok(YamlValue::Seq(out))
            }
            RawNode::Map(pairs) => {
                if in_alias {
                    self.charge_expansion(pairs.len().max(1))?;
                }
                self.charge_bytes(
                    pairs
                        .len()
                        .saturating_mul(std::mem::size_of::<(String, YamlValue)>()),
                )?;

                // Step 1: enforce `uniqueKeys` BEFORE merge expansion across non-merge keys.
                let mut seen_unique: HashSet<UniqueKey> = HashSet::with_capacity(pairs.len());
                let mut resolved_keys: Vec<Option<String>> = Vec::with_capacity(pairs.len());
                for (raw_key, _) in pairs {
                    if is_merge_key(raw_key) {
                        resolved_keys.push(None);
                        continue;
                    }
                    let key_val = self.eval(raw_key, depth + 1)?;
                    let (unique, key_str) = self.unique_and_property_key(raw_key, &key_val);
                    if !seen_unique.insert(unique) {
                        return Err(CoreError::YamlParse("Map keys must be unique".to_string()));
                    }
                    resolved_keys.push(Some(key_str));
                }

                // Step 2: evaluate entries and expand `<<` / `!!merge <<` merge keys in document order.
                let mut entries: Vec<(String, YamlValue)> = Vec::new();
                let mut index_by_key: HashMap<String, usize> = HashMap::new();

                for ((_, raw_val), maybe_key) in pairs.iter().zip(resolved_keys) {
                    let val = self.eval(raw_val, depth + 1)?;
                    match maybe_key {
                        None => {
                            // `<<` merge key: single map or sequence of maps; first-writer-wins.
                            match val {
                                YamlValue::Map(merged) => {
                                    self.charge_expansion(merged.len())?;
                                    for (mk, mv) in merged {
                                        if !index_by_key.contains_key(&mk) {
                                            self.charge_bytes(mk.len().saturating_add(
                                                std::mem::size_of::<(String, YamlValue)>(),
                                            ))?;
                                            index_by_key.insert(mk.clone(), entries.len());
                                            entries.push((mk, mv));
                                        }
                                    }
                                }
                                YamlValue::Seq(sources) => {
                                    for source in sources {
                                        match source {
                                            YamlValue::Map(merged) => {
                                                self.charge_expansion(merged.len())?;
                                                for (mk, mv) in merged {
                                                    if !index_by_key.contains_key(&mk) {
                                                        self.charge_bytes(
                                                            mk.len().saturating_add(
                                                                std::mem::size_of::<(
                                                                    String,
                                                                    YamlValue,
                                                                )>(
                                                                ),
                                                            ),
                                                        )?;
                                                        index_by_key
                                                            .insert(mk.clone(), entries.len());
                                                        entries.push((mk, mv));
                                                    }
                                                }
                                            }
                                            _ => {
                                                return Err(CoreError::YamlParse(
                                                    "Merge sources must be maps or map aliases"
                                                        .to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }
                                _ => {
                                    return Err(CoreError::YamlParse(
                                        "Merge sources must be maps or map aliases".to_string(),
                                    ));
                                }
                            }
                        }
                        Some(key_str) => {
                            if let Some(&existing_idx) = index_by_key.get(&key_str) {
                                entries[existing_idx].1 = val;
                            } else {
                                index_by_key.insert(key_str.clone(), entries.len());
                                entries.push((key_str, val));
                            }
                        }
                    }
                }

                Ok(YamlValue::Map(sort_js_object_entries(entries)))
            }
        }
    }
}

fn next_event<'input>(
    parser: &mut Parser<'input, saphyr_parser::StrInput<'input>>,
) -> Result<(Event<'input>, saphyr_parser::Span), CoreError> {
    match parser.next_event() {
        Some(Ok(pair)) => Ok(pair),
        Some(Err(err)) => Err(CoreError::YamlParse(err.to_string())),
        None => Err(CoreError::YamlParse(
            "unexpected end of YAML stream".to_string(),
        )),
    }
}

/// Fail closed on any non-1.2 `%YAML` directive in the directive region preceding `---`.
fn check_yaml_version_directives(
    lines: &[&str],
    start_line_1based: usize,
    doc_start_line_1based: usize,
) -> Result<(), CoreError> {
    let start_idx = start_line_1based.saturating_sub(1);
    let end_idx = doc_start_line_1based.saturating_sub(1).min(lines.len());
    if start_idx >= end_idx {
        return Ok(());
    }
    for (offset, &raw_line) in lines[start_idx..end_idx].iter().enumerate() {
        let line = if start_idx + offset == 0 {
            raw_line.trim_start_matches('\u{FEFF}')
        } else {
            raw_line
        };
        if let Some(rest) = line.strip_prefix("%YAML")
            && rest.starts_with([' ', '\t'])
        {
            let before_comment = rest.split('#').next().unwrap_or(rest);
            let version = before_comment.trim();
            if version != "1.2" {
                return Err(CoreError::YamlParse(format!(
                    "unsupported YAML version directive: %YAML {version} (only YAML 1.2 is supported)"
                )));
            }
        }
    }
    Ok(())
}

/// Parse every YAML 1.2 document in `contents` with merge-key resolution and unique-key validation.
///
/// Empty or comment-only inputs return `Ok(vec![])`; an explicit empty `---` document returns
/// `Ok(vec![YamlValue::Null])`, matching `yaml@2.9.1`. Non-1.2 `%YAML` directives fail closed.
pub fn parse_yaml_documents(contents: &str) -> Result<Vec<YamlValue>, CoreError> {
    let mut parser = Parser::new_from_str(contents);
    let mut documents = Vec::new();
    let lines: Vec<&str> = contents.lines().collect();
    let mut prev_doc_end_line: usize = 1;
    let mut budget = EvalBudget::default();

    while let Some(item) = parser.next_event() {
        let (event, span) = item.map_err(|err| CoreError::YamlParse(err.to_string()))?;
        match event {
            Event::StreamStart | Event::Nothing => {}
            Event::StreamEnd => break,
            Event::DocumentStart(explicit) => {
                if explicit {
                    check_yaml_version_directives(&lines, prev_doc_end_line, span.start.line())?;
                }
                let mut builder = DocBuilder::new();
                let (next_ev, next_span) = next_event(&mut parser)?;
                let raw_doc = match next_ev {
                    Event::DocumentEnd => {
                        prev_doc_end_line = next_span.end.line().max(prev_doc_end_line);
                        Rc::new(RawNode::Scalar {
                            value: String::new(),
                            style: ScalarStyle::Plain,
                            tag: None,
                        })
                    }
                    other => {
                        let node = builder.parse_node(&mut parser, other, 0)?;
                        let (end_ev, end_span) = next_event(&mut parser)?;
                        if !matches!(end_ev, Event::DocumentEnd) {
                            return Err(CoreError::YamlParse(format!(
                                "expected DocumentEnd, got {end_ev:?}"
                            )));
                        }
                        prev_doc_end_line = end_span.end.line().max(prev_doc_end_line);
                        node
                    }
                };
                let mut evaluator = Evaluator::new(&builder.anchors, &mut budget);
                documents.push(evaluator.eval(&raw_doc, 0)?);
            }
            other => {
                return Err(CoreError::YamlParse(format!(
                    "unexpected top-level YAML event: {other:?}"
                )));
            }
        }
    }

    Ok(documents)
}
