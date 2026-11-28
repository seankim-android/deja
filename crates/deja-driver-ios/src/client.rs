use crate::error::{IosDriverError, Result};
use deja_flow::backoff::{HashableNode, hash_hierarchy_nodes};
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
    #[serde(default)]
    pub hierarchy_hash: String,
    #[serde(default)]
    pub unchanged: bool,
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
                || n.title.contains(text)
                || n.value.as_deref().is_some_and(|v| v.contains(text))
        })
    }
}

fn round_coord_to_i32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let rounded = v.round();
    if rounded >= i32::MAX as f64 {
        i32::MAX
    } else if rounded <= i32::MIN as f64 {
        i32::MIN
    } else {
        rounded as i32
    }
}

/// Computes the deterministic 16-hex chrome-stripped hierarchy hash matching `DejaXCTestRunner`
/// and `deja_flow::backoff::hash_hierarchy_nodes` bit-for-bit.
pub fn compute_hierarchy_hash(nodes: &[HierarchyNode]) -> String {
    let hash = hash_hierarchy_nodes(nodes.iter().map(|n| HashableNode {
        index: n.index,
        parent_index: n.parent_index,
        depth: n.depth,
        source: &n.source,
        element_type: &n.element_type,
        identifier: &n.identifier,
        label: &n.label,
        title: &n.title,
        value: n.value.as_deref(),
        placeholder_value: n.placeholder_value.as_deref(),
        enabled: n.enabled,
        selected: n.selected,
        has_focus: n.has_focus,
        hittable: n.hittable,
        x: round_coord_to_i32(n.frame.x),
        y: round_coord_to_i32(n.frame.y),
        width: round_coord_to_i32(n.frame.width),
        height: round_coord_to_i32(n.frame.height),
    }));
    format!("{hash:016x}")
}

fn validate_known_hash(known_hash: &str) -> Result<String> {
    if known_hash.len() != 16 || !known_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(IosDriverError::InvalidArgument(
            "knownHash must be a 16-character hexadecimal string".to_string(),
        ));
    }
    Ok(known_hash.to_ascii_lowercase())
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_duration_ms: Option<u64>,
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

