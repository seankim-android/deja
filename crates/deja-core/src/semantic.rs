//! Conservative semantic goal similarity helpers and deterministic local lexical embedder.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::error::CoreError;

/// Default inclusive cosine-similarity floor for semantic hits.
pub const DEFAULT_MINIMUM_SIMILARITY: f64 = 0.90;

/// Default minimum similarity margin over the runner-up candidate.
pub const DEFAULT_MINIMUM_MARGIN: f64 = 0.15;

/// Provider identifier for the deterministic local lexical embedder.
pub const LOCAL_LEXICAL_EMBEDDING_PROVIDER: &str = "local-lexical-v1";

/// Fixed dimensionality of vectors emitted by [`LocalLexicalEmbeddingProvider`].
pub const LOCAL_LEXICAL_EMBEDDING_DIMENSIONS: usize = 128;

const MAX_VECTOR_DIMENSIONS: usize = 4096;

const NEGATION_WORDS: &[&str] = &[
    "no", "not", "never", "without", "avoid", "avoiding", "skip", "skipping", "except", "instead",
    "cannot", "can't", "don't", "won't", "mustnt",
];

const OPPOSITES: &[(&str, &str)] = &[
    ("add", "remove"),
    ("create", "delete"),
    ("enable", "disable"),
    ("open", "close"),
    ("accept", "reject"),
    ("confirm", "cancel"),
    ("purchase", "refund"),
    ("login", "logout"),
    ("signin", "signout"),
];

const STOP_WORDS: &[&str] = &["a", "an", "the", "please"];

fn word_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-z0-9]+(?:'[a-z]+)?").expect("valid word regex"))
}

fn ascii_word_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-z0-9]+").expect("valid ascii word regex"))
}

fn do_not_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bdo\s+not\b").expect("valid do-not regex"))
}

fn sep_run_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[_-]+").expect("valid separator regex"))
}

fn adversarial_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"\bignore\s+(?:all\s+|any\s+|the\s+)?(?:previous|prior|above|system|developer)\s+instructions?\b",
            r"\b(?:system|developer)\s+(?:prompt|message|instructions?)\b",
            r"\b(?:jailbreak|prompt\s+injection)\b",
            r"\b(?:bypass|skip|disable|avoid)\s+(?:the\s+)?(?:verification|validation|checks?|tests?)\b",
            r"\b(?:trust\s+me|do\s+anything|without\s+verification)\b",
            r"\b(?:reveal|print|exfiltrate|steal)\s+(?:the\s+)?(?:secrets?|credentials?|tokens?|passwords?)\b",
        ]
        .into_iter()
        .map(|src| Regex::new(src).expect("valid adversarial marker regex"))
        .collect()
    })
}

