//! Loopback-only HTTP client for the on-device `deja-android-agent` instrumentation server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deja_flow::backoff::{HashableNode, hash_hierarchy_nodes};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AndroidDriverError, Result};

/// HTTP authentication header required by `deja-android-agent` on all non-`/health` routes.
pub const AGENT_TOKEN_HEADER: &str = "X-Deja-Token";

/// Computes the 16-character lowercase hex SHA-256 prefix matching `HttpServer.java`'s `tokenDigest`.
#[must_use]
pub fn token_digest(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(&digest[..8])
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayBounds {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    pub status: String,
    pub agent: String,
    #[serde(default)]
    pub protocol_version: u32,
    pub pid: u32,
    #[serde(default)]
    pub sdk_int: u32,
    #[serde(default)]
    pub token_digest: String,
    #[serde(default)]
    pub display: Option<DisplayBounds>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppStateRequest {
    pub app_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppStateResponse {
    pub app_id: String,
    pub foreground: bool,
    #[serde(default)]
    pub foreground_package: String,
    pub running: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ElementFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
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
    #[serde(default)]
    pub checked: bool,
    pub selected: bool,
    pub has_focus: bool,
    pub hittable: bool,
    #[serde(default)]
    pub is_on_screen: Option<bool>,
    pub frame: ElementFrame,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_frame: Option<ElementFrame>,
}

impl HierarchyNode {
    #[must_use]
    pub fn effective_is_on_screen(&self) -> bool {
        self.is_on_screen.unwrap_or(self.hittable)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HierarchyRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_system_ui: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HierarchyResponse {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub foreground_package: String,
    #[serde(default)]
    pub viewport: Option<DisplayBounds>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub hierarchy_hash: String,
    #[serde(default)]
    pub unchanged: bool,
    #[serde(default)]
    pub node_count: usize,
    #[serde(default)]
    pub nodes: Vec<HierarchyNode>,
    #[serde(default)]
    pub visible_sample: Vec<String>,
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

/// Computes the deterministic 16-hex chrome-stripped FNV-1a hierarchy hash matching
/// `HierarchySnapshot.java` and `deja_flow::backoff::hash_hierarchy_nodes` bit-for-bit.
#[must_use]
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
        return Err(AndroidDriverError::InvalidArgument(
            "knownHash must be a 16-character hexadecimal string".to_string(),
        ));
    }
    Ok(known_hash.to_ascii_lowercase())
}

/// Action payload variants for `POST /act`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "camelCase")]
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
    DoubleTap {
        x: f64,
        y: f64,
    },
    #[serde(rename_all = "camelCase")]
    LongPress {
        x: f64,
        y: f64,
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
    },
    #[serde(rename_all = "camelCase")]
    Erase {
        count: u32,
    },
    #[serde(rename_all = "camelCase")]
    Key {
        key: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
    Back,
    HideKeyboard,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActResponse {
    pub ok: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub had_keyboard: Option<bool>,
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
    Regex,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WaitForSelectorRequest {
    pub app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<bool>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
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

/// RAII guard for an in-flight `busyLock`-acquiring agent HTTP RPC.
///
/// If the RPC future is dropped/cancelled/timed out before completing cleanly, `Drop`
/// synchronously marks the session dirty so the next driver call recycles the session
/// instead of colliding with a stale `409 Conflict`.
pub struct InFlightRpcGuard {
    dirty: Arc<AtomicBool>,
    armed: bool,
}

impl InFlightRpcGuard {
    #[must_use]
    pub fn new(dirty: Arc<AtomicBool>) -> Self {
        Self { dirty, armed: true }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlightRpcGuard {
    fn drop(&mut self) {
        if self.armed {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }
}

/// Loopback-only HTTP client for `deja-android-agent`.
#[derive(Clone)]
pub struct AgentClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    expected_token_digest: String,
    request_timeout: Duration,
}

impl std::fmt::Debug for AgentClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentClient")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .field("token_digest", &self.expected_token_digest)
            .finish_non_exhaustive()
    }
}

impl AgentClient {
    pub fn new(port: u16, token: String, timeout: Duration) -> Result<Self> {
        if port == 0 {
            return Err(AndroidDriverError::InvalidArgument(
                "agent loopback port must be non-zero".to_string(),
            ));
        }
        let mut headers = HeaderMap::new();
        let header_name = HeaderName::from_static("x-deja-token");
        let mut header_val = HeaderValue::from_str(&token).map_err(|_| {
            AndroidDriverError::InvalidArgument(
                "agent token contains invalid HTTP header characters".to_string(),
            )
        })?;
        header_val.set_sensitive(true);
        headers.insert(header_name, header_val);

        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .timeout(timeout)
            .build()
            .map_err(|source| AndroidDriverError::Http {
                endpoint: "client_init".to_string(),
                source,
            })?;

        let expected_token_digest = token_digest(&token);
        Ok(Self {
            http,
            base_url: format!("http://127.0.0.1:{port}"),
            token,
            expected_token_digest,
            request_timeout: timeout,
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn expected_token_digest(&self) -> &str {
        &self.expected_token_digest
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn health(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|source| AndroidDriverError::Http {
                endpoint: "/health".to_string(),
                source,
            })?;
        parse_response("/health", resp).await
    }

    pub async fn hierarchy(&self, req: &HierarchyRequest) -> Result<HierarchyResponse> {
        let mut normalized = req.clone();
        if let Some(ref h) = req.known_hash
            && !h.is_empty()
        {
            normalized.known_hash = Some(validate_known_hash(h)?);
        }
        self.post_json("/hierarchy", &normalized, None).await
    }

    pub async fn act(&self, req: &ActRequest) -> Result<ActResponse> {
        let mut normalized = req.clone();
        if let Some(ref h) = req.known_hash
            && !h.is_empty()
        {
            normalized.known_hash = Some(validate_known_hash(h)?);
        }
        let action_ms = match &normalized.kind {
            ActKind::Tap { duration_ms, .. } => duration_ms.unwrap_or(0).min(30_000),
            ActKind::LongPress { duration_ms, .. } => duration_ms.unwrap_or(1_000).min(30_000),
            ActKind::DoubleTap { .. } => 250,
            ActKind::Swipe {
                duration_ms,
                hold_duration_ms,
                ..
            } => duration_ms
                .unwrap_or(300)
                .min(10_000)
                .saturating_add(hold_duration_ms.unwrap_or(0).min(5_000)),
            ActKind::Type { .. } => 3_000,
            ActKind::Erase { count } => u64::from(*count).saturating_mul(25).min(10_000),
            ActKind::Key { count, .. } => {
                u64::from(count.unwrap_or(1)).saturating_mul(25).min(5_000)
            }
            ActKind::Back | ActKind::HideKeyboard => 1_000,
        };
        let extra =
            action_ms.saturating_add(normalized.post_action_delay_ms.unwrap_or(0).min(5_000));
        let timeout = self
            .request_timeout
            .max(Duration::from_millis(extra.saturating_add(5_000)));
        self.post_json("/act", &normalized, Some(timeout)).await
    }

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

    pub async fn app_state(&self, app_id: &str) -> Result<AppStateResponse> {
        let req = AppStateRequest {
            app_id: app_id.to_string(),
        };
        self.post_json("/appState", &req, None).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        let url = format!("{}/shutdown", self.base_url);
        let resp = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|source| AndroidDriverError::Http {
                endpoint: "/shutdown".to_string(),
                source,
            })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let bytes = resp.bytes().await.unwrap_or_default();
            return Err(AndroidDriverError::AgentHttpStatus {
                endpoint: "/shutdown".to_string(),
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Ok(())
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
            .map_err(|source| AndroidDriverError::Http {
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
    let bytes = resp
        .bytes()
        .await
        .map_err(|source| AndroidDriverError::Http {
            endpoint: endpoint.to_string(),
            source,
        })?;
    if !(200..300).contains(&status) {
        return Err(AndroidDriverError::AgentHttpStatus {
            endpoint: endpoint.to_string(),
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|source| AndroidDriverError::Json {
        context: format!("decode response from {endpoint}"),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentClient, ElementFrame, HierarchyNode, InFlightRpcGuard, compute_hierarchy_hash,
        token_digest,
    };
    use crate::error::AndroidDriverError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn agent_client_debug_redacts_secret_token() {
        let secret = "deadbeef-secret-android-token-9876543210";
        let client = AgentClient::new(41234, secret.to_string(), Duration::from_secs(5))
            .expect("client init");
        let debug = format!("{client:?}");
        assert!(
            !debug.contains(secret),
            "Debug output must never leak X-Deja-Token: {debug}"
        );
        assert!(debug.contains("[REDACTED]"));
        assert!(debug.contains(&token_digest(secret)));
    }

    #[test]
    fn fnv1a_hierarchy_hash_matches_java_golden_vector_bit_for_bit() {
        // Golden vector produced directly by compiled `HierarchySnapshot.java`:
        // base=28940cf4aa2f4599, withSystemUi=28940cf4aa2f4599
        let base = vec![
            HierarchyNode {
                index: 0,
                parent_index: None,
                depth: 0,
                source: "app".to_string(),
                element_type: "FrameLayout".to_string(),
                identifier: String::new(),
                label: "Root".to_string(),
                title: String::new(),
                value: None,
                placeholder_value: None,
                enabled: true,
                checked: false,
                selected: false,
                has_focus: false,
                hittable: true,
                is_on_screen: Some(true),
                frame: ElementFrame {
                    x: 0.0,
                    y: 0.0,
                    width: 1080.0,
                    height: 2400.0,
                },
                visible_frame: None,
            },
            HierarchyNode {
                index: 1,
                parent_index: Some(0),
                depth: 1,
                source: "app".to_string(),
                element_type: "Button".to_string(),
                identifier: "com.example.app:id/checkout_btn".to_string(),
                label: "Checkout".to_string(),
                title: "Proceed".to_string(),
                value: Some("ready".to_string()),
                placeholder_value: Some("tap here".to_string()),
                enabled: true,
                checked: false,
                selected: true,
                has_focus: false,
                hittable: true,
                is_on_screen: Some(true),
                frame: ElementFrame {
                    x: 40.0,
                    y: 1800.0,
                    width: 1000.0,
                    height: 120.0,
                },
                visible_frame: None,
            },
        ];

        let with_system_ui = vec![
            HierarchyNode {
                index: 0,
                parent_index: None,
                depth: 0,
                source: "com.android.systemui".to_string(),
                element_type: "FrameLayout".to_string(),
                identifier: "com.android.systemui:id/status_bar".to_string(),
                label: String::new(),
                title: String::new(),
                value: None,
                placeholder_value: None,
                enabled: true,
                checked: false,
                selected: false,
                has_focus: false,
                hittable: false,
                is_on_screen: Some(true),
                frame: ElementFrame {
                    x: 0.0,
                    y: 0.0,
                    width: 1080.0,
                    height: 96.0,
                },
                visible_frame: None,
            },
            HierarchyNode {
                index: 1,
                parent_index: Some(0),
                depth: 1,
                source: "app".to_string(),
                element_type: "TextView".to_string(),
                identifier: String::new(),
                label: "12:34".to_string(),
                title: String::new(),
                value: None,
                placeholder_value: None,
                enabled: true,
                checked: false,
                selected: false,
                has_focus: false,
                hittable: false,
                is_on_screen: Some(true),
                frame: ElementFrame {
                    x: 10.0,
                    y: 10.0,
                    width: 100.0,
                    height: 40.0,
                },
                visible_frame: None,
            },
            HierarchyNode {
                index: 2,
                parent_index: None,
                depth: 0,
                ..base[0].clone()
            },
            HierarchyNode {
                index: 3,
                parent_index: Some(2),
                depth: 1,
                ..base[1].clone()
            },
        ];

        assert_eq!(compute_hierarchy_hash(&base), "28940cf4aa2f4599");
        assert_eq!(compute_hierarchy_hash(&with_system_ui), "28940cf4aa2f4599");
    }

    #[test]
    fn in_flight_rpc_guard_marks_dirty_on_early_drop_unless_disarmed() {
        let dirty = Arc::new(AtomicBool::new(false));
        {
            let mut g = InFlightRpcGuard::new(Arc::clone(&dirty));
            g.disarm();
        }
        assert!(!dirty.load(Ordering::SeqCst));

        {
            let _g = InFlightRpcGuard::new(Arc::clone(&dirty));
        }
        assert!(dirty.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn agent_client_never_follows_redirects() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let hit = Arc::new(AtomicBool::new(false));
        let hit_clone = Arc::clone(&hit);
        let t1 = tokio::spawn(async move {
            if let Ok((mut s, _)) = target.accept().await {
                hit_clone.store(true, Ordering::SeqCst);
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await;
            }
        });

        let redirect = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redir_port = redirect.local_addr().unwrap().port();
        let t2 = tokio::spawn(async move {
            if let Ok((mut s, _)) = redirect.accept().await {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/health\r\nContent-Length: 0\r\n\r\n"
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });

        let client =
            AgentClient::new(redir_port, "tok".to_string(), Duration::from_secs(5)).unwrap();
        match client.health().await {
            Err(AndroidDriverError::AgentHttpStatus { status: 302, .. }) => {}
            other => panic!("expected 302 without following redirect, got {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(!hit.load(Ordering::SeqCst));
        t1.abort();
        t2.abort();
    }
}