/// Action payload for compound `POST /act`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ActKind {
    #[serde(rename_all = "camelCase")]
    Tap {
        x: f64,
        y: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Swipe {
        start_x: f64,
        start_y: f64,
        end_x: f64,
        end_y: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold_duration_ms: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Type {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        settle_keyboard: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    Key {
        key: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActRequest {
    pub app_id: String,
    #[serde(flatten)]
    pub kind: ActKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_action_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_hierarchy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_system_ui: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActResponse {
    pub ok: bool,
    #[serde(default)]
    pub had_software_keyboard: Option<bool>,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub hierarchy: Option<HierarchyResponse>,
    #[serde(default)]
    pub hierarchy_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TextMatchMode {
    Contains,
    Exact,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WaitForSelectorRequest {
    pub app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_match: Option<TextMatchMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_stable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_polls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_system_ui: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_hierarchy: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WaitForSelectorResponse {
    pub matched: bool,
    pub elapsed_ms: u64,
    pub polls: u32,
    #[serde(default)]
    pub hierarchy_hash: String,
    #[serde(default)]
    pub node: Option<HierarchyNode>,
    #[serde(default)]
    pub visible_sample: Vec<String>,
    #[serde(default)]
    pub hierarchy: Option<HierarchyResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WarmupRequest {
    pub app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_system_ui: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_hierarchy: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WarmupResponse {
    pub ok: bool,
    pub app_id: String,
    pub state: String,
    pub state_code: i32,
    pub elapsed_ms: u64,
    pub node_count: usize,
    #[serde(default)]
    pub hierarchy_hash: String,
    #[serde(default)]
    pub hierarchy: Option<HierarchyResponse>,
}

/// Typed HTTP client for the loopback `DejaXCTestRunner` companion agent.
#[derive(Clone)]
pub struct AgentClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    request_timeout: Duration,
}

impl std::fmt::Debug for AgentClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentClient")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl AgentClient {
    pub fn new(port: u16, token: String, timeout: Duration) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let header_name = HeaderName::from_static("x-deja-agent-token");
        let mut header_val = HeaderValue::from_str(&token).map_err(|_| {
            IosDriverError::InvalidArgument("agent token contains invalid header characters".into())
        })?;
        header_val.set_sensitive(true);
        headers.insert(header_name, header_val);

        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
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
            request_timeout: timeout,
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
        self.hierarchy_with_known_hash(app_id, include_system_ui, None)
            .await
    }

    pub async fn hierarchy_with_known_hash(
        &self,
        app_id: &str,
        include_system_ui: bool,
        known_hash: Option<&str>,
    ) -> Result<HierarchyResponse> {
        let include = if include_system_ui { "true" } else { "false" };
        let normalized_hash = match known_hash {
            Some(h) if !h.is_empty() => Some(validate_known_hash(h)?),
            _ => None,
        };
        let mut params = vec![("appId", app_id), ("includeSystemUi", include)];
        if let Some(ref h) = normalized_hash {
            params.push(("knownHash", h.as_str()));
        }
        self.get_json("/hierarchy", &params).await
    }

    pub async fn tap(&self, req: &TapRequest) -> Result<ActionResponse> {
        let dur = req.duration_ms.unwrap_or(0).min(30_000);
        let timeout = (dur > 0).then(|| {
            self.request_timeout
                .max(Duration::from_millis(dur.saturating_add(5_000)))
        });
        self.post_json("/tap", req, timeout).await
    }

    pub async fn swipe(&self, req: &SwipeRequest) -> Result<ActionResponse> {
        let dur = req
            .duration_ms
            .unwrap_or(300)
            .min(10_000)
            .saturating_add(req.hold_duration_ms.unwrap_or(0).min(5_000));
        let timeout = self
            .request_timeout
            .max(Duration::from_millis(dur.saturating_add(5_000)));
        self.post_json("/swipe", req, Some(timeout)).await
    }

    pub async fn type_text(&self, req: &TypeRequest) -> Result<ActionResponse> {
        self.post_json("/type", req, None).await
    }

    pub async fn press_key(&self, req: &KeyRequest) -> Result<ActionResponse> {
        self.post_json("/key", req, None).await
    }

    /// Executes an action and optionally returns the post-action hierarchy in one round-trip.
    pub async fn act(&self, req: &ActRequest) -> Result<ActResponse> {
        if let Some(ref h) = req.known_hash
            && !h.is_empty()
        {
            validate_known_hash(h)?;
        }
        let action_ms = match &req.kind {
            ActKind::Tap { duration_ms, .. } => duration_ms.unwrap_or(0).min(30_000),
            ActKind::Swipe {
                duration_ms,
                hold_duration_ms,
                ..
            } => duration_ms
                .unwrap_or(300)
                .min(10_000)
                .saturating_add(hold_duration_ms.unwrap_or(0).min(5_000)),
            ActKind::Type { .. } => 4_000,
            ActKind::Key { .. } => 0,
        };
        let extra = action_ms.saturating_add(req.post_action_delay_ms.unwrap_or(0).min(5_000));
        let timeout = self
            .request_timeout
            .max(Duration::from_millis(extra.saturating_add(5_000)));
        self.post_json("/act", req, Some(timeout)).await
    }

    /// Server-side adaptive selector / stability wait inside `DejaXCTestRunner`.
    pub async fn wait_for_selector(
        &self,
        req: &WaitForSelectorRequest,
    ) -> Result<WaitForSelectorResponse> {
        let wait_ms = req.timeout_ms.unwrap_or(5_000).min(60_000);
        let timeout = self
            .request_timeout
            .max(Duration::from_millis(wait_ms.saturating_add(5_000)));
        self.post_json("/waitForSelector", req, Some(timeout)).await
    }

    /// Post-launch AX warmup call inside `DejaXCTestRunner`.
    pub async fn warmup(&self, req: &WarmupRequest) -> Result<WarmupResponse> {
        let wait_ms = req.timeout_ms.unwrap_or(10_000).min(30_000);
        let timeout = self
            .request_timeout
            .max(Duration::from_millis(wait_ms.saturating_add(5_000)));
        self.post_json("/warmup", req, Some(timeout)).await
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
        timeout_override: Option<Duration>,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self.http.post(&url).json(body);
        if let Some(timeout) = timeout_override {
            builder = builder.timeout(timeout);
        }
        let resp = builder
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
    use super::{
        ActKind, ActRequest, AgentClient, ElementFrame, HierarchyNode, HierarchyResponse,
        SwipeRequest, TextMatchMode, WaitForSelectorRequest, WarmupRequest, compute_hierarchy_hash,
    };
    use crate::error::IosDriverError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn agent_client_debug_redacts_secret_token() {
        let secret = "deadbeef-secret-agent-token-0123456789";
        let client = AgentClient::new(51234, secret.to_string(), Duration::from_secs(5))
            .expect("client init");
        let formatted = format!("{client:?}");
        assert!(
            !formatted.contains(secret),
            "Debug output leaked agent token: {formatted}"
        );
        assert!(
            formatted.contains("[REDACTED]"),
            "Debug output should show [REDACTED]: {formatted}"
        );
        assert!(formatted.contains("http://127.0.0.1:51234"));
    }

    #[tokio::test]
    async fn agent_client_does_not_follow_http_redirects() {
        let target_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target listener");
        let target_port = target_listener.local_addr().expect("target addr").port();
        let target_hit = Arc::new(AtomicBool::new(false));
        let target_hit_clone = Arc::clone(&target_hit);
        let target_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = target_listener.accept().await {
                target_hit_clone.store(true, Ordering::SeqCst);
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await;
            }
        });

        let redirect_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirect listener");
        let redirect_port = redirect_listener
            .local_addr()
            .expect("redirect addr")
            .port();
        let redirect_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = redirect_listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/health\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });

        let client = AgentClient::new(
            redirect_port,
            "secret-agent-token".to_string(),
            Duration::from_secs(5),
        )
        .expect("build agent client");

        match client.health().await {
            Err(IosDriverError::AgentHttpStatus { status: 302, .. }) => {}
            other => panic!("expected 302 without following redirect, got {other:?}"),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !target_hit.load(Ordering::SeqCst),
            "AgentClient must never follow HTTP redirects"
        );

        redirect_task.abort();
        target_task.abort();
    }

    #[test]
    fn hierarchy_helpers_and_hash_work_and_deserialize_legacy_json() {
        let legacy_json = r#"{
            "appId": "dev.deja.ios.checkout",
            "state": "running_foreground",
            "stateCode": 4,
            "truncated": false,
            "nodeCount": 2,
            "nodes": [
                {
                    "index": 0,
                    "depth": 0,
                    "source": "app",
                    "elementType": "Application",
                    "identifier": "",
                    "label": "DejaFixture",
                    "title": "",
                    "enabled": true,
                    "selected": false,
                    "hasFocus": false,
                    "hittable": true,
                    "frame": {"x": 0.0, "y": 0.0, "width": 393.0, "height": 852.0}
                },
                {
                    "index": 1,
                    "parentIndex": 0,
                    "depth": 1,
                    "source": "app",
                    "elementType": "Button",
                    "identifier": "login_button",
                    "label": "Log in",
                    "title": "",
                    "enabled": true,
                    "selected": false,
                    "hasFocus": false,
                    "hittable": true,
                    "frame": {"x": 100.0, "y": 400.0, "width": 120.0, "height": 44.0}
                }
            ]
        }"#;

        let resp: HierarchyResponse =
            serde_json::from_str(legacy_json).expect("legacy hierarchy JSON must deserialize");
        assert!(resp.hierarchy_hash.is_empty());
        assert!(!resp.unchanged);
        assert_eq!(resp.find_by_id("login_button").unwrap().index, 1);
        assert!(resp.contains_text("Log in"));
        assert!(!resp.contains_text("Missing"));

        let hash = compute_hierarchy_hash(&resp.nodes);
        assert_eq!(hash.len(), 16);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));

        // Adding a StatusBar + ticking StaticText child must not alter compute_hierarchy_hash.
        let mut with_status = resp.nodes.clone();
        with_status.push(HierarchyNode {
            index: 2,
            parent_index: Some(0),
            depth: 1,
            source: "app".into(),
            element_type: "StatusBar".into(),
            identifier: "".into(),
            label: "".into(),
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
                height: 54.0,
            },
        });
        with_status.push(HierarchyNode {
            index: 3,
            parent_index: Some(2),
            depth: 2,
            source: "app".into(),
            element_type: "StaticText".into(),
            identifier: "".into(),
            label: "9:42".into(),
            title: "".into(),
            value: None,
            placeholder_value: None,
            enabled: true,
            selected: false,
            has_focus: false,
            hittable: true,
            frame: ElementFrame {
                x: 20.0,
                y: 12.0,
                width: 40.0,
                height: 16.0,
            },
        });
        assert_eq!(compute_hierarchy_hash(&with_status), hash);
    }

    #[test]
    fn act_wait_and_warmup_requests_serialize_flat_wire_contract() {
        let act = ActRequest {
            app_id: "dev.deja.ios.checkout".into(),
            kind: ActKind::Tap {
                x: 120.5,
                y: 340.0,
                count: Some(1),
                duration_ms: None,
            },
            post_action_delay_ms: Some(50),
            return_hierarchy: Some(true),
            include_system_ui: Some(false),
            known_hash: Some("0123456789abcdef".into()),
        };
        let v = serde_json::to_value(&act).expect("serialize ActRequest");
        assert_eq!(v["appId"], "dev.deja.ios.checkout");
        assert_eq!(v["action"], "tap");
        assert_eq!(v["x"], 120.5);
        assert_eq!(v["returnHierarchy"], true);
        assert_eq!(v["knownHash"], "0123456789abcdef");

        let swipe = SwipeRequest {
            app_id: "dev.deja.ios.checkout".into(),
            start_x: 100.0,
            start_y: 500.0,
            end_x: 100.0,
            end_y: 200.0,
            duration_ms: Some(300),
            hold_duration_ms: Some(200),
        };
        let sv = serde_json::to_value(&swipe).expect("serialize SwipeRequest");
        assert_eq!(sv["holdDurationMs"], 200);

        let wait = WaitForSelectorRequest {
            app_id: "dev.deja.ios.checkout".into(),
            identifier: Some("checkout_button".into()),
            text: Some("Checkout".into()),
            text_match: Some(TextMatchMode::Exact),
            not_visible: Some(false),
            wait_stable: Some(true),
            stable_polls: Some(2),
            timeout_ms: Some(4_000),
            include_system_ui: Some(false),
            return_hierarchy: Some(true),
        };
        let wv = serde_json::to_value(&wait).expect("serialize WaitForSelectorRequest");
        assert_eq!(wv["textMatch"], "exact");
        assert_eq!(wv["waitStable"], true);

        let warmup = WarmupRequest {
            app_id: "dev.deja.ios.checkout".into(),
            timeout_ms: Some(5_000),
            include_system_ui: Some(false),
            return_hierarchy: Some(true),
        };
        let wu = serde_json::to_value(&warmup).expect("serialize WarmupRequest");
        assert_eq!(wu["timeoutMs"], 5_000);
        assert_eq!(wu["returnHierarchy"], true);

        let partial_act: super::ActResponse = serde_json::from_str(
            r#"{"ok":true,"elapsedMs":42,"hierarchyError":"snapshot failed"}"#,
        )
        .expect("decode partial-success ActResponse");
        assert!(partial_act.ok);
        assert!(partial_act.hierarchy.is_none());
        assert_eq!(
            partial_act.hierarchy_error.as_deref(),
            Some("snapshot failed")
        );
    }

    #[tokio::test]
    async fn client_validates_known_hash_and_decodes_compound_endpoints() {
        let client = AgentClient::new(1, "tok".into(), Duration::from_secs(2)).expect("client");
        match client
            .hierarchy_with_known_hash("dev.deja.app", false, Some("not-hex"))
            .await
        {
            Err(IosDriverError::InvalidArgument(msg)) => assert!(msg.contains("knownHash")),
            other => panic!("expected InvalidArgument for bad knownHash, got {other:?}"),
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        let server_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"matched":false,"elapsedMs":120,"polls":4,"hierarchyHash":"0123456789abcdef","visibleSample":["Log in","username_input"]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });

        let mock_client =
            AgentClient::new(port, "tok".into(), Duration::from_secs(2)).expect("mock client");
        let wait_resp = mock_client
            .wait_for_selector(&WaitForSelectorRequest {
                app_id: "dev.deja.app".into(),
                identifier: Some("missing_btn".into()),
                text: None,
                text_match: None,
                not_visible: None,
                wait_stable: None,
                stable_polls: None,
                timeout_ms: Some(200),
                include_system_ui: None,
                return_hierarchy: None,
            })
            .await
            .expect("decode wait_for_selector");
        assert!(!wait_resp.matched);
        assert_eq!(wait_resp.polls, 4);
        assert_eq!(wait_resp.visible_sample, vec!["Log in", "username_input"]);
        server_task.abort();
    }
}
