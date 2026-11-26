use crate::error::{IosDriverError, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const AGENT_TOKEN_HEADER: &str = "X-Deja-Agent-Token";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: String,
    pub agent: String,
    pub version: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppStateResponse {
    pub app_id: String,
    pub state: String,
    pub state_code: i32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ElementFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ElementFrame {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HierarchyNode {
    pub index: usize,
    #[serde(default)]
    pub parent_index: Option<usize>,
    pub depth: usize,
    pub source: String,
    pub element_type: String,
    pub identifier: String,
    pub label: String,
    pub title: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub placeholder_value: Option<String>,
    pub enabled: bool,
    pub selected: bool,
    pub has_focus: bool,
    pub hittable: bool,
    pub frame: ElementFrame,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HierarchyResponse {
    pub app_id: String,
    pub state: String,
    pub state_code: i32,
    pub truncated: bool,
    pub node_count: usize,
    pub nodes: Vec<HierarchyNode>,
}

impl HierarchyResponse {
    /// Finds the first node matching an accessibility identifier (preferring hittable app nodes).
    pub fn find_by_id(&self, id: &str) -> Option<&HierarchyNode> {
        self.nodes
            .iter()
            .find(|n| n.identifier == id && n.hittable)
            .or_else(|| self.nodes.iter().find(|n| n.identifier == id))
    }

    /// Returns whether any node displays `text` in its `label`, `title`, or `value`.
    pub fn contains_text(&self, text: &str) -> bool {
        self.nodes.iter().any(|n| {
            n.label == text
                || n.title == text
                || n.value.as_deref() == Some(text)
                || n.label.contains(text)
                || n.value.as_deref().is_some_and(|v| v.contains(text))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapRequest {
    pub app_id: String,
    pub x: f64,
    pub y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwipeRequest {
    pub app_id: String,
    pub start_x: f64,
    pub start_y: f64,
    pub end_x: f64,
    pub end_y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeRequest {
    pub app_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settle_keyboard: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRequest {
    pub app_id: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResponse {
    pub ok: bool,
    #[serde(default)]
    pub had_software_keyboard: Option<bool>,
}

/// Typed HTTP client for the loopback `DejaXCTestRunner` companion agent.
#[derive(Debug, Clone)]
pub struct AgentClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl AgentClient {
    pub fn new(port: u16, token: String, timeout: Duration) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let header_name = HeaderName::from_static("x-deja-agent-token");
        let header_val = HeaderValue::from_str(&token).map_err(|_| {
            IosDriverError::InvalidArgument("agent token contains invalid header characters".into())
        })?;
        headers.insert(header_name, header_val);

        let http = reqwest::Client::builder()
            .no_proxy()
            .default_headers(headers)
            .timeout(timeout)
            .build()
            .map_err(|source| IosDriverError::Http {
                endpoint: "client_init".to_string(),
                source,
            })?;

        Ok(Self {
            http,
            base_url: format!("http://127.0.0.1:{port}"),
            token,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Creates a copy of this client using a different auth token (e.g. for negative 401 testing).
    pub fn with_token(&self, token: String, timeout: Duration) -> Result<Self> {
        let port = self
            .base_url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| IosDriverError::InvalidArgument("invalid base_url port".into()))?;
        Self::new(port, token, timeout)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn health(&self) -> Result<HealthResponse> {
        self.get_json("/health", &[]).await
    }

    pub async fn app_state(&self, app_id: &str) -> Result<AppStateResponse> {
        self.get_json("/appState", &[("appId", app_id)]).await
    }

    pub async fn hierarchy(
        &self,
        app_id: &str,
        include_system_ui: bool,
    ) -> Result<HierarchyResponse> {
        let include = if include_system_ui { "true" } else { "false" };
        self.get_json(
            "/hierarchy",
            &[("appId", app_id), ("includeSystemUi", include)],
        )
        .await
    }

    pub async fn tap(&self, req: &TapRequest) -> Result<ActionResponse> {
        self.post_json("/tap", req).await
    }

    pub async fn swipe(&self, req: &SwipeRequest) -> Result<ActionResponse> {
        self.post_json("/swipe", req).await
    }

    pub async fn type_text(&self, req: &TypeRequest) -> Result<ActionResponse> {
        self.post_json("/type", req).await
    }

    pub async fn press_key(&self, req: &KeyRequest) -> Result<ActionResponse> {
        self.post_json("/key", req).await
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let raw_url = format!("{}{}", self.base_url, path);
        let url = if query.is_empty() {
            reqwest::Url::parse(&raw_url)
        } else {
            reqwest::Url::parse_with_params(&raw_url, query)
        }
        .map_err(|err| {
            IosDriverError::InvalidArgument(format!("failed to construct URL for {path}: {err}"))
        })?;
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|source| IosDriverError::Http {
                endpoint: path.to_string(),
                source,
            })?;
        parse_response(path, resp).await
    }

    async fn post_json<B: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|source| IosDriverError::Http {
                endpoint: path.to_string(),
                source,
            })?;
        parse_response(path, resp).await
    }
}

async fn parse_response<T: for<'de> Deserialize<'de>>(
    endpoint: &str,
    resp: reqwest::Response,
) -> Result<T> {
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|source| IosDriverError::Http {
        endpoint: endpoint.to_string(),
        source,
    })?;
    if !(200..300).contains(&status) {
        return Err(IosDriverError::AgentHttpStatus {
            endpoint: endpoint.to_string(),
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|source| IosDriverError::Json {
        context: format!("decode response from {endpoint}"),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{ElementFrame, HierarchyNode, HierarchyResponse};

    #[test]
    fn hierarchy_helpers_find_nodes_by_id_and_text() {
        let resp = HierarchyResponse {
            app_id: "dev.deja.ios.checkout".into(),
            state: "running_foreground".into(),
            state_code: 4,
            truncated: false,
            node_count: 2,
            nodes: vec![
                HierarchyNode {
                    index: 0,
                    parent_index: None,
                    depth: 0,
                    source: "app".into(),
                    element_type: "Application".into(),
                    identifier: "".into(),
                    label: "DejaFixture".into(),
                    title: "".into(),
                    value: None,
                    placeholder_value: None,
                    enabled: true,
                    selected: false,
                    has_focus: false,
                    hittable: true,
                    frame: ElementFrame {
                        x: 0.0,
                        y: 0.0,
                        width: 393.0,
                        height: 852.0,
                    },
                },
                HierarchyNode {
                    index: 1,
                    parent_index: Some(0),
                    depth: 1,
                    source: "app".into(),
                    element_type: "Button".into(),
                    identifier: "login_button".into(),
                    label: "Log in".into(),
                    title: "".into(),
                    value: None,
                    placeholder_value: None,
                    enabled: true,
                    selected: false,
                    has_focus: false,
                    hittable: true,
                    frame: ElementFrame {
                        x: 100.0,
                        y: 400.0,
                        width: 120.0,
                        height: 44.0,
                    },
                },
            ],
        };

        let btn = resp.find_by_id("login_button").expect("button found");
        assert_eq!(btn.frame.center(), (160.0, 422.0));
        assert!(resp.contains_text("Log in"));
        assert!(!resp.contains_text("nonexistent"));
    }
}