fn words(goal: &str) -> Vec<String> {
    let lower = goal.to_lowercase().replace('’', "'");
    word_regex()
        .find_iter(&lower)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn validate_vector(value: &[f64]) -> Result<&[f64], CoreError> {
    if value.is_empty()
        || value.len() > MAX_VECTOR_DIMENSIONS
        || value.iter().any(|component| !component.is_finite())
    {
        return Err(CoreError::Semantic(
            "Embedding provider returned an invalid vector".to_string(),
        ));
    }
    Ok(value)
}

/// Scale-normalized cosine similarity in `[-1.0, 1.0]`, failing closed on invalid or mismatched vectors.
pub fn cosine_similarity(left: &[f64], right: &[f64]) -> Result<f64, CoreError> {
    let left_vec = validate_vector(left)?;
    let right_vec = validate_vector(right)?;
    if left_vec.len() != right_vec.len() {
        return Err(CoreError::Semantic(
            "Embedding vectors must have the same non-zero dimension".to_string(),
        ));
    }

    let mut left_scale = 0.0_f64;
    let mut right_scale = 0.0_f64;
    for (&a, &b) in left_vec.iter().zip(right_vec.iter()) {
        left_scale = left_scale.max(a.abs());
        right_scale = right_scale.max(b.abs());
    }
    if left_scale == 0.0 || right_scale == 0.0 {
        return Ok(0.0);
    }

    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (&raw_a, &raw_b) in left_vec.iter().zip(right_vec.iter()) {
        let a = raw_a / left_scale;
        let b = raw_b / right_scale;
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }

    let result = dot / (left_norm * right_norm).sqrt();
    if !result.is_finite() {
        return Err(CoreError::Semantic(
            "Embedding similarity is not finite".to_string(),
        ));
    }
    Ok(result.clamp(-1.0, 1.0))
}

/// Narrow prompt-injection marker guard matching TypeScript `hasAdversarialGoalMarker`.
#[must_use]
pub fn has_adversarial_goal_marker(goal: &str) -> bool {
    let lower = goal.to_lowercase().replace('’', "'");
    let normalized = sep_run_regex().replace_all(&lower, " ");
    adversarial_patterns()
        .iter()
        .any(|pattern| pattern.is_match(&normalized))
}

/// Returns `true` when one goal is negated and the other is not, or they contain explicit opposite verbs.
#[must_use]
pub fn goals_are_contradictory(left: &str, right: &str) -> bool {
    let left_words: HashSet<String> = words(left).into_iter().collect();
    let right_words: HashSet<String> = words(right).into_iter().collect();
    let left_lower = left.to_lowercase();
    let right_lower = right.to_lowercase();

    let left_negated = NEGATION_WORDS.iter().any(|w| left_words.contains(*w))
        || do_not_regex().is_match(&left_lower);
    let right_negated = NEGATION_WORDS.iter().any(|w| right_words.contains(*w))
        || do_not_regex().is_match(&right_lower);
    if left_negated != right_negated {
        return true;
    }

    for &(left_opp, right_opp) in OPPOSITES {
        if (left_words.contains(left_opp) && right_words.contains(right_opp))
            || (left_words.contains(right_opp) && right_words.contains(left_opp))
        {
            return true;
        }
    }
    false
}

/// A bounded goal embedding provider.
pub trait GoalEmbeddingProvider {
    fn id(&self) -> &str;
    fn embed(&self, goal: &str) -> Result<Vec<f64>, CoreError>;

    /// Optional lexical-equivalence guard used by the local provider to reject hash collisions.
    fn is_collision_safe_match(&self, _requested_goal: &str, _candidate_goal: &str) -> bool {
        true
    }
}

fn token_equivalent(word: &str) -> &str {
    match word {
        "finish" | "finished" | "completes" | "completed" => "complete",
        other => other,
    }
}

fn ascii_words(goal: &str) -> Vec<String> {
    let nfkd: String = goal.nfkd().collect();
    let lower = nfkd.to_lowercase().replace('’', "'");
    ascii_word_regex()
        .find_iter(&lower)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Canonical lexical tokens matching TypeScript `canonicalGoalTokens`.
#[must_use]
pub fn canonical_goal_tokens(goal: &str) -> Vec<String> {
    let raw = ascii_words(goal);
    let mut tokens = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let next = raw.get(index + 1).map(String::as_str).unwrap_or("");
        match (raw[index].as_str(), next) {
            ("check", "out") => {
                tokens.push("checkout".to_string());
                index += 2;
                continue;
            }
            ("log", "in") | ("sign", "in") => {
                tokens.push("login".to_string());
                index += 2;
                continue;
            }
            ("log", "out") | ("sign", "out") => {
                tokens.push("logout".to_string());
                index += 2;
                continue;
            }
            _ => {}
        }
        let word = raw[index].as_str();
        if !STOP_WORDS.contains(&word) {
            tokens.push(token_equivalent(word).to_string());
        }
        index += 1;
    }
    tokens
}

fn feature_index(feature: &str) -> usize {
    let digest = Sha256::digest(feature.as_bytes());
    let head = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    (head as usize) % LOCAL_LEXICAL_EMBEDDING_DIMENSIONS
}

/// Deterministic in-process lexical bag-of-features embedder matching `packages/semantic-local`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalLexicalEmbeddingProvider;

impl LocalLexicalEmbeddingProvider {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl GoalEmbeddingProvider for LocalLexicalEmbeddingProvider {
    fn id(&self) -> &str {
        LOCAL_LEXICAL_EMBEDDING_PROVIDER
    }

    fn is_collision_safe_match(&self, requested_goal: &str, candidate_goal: &str) -> bool {
        canonical_goal_tokens(requested_goal) == canonical_goal_tokens(candidate_goal)
    }

    fn embed(&self, goal: &str) -> Result<Vec<f64>, CoreError> {
        let tokens = canonical_goal_tokens(goal);
        // Mirror JS `Map<string, number>`: duplicate unigrams/bigrams overwrite in the map;
        // only distinct features colliding in the 128-bucket SHA-256 projection accumulate.
        let mut features: BTreeMap<String, f64> = BTreeMap::new();
        for index in 0..tokens.len() {
            features.insert(format!("u:{}", tokens[index]), 2.0);
            if index + 1 < tokens.len() {
                features.insert(format!("b:{}:{}", tokens[index], tokens[index + 1]), 1.0);
            }
        }
        let mut embedding = vec![0.0_f64; LOCAL_LEXICAL_EMBEDDING_DIMENSIONS];
        for (feature, weight) in features {
            embedding[feature_index(&feature)] += weight;
        }
        Ok(embedding)
    }
}
