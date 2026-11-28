use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deja_core::{
    CrashSignalSource, FailureCategory, RunAttempt, WalkFailure, collect_flow_closure,
    create_redactor, provider_key_redactor,
};
use deja_flow::{
    ActAction, ActPayload, ClassificationInput, CompiledSelector, DriverCapabilities,
    DriverCapability, DriverCrashSignal, DriverError, DriverFuture, EnvStack, FailedStepContext,
    FailureClassifier, FlowExecutionConfig, FlowExecutor, FlowParseError, KeyPress, LaunchOptions,
    MAX_INTERPOLATED_BYTES, ObservationSource, PlatformDriver, PointSpec, Selector, SelectorError,
    StepFailureKind, SwipeParams, SynthesizedAttemptsJunitInput, SynthesizedJunitInput,
    UiHierarchy, UiNode, UiPoint, UiRect, UiSize, VisibleContext, WaitSelectorOutcome,
    WaitSelectorRequest, build_visible_context, count_assertions_in_closure,
    count_assertions_in_flow_yaml, hash_hierarchy_nodes, merge_junit_suites, parse_flow,
    prepare_junit, redact_junit, redact_junit_with_redactors, reports_failure,
    synthesized_attempts_junit, synthesized_junit, wait_local, write_errored_junit,
};
use tokio_util::sync::CancellationToken;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "deja-flow-test-{label}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp test workspace");
        Self { path: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn node(index: usize, parent: Option<usize>, id: &str, label: &str, frame: UiRect) -> UiNode {
    UiNode {
        index,
        parent_index: parent,
        depth: usize::from(parent.is_some()),
        source: "app".to_string(),
        observation_source: ObservationSource::Tree,
        element_type: "Button".to_string(),
        identifier: id.to_string(),
        label: label.to_string(),
        title: String::new(),
        value: None,
        placeholder_value: None,
        enabled: true,
        checked: false,
        selected: false,
        has_focus: false,
        hittable: true,
        is_on_screen: true,
        frame,
    }
}

fn hierarchy_of(nodes: Vec<UiNode>) -> UiHierarchy {
    let mut h = UiHierarchy {
        app_id: "dev.deja.checkout".to_string(),
        state: "running_foreground".to_string(),
        viewport: Some(UiSize::new(390.0, 844.0)),
        truncated: false,
        hierarchy_hash: String::new(),
        unchanged: false,
        nodes,
    };
    h.hierarchy_hash = h.hierarchy_hash_hex();
    h
}

#[derive(Debug, Clone, PartialEq)]
enum DriverEvent {
    EnsureSession,
    LaunchApp {
        app_id: String,
        clear_state: bool,
        clear_keychain: bool,
    },
    StopApp(String),
    KillApp(String),
    ClearState(String),
    ClearKeychain,
    OpenLink(String),
    AddMedia(Vec<PathBuf>),
    Hierarchy {
        known_hash: Option<String>,
    },
    Tap(f64, f64),
    LongPress(f64, f64, Option<u64>),
    DoubleTap(f64, f64),
    Swipe {
        start: UiPoint,
        end: UiPoint,
        duration_ms: u64,
    },
    InputText(String),
    EraseText(u32),
    PressKey(String),
    HideKeyboard,
    Back,
    TakeScreenshot,
    ServerWaitCalled,
}

#[derive(Default)]
struct MockState {
    events: Vec<DriverEvent>,
    hierarchies: VecDeque<Result<UiHierarchy, String>>,
    fallback_hierarchy: UiHierarchy,
    /// When true, `hierarchy()` returns `unchanged: true, nodes: []` whenever `known_hash`
    /// matches the current hierarchy's hash.
    honour_known_hash: bool,
    /// Optional per-tap/swipe screen transitions: pops next hierarchy into `fallback_hierarchy`.
    on_swipe_screens: VecDeque<UiHierarchy>,
    on_tap_screens: VecDeque<UiHierarchy>,
    launch_error: Option<String>,
    open_link_error: Option<String>,
    crash_signal: Option<DriverCrashSignal>,
    hierarchy_delay: Option<Duration>,
    fail_hierarchy_once: bool,
    /// Fails the first N taps with Selector-invisible screen before switching.
    fail_tap_count: usize,
    tap_calls: usize,
}

struct MockDriver {
    platform: &'static str,
    caps: DriverCapabilities,
    state: Mutex<MockState>,
    server_wait_hits: AtomicUsize,
}

impl MockDriver {
    fn full() -> Self {
        Self::with_caps(
            "ios",
            DriverCapabilities::with([
                DriverCapability::AccessibilityTree,
                DriverCapability::SystemBack,
                DriverCapability::ClearState,
                DriverCapability::ClearKeychain,
                DriverCapability::HierarchyHashShortCircuit,
                DriverCapability::AppCrashReports,
            ]),
        )
    }

    fn with_caps(platform: &'static str, caps: DriverCapabilities) -> Self {
        Self {
            platform,
            caps,
            state: Mutex::new(MockState::default()),
            server_wait_hits: AtomicUsize::new(0),
        }
    }

    fn set_screen(&self, h: UiHierarchy) {
        self.state.lock().unwrap().fallback_hierarchy = h;
    }

    fn push_hierarchy(&self, h: UiHierarchy) {
        self.state.lock().unwrap().hierarchies.push_back(Ok(h));
    }

    fn push_hierarchy_err(&self, msg: &str) {
        self.state
            .lock()
            .unwrap()
            .hierarchies
            .push_back(Err(msg.to_string()));
    }

    fn push_swipe_screen(&self, h: UiHierarchy) {
        self.state.lock().unwrap().on_swipe_screens.push_back(h);
    }

    fn push_tap_screen(&self, h: UiHierarchy) {
        self.state.lock().unwrap().on_tap_screens.push_back(h);
    }

    fn events(&self) -> Vec<DriverEvent> {
        self.state.lock().unwrap().events.clone()
    }
}

impl PlatformDriver for MockDriver {
    fn platform(&self) -> &'static str {
        self.platform
    }

    fn capabilities(&self) -> &DriverCapabilities {
        &self.caps
    }

    fn ensure_session<'a>(&'a self, cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::EnsureSession);
            Ok(())
        })
    }

    fn launch_app<'a>(
        &'a self,
        app_id: &'a str,
        opts: &'a LaunchOptions,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let mut st = self.state.lock().unwrap();
            st.events.push(DriverEvent::LaunchApp {
                app_id: app_id.to_string(),
                clear_state: opts.clear_state,
                clear_keychain: opts.clear_keychain,
            });
            if let Some(ref err) = st.launch_error {
                return Err(DriverError::LaunchFailed {
                    app_id: app_id.to_string(),
                    detail: err.clone(),
                });
            }
            Ok(())
        })
    }

    fn stop_app<'a>(
        &'a self,
        app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::StopApp(app_id.to_string()));
            Ok(())
        })
    }

    fn kill_app<'a>(
        &'a self,
        app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::KillApp(app_id.to_string()));
            Ok(())
        })
    }

    fn clear_state<'a>(
        &'a self,
        app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::ClearState(app_id.to_string()));
            Ok(())
        })
    }

    fn clear_keychain<'a>(&'a self, _cancel: &'a CancellationToken) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::ClearKeychain);
            Ok(())
        })
    }

    fn open_link<'a>(
        &'a self,
        _app_id: Option<&'a str>,
        url: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let mut st = self.state.lock().unwrap();
            st.events.push(DriverEvent::OpenLink(url.to_string()));
            if let Some(ref err) = st.open_link_error {
                return Err(DriverError::NavigationFailed {
                    target: url.to_string(),
                    detail: err.clone(),
                });
            }
            Ok(())
        })
    }

    fn add_media<'a>(
        &'a self,
        paths: &'a [PathBuf],
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::AddMedia(paths.to_vec()));
            Ok(())
        })
    }

    fn hierarchy<'a>(
        &'a self,
        _app_id: &'a str,
        _include_system_ui: bool,
        known_hash: Option<&'a str>,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, UiHierarchy> {
        Box::pin(async move {
            let delay = self.state.lock().unwrap().hierarchy_delay;
            if let Some(d) = delay {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(DriverError::Cancelled),
                    _ = tokio::time::sleep(d) => {}
                }
            }
            if cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            let mut st = self.state.lock().unwrap();
            st.events.push(DriverEvent::Hierarchy {
                known_hash: known_hash.map(str::to_string),
            });
            if st.fail_hierarchy_once {
                st.fail_hierarchy_once = false;
                return Err(DriverError::Transport {
                    operation: "hierarchy".to_string(),
                    detail: "simulated transient hierarchy error".to_string(),
                    transient: true,
                });
            }
            if let Some(next) = st.hierarchies.pop_front() {
                return next.map_err(|detail| DriverError::Transport {
                    operation: "hierarchy".to_string(),
                    detail,
                    transient: false,
                });
            }
            let current = st.fallback_hierarchy.clone();
            if st.honour_known_hash
                && let Some(kh) = known_hash
                && !current.hierarchy_hash.is_empty()
                && kh.eq_ignore_ascii_case(&current.hierarchy_hash)
            {
                return Ok(UiHierarchy {
                    app_id: current.app_id,
                    state: current.state,
                    viewport: current.viewport,
                    truncated: false,
                    hierarchy_hash: current.hierarchy_hash,
                    unchanged: true,
                    nodes: Vec::new(),
                });
            }
            Ok(current)
        })
    }

    fn tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        point: UiPoint,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let mut st = self.state.lock().unwrap();
            st.tap_calls += 1;
            st.events.push(DriverEvent::Tap(point.x, point.y));
            if st.tap_calls <= st.fail_tap_count {
                return Err(DriverError::Transport {
                    operation: "tap".to_string(),
                    detail: "transient tap failure".to_string(),
                    transient: true,
                });
            }
            if let Some(next_screen) = st.on_tap_screens.pop_front() {
                st.fallback_hierarchy = next_screen;
            }
            Ok(())
        })
    }

    fn long_press_point<'a>(
        &'a self,
        _app_id: &'a str,
        point: UiPoint,
        duration_ms: Option<u64>,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::LongPress(point.x, point.y, duration_ms));
            Ok(())
        })
    }

    fn double_tap_point<'a>(
        &'a self,
        _app_id: &'a str,
        point: UiPoint,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::DoubleTap(point.x, point.y));
            Ok(())
        })
    }

    fn swipe<'a>(
        &'a self,
        params: &'a SwipeParams,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let mut st = self.state.lock().unwrap();
            st.events.push(DriverEvent::Swipe {
                start: params.start,
                end: params.end,
                duration_ms: params.duration_ms,
            });
            if let Some(next_screen) = st.on_swipe_screens.pop_front() {
                st.fallback_hierarchy = next_screen;
            }
            Ok(())
        })
    }

    fn input_text<'a>(
        &'a self,
        _app_id: &'a str,
        text: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::InputText(text.to_string()));
            Ok(())
        })
    }

    fn erase_text<'a>(
        &'a self,
        _app_id: &'a str,
        count: u32,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::EraseText(count));
            Ok(())
        })
    }

    fn press_key<'a>(
        &'a self,
        _app_id: &'a str,
        key: &'a KeyPress,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::PressKey(key.key.clone()));
            Ok(())
        })
    }

    fn hide_keyboard<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::HideKeyboard);
            Ok(())
        })
    }

    fn back<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            self.state.lock().unwrap().events.push(DriverEvent::Back);
            Ok(())
        })
    }

    fn take_screenshot<'a>(
        &'a self,
        _app_id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::TakeScreenshot);
            Ok(vec![0x89, b'P', b'N', b'G'])
        })
    }

    fn check_app_crash<'a>(
        &'a self,
        _app_id: &'a str,
        _since_epoch_ms: u64,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move { Ok(self.state.lock().unwrap().crash_signal.clone()) })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn wait_for_selector<'a>(
        &'a self,
        req: &'a WaitSelectorRequest,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, WaitSelectorOutcome> {
        Box::pin(async move {
            self.server_wait_hits.fetch_add(1, Ordering::SeqCst);
            self.state
                .lock()
                .unwrap()
                .events
                .push(DriverEvent::ServerWaitCalled);
            wait_local(self, req, cancel).await
        })
    }
}

// ============================================================================
// Category 1: Checkout E2E + hooks + timing + dyn-compatibility + act_and_observe
// ============================================================================

#[tokio::test]
async fn checkout_e2e_hooks_timing_and_act_and_observe_fallback() {
    let ws = TempDir::new("checkout-e2e");
    fs::write(ws.path().join("receipt.jpg"), b"fake-jpeg").unwrap();

    let driver = MockDriver::full();
    // Compile-time dyn-compatibility check for PlatformDriver.
    let dyn_driver: &dyn PlatformDriver = &driver;

    // Screen 1: login/cart screen -> tap checkout -> Screen 2: order confirmation.
    let screen1 = hierarchy_of(vec![
        node(0, None, "", "App", UiRect::new(0.0, 0.0, 390.0, 844.0)),
        node(
            1,
            Some(0),
            "email_input",
            "Email",
            UiRect::new(20.0, 120.0, 350.0, 44.0),
        ),
        node(
            2,
            Some(0),
            "checkout_btn",
            "Checkout",
            UiRect::new(20.0, 400.0, 350.0, 48.0),
        ),
    ]);
    let screen2 = hierarchy_of(vec![
        node(0, None, "", "App", UiRect::new(0.0, 0.0, 390.0, 844.0)),
        node(
            1,
            Some(0),
            "confirmation_title",
            "Order confirmed",
            UiRect::new(20.0, 200.0, 350.0, 40.0),
        ),
    ]);
    driver.set_screen(screen1.clone());
    // First tap selects email_input (stays on screen1), second tap hits checkout_btn -> screen2.
    driver.push_tap_screen(screen1);
    driver.push_tap_screen(screen2);

    let flow_yaml = r#"
appId: dev.deja.checkout
name: Checkout flow
tags: [smoke, checkout]
env:
  BUYER_EMAIL: alice@example.com
onFlowStart:
  - clearState
onFlowComplete:
  - stopApp
---
- launchApp:
    clearState: true
- addMedia:
    - receipt.jpg
- tapOn:
    id: email_input
- eraseText: 10
- inputText: ${BUYER_EMAIL}
- hideKeyboard
- pressKey: Enter
- tapOn:
    id: checkout_btn
- extendedWaitUntil:
    visible: "Order confirmed"
    timeout: 1000
- assertVisible: "Order confirmed"
- assertNotVisible: "Checkout"
- waitForAnimationToEnd:
    timeout: 500
- takeScreenshot: shots/final
"#;

    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(500);
    let executor = FlowExecutor::new(cfg);
    let cancel = CancellationToken::new();

    let outcome = executor.execute_yaml(dyn_driver, flow_yaml, &cancel).await;
    assert!(
        outcome.passed,
        "expected pass, got: {:#?}",
        outcome.failed_step
    );
    assert!(outcome.failure.is_none());
    assert_eq!(outcome.app_id.as_deref(), Some("dev.deja.checkout"));
    assert_eq!(outcome.assertions_executed, 2);
    assert_eq!(outcome.screenshots.len(), 1);
    assert!(ws.path().join("shots/final.png").exists());
    assert!(outcome.timing.steps.len() >= 14);
    assert!(outcome.timing.step_latency_stats().is_some());

    let events = driver.events();
    // Verify onFlowStart ran before launchApp and onFlowComplete ran last.
    let clear_pos = events
        .iter()
        .position(|e| matches!(e, DriverEvent::ClearState(_)))
        .expect("onFlowStart clearState");
    let launch_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                DriverEvent::LaunchApp {
                    clear_state: true,
                    ..
                }
            )
        })
        .expect("launchApp");
    let stop_pos = events
        .iter()
        .position(|e| matches!(e, DriverEvent::StopApp(_)))
        .expect("onFlowComplete stopApp");
    assert!(clear_pos < launch_pos);
    assert!(launch_pos < stop_pos);
    assert!(events.contains(&DriverEvent::InputText("alice@example.com".to_string())));

    // Verify default act_and_observe preserves `action_ok: true` when follow-up hierarchy fails.
    driver.state.lock().unwrap().fail_hierarchy_once = true;
    let act_outcome = dyn_driver
        .act_and_observe(
            &ActPayload {
                app_id: "dev.deja.checkout".to_string(),
                action: ActAction::Tap { x: 10.0, y: 20.0 },
                post_action_delay_ms: None,
                return_hierarchy: true,
                include_system_ui: false,
                known_hash: None,
            },
            &cancel,
        )
        .await
        .expect("act_and_observe must succeed even when follow-up hierarchy fails");
    assert!(act_outcome.action_ok);
    assert!(act_outcome.hierarchy.is_none());
    assert!(
        act_outcome
            .hierarchy_error
            .as_deref()
            .is_some_and(|m| m.contains("simulated transient hierarchy error"))
    );
}

#[tokio::test]
async fn on_flow_complete_runs_as_finally_and_preserves_primary_error() {
    let ws = TempDir::new("hooks-finally");
    let driver = MockDriver::full();
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]));

    let flow_yaml = r#"
appId: dev.deja.checkout
onFlowComplete:
  - stopApp
  - assertVisible: "HookAlsoMissing"
---
- launchApp
- assertVisible: "PrimaryMissing"
"#;

    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(60);
    let outcome = FlowExecutor::new(cfg)
        .execute_yaml(&driver, flow_yaml, &CancellationToken::new())
        .await;

    assert!(!outcome.passed);
    let failed = outcome.failed_step.expect("primary failed step");
    assert!(failed.label.contains("PrimaryMissing"));
    assert_eq!(
        outcome.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::ProductAssertion)
    );
    // onFlowComplete still executed `stopApp` and recorded its own failure in `hook_errors`.
    assert!(
        driver
            .events()
            .iter()
            .any(|e| matches!(e, DriverEvent::StopApp(_)))
    );
    assert_eq!(outcome.hook_errors.len(), 1);
    assert!(outcome.hook_errors[0].contains("HookAlsoMissing"));
}

// ============================================================================
// Category 2: knownHash cached-node reuse & Swift<->Rust FNV-1a parity
// ============================================================================

#[tokio::test]
async fn known_hash_reuses_cached_nodes_and_preserves_fnv1a_parity() {
    let driver = MockDriver::full();
    driver.state.lock().unwrap().honour_known_hash = true;

    let mut status_bar = node(1, Some(0), "", "", UiRect::new(0.0, 0.0, 390.0, 44.0));
    status_bar.element_type = "StatusBar".to_string();
    let mut clock = node(2, Some(1), "", "9:41", UiRect::new(10.0, 5.0, 40.0, 14.0));
    clock.element_type = "StaticText".to_string();
    clock.depth = 2;
    let checkout = node(
        3,
        Some(0),
        "checkout_btn",
        "Checkout",
        UiRect::new(20.0, 400.0, 350.0, 48.0),
    );
    let root = node(0, None, "", "App", UiRect::new(0.0, 0.0, 390.0, 844.0));

    let screen = hierarchy_of(vec![root.clone(), status_bar, clock, checkout.clone()]);
    let expected_hash = hash_hierarchy_nodes(screen.as_hashable());
    assert_eq!(screen.hierarchy_hash_u64(), expected_hash);

    // Changing status-bar clock does not alter hierarchy_hash_u64.
    let screen_without_clock = hierarchy_of(vec![root, checkout]);
    assert_eq!(
        screen.hierarchy_hash_u64(),
        screen_without_clock.hierarchy_hash_u64()
    );

    driver.set_screen(screen);

    // 1. wait_stable: poll 1 fetches full tree; poll 2 gets `unchanged: true, nodes: []` via knownHash
    // and must still match `checkout_btn` by reusing cached nodes!
    let mut req = WaitSelectorRequest::visible(
        "dev.deja.checkout",
        Selector::by_id("checkout_btn"),
        Duration::from_millis(500),
    );
    req.wait_stable = true;
    req.stable_polls = 2;

    let cancel = CancellationToken::new();
    let outcome = wait_local(&driver, &req, &cancel).await.unwrap();
    assert!(outcome.matched);
    assert!(outcome.polls >= 2);
    assert_eq!(
        outcome.matched_node.as_ref().map(|n| n.identifier.as_str()),
        Some("checkout_btn")
    );
    // Verify poll 2 sent the exact `known_hash` computed from poll 1.
    let hashes: Vec<Option<String>> = driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::Hierarchy { known_hash } => Some(known_hash),
            _ => None,
        })
        .collect();
    assert_eq!(hashes[0], None);
    assert!(hashes[1].is_some());

    // 2. `not_visible: true` on an element that IS present must NEVER falsely pass when poll 2
    // returns `unchanged: true, nodes: []`!
    let nv_req = WaitSelectorRequest::not_visible(
        "dev.deja.checkout",
        Selector::by_id("checkout_btn"),
        Duration::from_millis(80),
    );
    let nv_outcome = wait_local(&driver, &nv_req, &cancel).await.unwrap();
    assert!(
        !nv_outcome.matched,
        "unchanged: true with empty nodes must reuse cached nodes and NOT falsely pass not_visible"
    );

    // 3. Buggy driver returning `unchanged: true, nodes: []` with NO cache must also NEVER falsely pass.
    let uncached_driver = MockDriver::full();
    uncached_driver.set_screen(UiHierarchy {
        app_id: "dev.deja.checkout".to_string(),
        unchanged: true,
        nodes: Vec::new(),
        ..UiHierarchy::default()
    });
    let uncached_nv = wait_local(&uncached_driver, &nv_req, &cancel)
        .await
        .unwrap();
    assert!(!uncached_nv.matched);
}

// ============================================================================
// Category 3: truncated guards & scrubbed VisibleContext
// ============================================================================

#[tokio::test]
async fn truncated_guards_and_scrubbed_visible_context() {
    let driver = MockDriver::full();
    let mut trunc = hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]);
    trunc.truncated = true;
    let complete = hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]);

    // Always-truncated must NEVER falsely pass `not_visible`.
    driver.set_screen(trunc.clone());
    let cancel = CancellationToken::new();
    let nv_req = WaitSelectorRequest::not_visible(
        "dev.deja.checkout",
        Selector::by_id("missing_spinner"),
        Duration::from_millis(80),
    );
    let res = wait_local(&driver, &nv_req, &cancel).await.unwrap();
    assert!(
        !res.matched,
        "not_visible must be inconclusive while hierarchy is truncated"
    );

    // Once a non-truncated capture arrives, `not_visible` passes.
    driver.push_hierarchy(trunc.clone());
    driver.push_hierarchy(complete.clone());
    let res2 = wait_local(&driver, &nv_req, &cancel).await.unwrap();
    assert!(res2.matched);
    assert_eq!(res2.polls, 2);

    // `wait_stable` resets SettleTracker on a truncated capture.
    let stable_driver = MockDriver::full();
    stable_driver.push_hierarchy(complete.clone()); // consecutive = 1
    stable_driver.push_hierarchy(trunc.clone()); // truncated -> reset to 0!
    stable_driver.push_hierarchy(complete.clone()); // consecutive = 1
    stable_driver.push_hierarchy(complete.clone()); // consecutive = 2 -> settled!
    stable_driver.set_screen(complete);
    let mut stable_req = WaitSelectorRequest::visible(
        "dev.deja.checkout",
        Selector::by_id("home"),
        Duration::from_millis(500),
    );
    stable_req.wait_stable = true;
    stable_req.stable_polls = 2;
    let settled = wait_local(&stable_driver, &stable_req, &cancel)
        .await
        .unwrap();
    assert!(settled.matched);
    assert_eq!(
        settled.polls, 4,
        "truncated capture between two matching hashes must reset SettleTracker"
    );

    // VisibleContext excludes StatusBar descendants & off-screen nodes, caps at 30, and scrubs secrets.
    let mut nodes = Vec::new();
    let mut sb = node(0, None, "", "", UiRect::new(0.0, 0.0, 390.0, 44.0));
    sb.element_type = "StatusBar".to_string();
    nodes.push(sb);
    let mut clock = node(1, Some(0), "", "9:41", UiRect::new(10.0, 5.0, 40.0, 14.0));
    clock.element_type = "StaticText".to_string();
    nodes.push(clock);

    let mut offscreen = node(
        2,
        None,
        "offscreen_id",
        "OffscreenLabel",
        UiRect::new(0.0, 1200.0, 200.0, 40.0),
    );
    offscreen.is_on_screen = false;
    nodes.push(offscreen);

    // Construct sensitive test values dynamically so repo secret-leak scanners stay happy.
    let fake_key = format!("{}test_0123456789abcdef0123", "sk-");
    nodes.push(node(
        3,
        None,
        "secret_field",
        &format!("Token {fake_key} at /tmp/my-ws/app.yaml fixture=superSecretFixture"),
        UiRect::new(10.0, 100.0, 300.0, 40.0),
    ));
    for i in 0..40 {
        nodes.push(node(
            10 + i,
            None,
            &format!("item_{i}"),
            &format!("Item {i}"),
            UiRect::new(10.0, 150.0 + (i as f64) * 10.0, 200.0, 20.0),
        ));
    }

    let redactor = create_redactor("/tmp/my-ws", None, &["superSecretFixture".to_string()]);
    let provider = provider_key_redactor([("API_TOKEN", "mySecretEnvToken123456")]);
    let ctx = build_visible_context(
        &nodes,
        Some(&redactor),
        Some(&provider),
        VisibleContext::DEFAULT_LIMIT,
    );
    assert_eq!(ctx.items().len(), 30);
    assert!(ctx.is_truncated());
    let summary = ctx.format_summary();
    assert!(!summary.contains("9:41"), "StatusBar descendant leaked");
    assert!(
        !summary.contains("OffscreenLabel"),
        "off-screen node leaked"
    );
    assert!(!summary.contains(&fake_key), "provider key leaked");
    assert!(!summary.contains("/tmp/my-ws"), "workspace path leaked");
    assert!(
        !summary.contains("superSecretFixture"),
        "fixture secret leaked"
    );
    assert!(summary.contains("<redacted-secret>"));
    assert!(summary.contains("<workspace>"));
    assert!(summary.contains("<redacted-fixture-value>"));
}

// ============================================================================
// Category 4: Anchored regex, ReDoS cap, relational predicates, index, % point
// ============================================================================

#[tokio::test]
async fn anchored_regex_redos_cap_relational_predicates_index_and_percent_points() {
    // 1. Anchored regex + ReDoS cap.
    let order_sel = Selector::by_text(r"Order #[0-9]+");
    assert!(!order_sel.is_simple_literal_for_server_wait());
    let compiled = CompiledSelector::compile(&order_sel).unwrap();
    let n_exact = node(
        1,
        None,
        "o1",
        "Order #42",
        UiRect::new(0.0, 100.0, 100.0, 30.0),
    );
    let n_prefix = node(
        2,
        None,
        "o2",
        "Prefix Order #42",
        UiRect::new(0.0, 140.0, 100.0, 30.0),
    );
    let mut n_placeholder = node(3, None, "o3", "", UiRect::new(0.0, 180.0, 100.0, 30.0));
    n_placeholder.placeholder_value = Some("Order #99".to_string());
    let regex_nodes = [n_exact, n_prefix, n_placeholder];
    let matched = compiled.match_all(&regex_nodes);
    let matched_ids: Vec<&str> = matched.iter().map(|n| n.identifier.as_str()).collect();
    assert_eq!(matched_ids, vec!["o1", "o3"]);

    // Giant regex must trip the 64 KiB size_limit cleanly without hanging/OOM.
    let huge_pattern = "a{10000}".repeat(20);
    let err = CompiledSelector::compile(&Selector::by_text(huge_pattern))
        .expect_err("oversized regex must fail size_limit");
    assert!(matches!(
        err,
        SelectorError::InvalidRegex { field: "text", .. }
    ));

    // 2. Simple literal vs non-simple server-wait gating.
    assert!(Selector::by_id("checkout_btn").is_simple_literal_for_server_wait());
    assert!(Selector::by_text("Checkout").is_simple_literal_for_server_wait());
    assert!(!Selector::by_text("Order.*").is_simple_literal_for_server_wait());
    let mut indexed = Selector::by_text("Delete");
    indexed.index = Some(1);
    assert!(!indexed.is_simple_literal_for_server_wait());
    let mut stateful = Selector::by_id("submit");
    stateful.enabled = Some(true);
    assert!(!stateful.is_simple_literal_for_server_wait());

    // Even when driver supports ServerSideWait, regex/relational/state selectors run locally!
    let ws = TempDir::new("local-wait-gating");
    let srv_driver = MockDriver::with_caps(
        "ios",
        DriverCapabilities::with([
            DriverCapability::AccessibilityTree,
            DriverCapability::ServerSideWait,
        ]),
    );
    srv_driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "order_42",
        "Order #42",
        UiRect::new(20.0, 200.0, 200.0, 44.0),
    )]));
    let yaml = r#"
appId: dev.deja.checkout
---
- assertVisible: "Order #[0-9]+"
- assertVisible:
    id: order_42
"#;
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&srv_driver, yaml, &CancellationToken::new())
        .await;
    assert!(out.passed, "{:#?}", out.failed_step);
    // Only the plain literal `id: order_42` hit server wait (1 hit, NOT 2).
    assert_eq!(srv_driver.server_wait_hits.load(Ordering::SeqCst), 1);

    // 3. Spatial ranking (`below`, `above`, `leftOf`, `rightOf`) + `childOf` (with cycle guard)
    // + `containsChild` + `containsDescendants` + tie-breaking + `index`.
    let container = node(10, None, "card", "", UiRect::new(0.0, 0.0, 390.0, 500.0));
    let anchor = node(
        11,
        Some(10),
        "email_label",
        "Email",
        UiRect::new(150.0, 200.0, 80.0, 30.0),
    );
    let below_near = node(
        12,
        Some(10),
        "input_near",
        "Field",
        UiRect::new(150.0, 250.0, 80.0, 30.0),
    );
    let below_far = node(
        13,
        Some(10),
        "input_far",
        "Field",
        UiRect::new(150.0, 420.0, 80.0, 30.0),
    );
    let above_node = node(
        14,
        Some(10),
        "header_above",
        "Header",
        UiRect::new(150.0, 100.0, 80.0, 30.0),
    );
    let left_node = node(
        15,
        Some(10),
        "icon_left",
        "Left",
        UiRect::new(40.0, 200.0, 80.0, 30.0),
    );
    let right_node = node(
        16,
        Some(10),
        "badge_right",
        "Right",
        UiRect::new(260.0, 200.0, 80.0, 30.0),
    );
    let deep_child = node(
        17,
        Some(12),
        "deep_leaf",
        "Leaf",
        UiRect::new(155.0, 255.0, 30.0, 20.0),
    );

    let all_nodes = vec![
        container, anchor, below_far, below_near, above_node, left_node, right_node, deep_child,
    ];

    // `below` ranks `input_near` ahead of `input_far` even though `input_far` appeared earlier in tree!
    let below_sel = Selector {
        text: Some("Field".to_string()),
        below: Some(Box::new(Selector::by_text("Email"))),
        ..Selector::default()
    };
    let below_hit = CompiledSelector::compile(&below_sel)
        .unwrap()
        .match_one(&all_nodes)
        .unwrap();
    assert_eq!(below_hit.identifier, "input_near");

    let above_sel = Selector {
        above: Some(Box::new(Selector::by_text("Email"))),
        ..Selector::default()
    };
    assert_eq!(
        CompiledSelector::compile(&above_sel)
            .unwrap()
            .match_one(&all_nodes)
            .unwrap()
            .identifier,
        "header_above"
    );

    let left_sel = Selector {
        left_of: Some(Box::new(Selector::by_text("Email"))),
        ..Selector::default()
    };
    assert_eq!(
        CompiledSelector::compile(&left_sel)
            .unwrap()
            .match_one(&all_nodes)
            .unwrap()
            .identifier,
        "icon_left"
    );

    let right_sel = Selector {
        right_of: Some(Box::new(Selector::by_text("Email"))),
        ..Selector::default()
    };
    assert_eq!(
        CompiledSelector::compile(&right_sel)
            .unwrap()
            .match_one(&all_nodes)
            .unwrap()
            .identifier,
        "badge_right"
    );

    let child_of_sel = Selector {
        text: Some("Leaf".to_string()),
        child_of: Some(Box::new(Selector::by_id("card"))),
        ..Selector::default()
    };
    assert_eq!(
        CompiledSelector::compile(&child_of_sel)
            .unwrap()
            .match_one(&all_nodes)
            .unwrap()
            .identifier,
        "deep_leaf"
    );

    // Ancestor cycle guard on `child_of`: cyclic parent_index pointers must terminate cleanly.
    let cyclic_nodes = vec![
        node(0, Some(1), "cyc_a", "A", UiRect::new(0.0, 0.0, 50.0, 50.0)),
        node(1, Some(0), "cyc_b", "B", UiRect::new(0.0, 0.0, 50.0, 50.0)),
    ];
    let cyc_sel = Selector {
        id: Some("cyc_a".to_string()),
        child_of: Some(Box::new(Selector::by_id("nonexistent"))),
        ..Selector::default()
    };
    assert!(
        CompiledSelector::compile(&cyc_sel)
            .unwrap()
            .match_one(&cyclic_nodes)
            .is_none()
    );

    // `containsChild` (direct child) & `containsDescendants` (multiple deep descendants).
    let cc_sel = Selector {
        contains_child: Some(Box::new(Selector::by_id("email_label"))),
        ..Selector::default()
    };
    assert_eq!(
        CompiledSelector::compile(&cc_sel)
            .unwrap()
            .match_one(&all_nodes)
            .unwrap()
            .identifier,
        "card"
    );

    let cd_sel = Selector {
        contains_descendants: vec![Selector::by_id("email_label"), Selector::by_id("deep_leaf")],
        ..Selector::default()
    };
    assert_eq!(
        CompiledSelector::compile(&cd_sel)
            .unwrap()
            .match_one(&all_nodes)
            .unwrap()
            .identifier,
        "card"
    );

    // Tie-breaking: hittable && is_on_screen > is_on_screen > off-screen; index picks Nth.
    let mut off = node(1, None, "dup_off", "Buy", UiRect::new(0.0, 0.0, 50.0, 30.0));
    off.is_on_screen = false;
    off.hittable = false;
    let mut vis_nonhit = node(
        2,
        None,
        "dup_vis",
        "Buy",
        UiRect::new(0.0, 50.0, 50.0, 30.0),
    );
    vis_nonhit.is_on_screen = true;
    vis_nonhit.hittable = false;
    let hit_a = node(
        3,
        None,
        "dup_hit_a",
        "Buy",
        UiRect::new(0.0, 100.0, 50.0, 30.0),
    );
    let hit_b = node(
        4,
        None,
        "dup_hit_b",
        "Buy",
        UiRect::new(0.0, 150.0, 50.0, 30.0),
    );

    let tie_nodes = vec![off, vis_nonhit, hit_a, hit_b];
    let buy_all = CompiledSelector::compile(&Selector::by_text("Buy"))
        .unwrap()
        .match_all(&tie_nodes);
    let ordered: Vec<&str> = buy_all.iter().map(|n| n.identifier.as_str()).collect();
    assert_eq!(
        ordered,
        vec!["dup_hit_a", "dup_hit_b", "dup_vis", "dup_off"]
    );

    let mut buy_second = Selector::by_text("Buy");
    buy_second.index = Some(1);
    assert_eq!(
        CompiledSelector::compile(&buy_second)
            .unwrap()
            .match_one(&tie_nodes)
            .unwrap()
            .identifier,
        "dup_hit_b"
    );

    // `%` point & pixel point resolution.
    let pct = PointSpec::parse("50%, 75%").unwrap();
    let resolved_pct = pct.resolve(UiSize::new(400.0, 800.0));
    assert!((resolved_pct.x - 200.0).abs() < 1e-6);
    assert!((resolved_pct.y - 600.0).abs() < 1e-6);
    let px = PointSpec::parse("120, 340").unwrap();
    let resolved_px = px.resolve(UiSize::new(400.0, 800.0));
    assert!((resolved_px.x - 120.0).abs() < 1e-6);
    assert!((resolved_px.y - 340.0).abs() < 1e-6);
}

// ============================================================================
// Category 5: scrollUntilVisible settle bailout + centerElement nudge
// ============================================================================

#[tokio::test]
async fn scroll_until_visible_center_nudge_and_settle_bailout() {
    let ws = TempDir::new("scroll-until-visible");

    // 1. Target appears after 1 scroll near the bottom edge (y=790..830, center=810 > 80% of 844)
    // -> `centerElement: true` issues a nudge swipe toward 50%!
    let driver = MockDriver::full();
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "row_1",
        "Row 1",
        UiRect::new(0.0, 100.0, 390.0, 40.0),
    )]));
    driver.push_swipe_screen(hierarchy_of(vec![
        node(
            0,
            None,
            "row_2",
            "Row 2",
            UiRect::new(0.0, 100.0, 390.0, 40.0),
        ),
        node(
            1,
            None,
            "terms_btn",
            "Terms",
            UiRect::new(20.0, 790.0, 350.0, 40.0),
        ),
    ]));

    let yaml = r#"
appId: dev.deja.checkout
---
- scrollUntilVisible:
    element:
      id: terms_btn
    direction: DOWN
    timeout: 5000
    speed: 60
    visibilityPercentage: 100
    centerElement: true
"#;
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver, yaml, &CancellationToken::new())
        .await;
    assert!(out.passed, "{:#?}", out.failed_step);
    let swipes: Vec<_> = driver
        .events()
        .into_iter()
        .filter(|e| matches!(e, DriverEvent::Swipe { .. }))
        .collect();
    assert_eq!(
        swipes.len(),
        2,
        "expected 1 discovery scroll swipe + 1 centerElement nudge swipe"
    );

    // 2. End-of-scroll bailout via SettleTracker::new(2): static screen bails out in <500ms
    // instead of burning the 20s timeout!
    let static_driver = MockDriver::full();
    static_driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "end_row",
        "End of list",
        UiRect::new(0.0, 200.0, 390.0, 40.0),
    )]));

    let bail_yaml = r#"
appId: dev.deja.checkout
---
- scrollUntilVisible:
    element: "NeverAppears"
    direction: DOWN
    timeout: 20000
"#;
    let started = Instant::now();
    let bail_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&static_driver, bail_yaml, &CancellationToken::new())
        .await;
    assert!(!bail_out.passed);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "end-of-scroll settle bailout should terminate immediately, took {:?}",
        started.elapsed()
    );
    assert!(
        bail_out
            .failed_step
            .as_ref()
            .unwrap()
            .message
            .contains("reached end of scroll")
    );
    assert_eq!(
        bail_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Selector)
    );
}

// ============================================================================
// Category 6: runFlow confinement/depth/cycle + ${VAR} scoping + when/repeat/retry
// ============================================================================

#[tokio::test]
async fn run_flow_confinement_cycles_env_scoping_when_repeat_and_retry() {
    let ws = TempDir::new("control-flow");

    // 1. EnvStack: single-pass non-recursive, ident regex, 64 KiB cap, child isolation.
    let mut base = BTreeMap::new();
    base.insert("A".to_string(), "${B}".to_string());
    base.insert("B".to_string(), "SECRET_NEVER_EXPAND".to_string());
    base.insert("OK_1".to_string(), "hello".to_string());
    let mut stack = EnvStack::new(base);
    assert_eq!(stack.interpolate("${A}").unwrap(), "${B}");
    assert_eq!(
        stack.interpolate("${OK_1} ${123} ${a-b}").unwrap(),
        "hello ${123} ${a-b}"
    );

    // Child frame does not leak to parent after pop.
    let mut child = BTreeMap::new();
    child.insert("OK_1".to_string(), "shadowed".to_string());
    child.insert("CHILD_ONLY".to_string(), "inside".to_string());
    stack.push(child);
    assert_eq!(
        stack.interpolate("${OK_1}-${CHILD_ONLY}").unwrap(),
        "shadowed-inside"
    );
    stack.pop();
    assert_eq!(
        stack.interpolate("${OK_1}-${CHILD_ONLY}").unwrap(),
        "hello-${CHILD_ONLY}"
    );

    // 64 KiB cap enforced.
    let mut big = BTreeMap::new();
    big.insert("BIG".to_string(), "x".repeat(MAX_INTERPOLATED_BYTES + 1));
    let big_stack = EnvStack::new(big);
    assert!(big_stack.interpolate("${BIG}").is_err());

    // 2. Child subflow env isolation + `when` + `repeat` + `retry`.
    fs::create_dir_all(ws.path().join("sub")).unwrap();
    fs::write(
        ws.path().join("sub/child.yaml"),
        r#"
- inputText: ${WHO}
"#,
    )
    .unwrap();

    let driver = MockDriver::full();
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "ready",
        "Ready",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]));
    // Make first 2 taps fail transiently so `retry` with `maxRetries: 2` succeeds on attempt 3!
    driver.state.lock().unwrap().fail_tap_count = 2;

    let main_yaml = r#"
appId: dev.deja.checkout
env:
  WHO: parent
---
- runFlow:
    file: sub/child.yaml
    env:
      WHO: child
- inputText: ${WHO}
- runFlow:
    when:
      platform: android
    commands:
      - inputText: should-not-run
- runFlow:
    when:
      platform: ios
      visible: "Ready"
    commands:
      - inputText: ios-ran
- repeat:
    times: 2
    commands:
      - pressKey: Tab
- retry:
    maxRetries: 2
    commands:
      - tapOn:
          id: ready
"#;
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver, main_yaml, &CancellationToken::new())
        .await;
    assert!(out.passed, "{:#?}", out.failed_step);

    let typed: Vec<String> = driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::InputText(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(typed, vec!["child", "parent", "ios-ran"]);
    let tabs = driver
        .events()
        .into_iter()
        .filter(|e| matches!(e, DriverEvent::PressKey(k) if k == "Tab"))
        .count();
    assert_eq!(tabs, 2);

    // 3. Confinement: escape via `../outside.yaml` must fail closed.
    let escape_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- runFlow: ../outside.yaml\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!escape_out.passed);
    assert_eq!(
        escape_out.failed_step.as_ref().unwrap().kind,
        StepFailureKind::ParseOrConfinement
    );

    // 4. Cycle detection (`a.yaml -> b.yaml -> a.yaml`).
    fs::write(ws.path().join("a.yaml"), "- runFlow: b.yaml\n").unwrap();
    fs::write(ws.path().join("b.yaml"), "- runFlow: a.yaml\n").unwrap();
    let cyc_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_file(&driver, Path::new("a.yaml"), &CancellationToken::new())
        .await;
    assert!(!cyc_out.passed);
    assert!(
        cyc_out
            .failed_step
            .as_ref()
            .unwrap()
            .message
            .contains("cycle")
    );

    // 5. Max depth > 16 fails closed.
    for i in 0..=17 {
        let content = if i <= 16 {
            format!("- runFlow: d{}.yaml\n", i + 1)
        } else {
            "- hideKeyboard\n".to_string()
        };
        fs::write(ws.path().join(format!("d{i}.yaml")), content).unwrap();
    }
    let depth_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_file(&driver, Path::new("d0.yaml"), &CancellationToken::new())
        .await;
    assert!(!depth_out.passed);
    assert!(
        depth_out
            .failed_step
            .as_ref()
            .unwrap()
            .message
            .contains("maximum depth")
    );

    // 6. Repeat/retry cap enforcement.
    let repeat_cap = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- repeat:\n    times: 1001\n    commands:\n      - hideKeyboard\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!repeat_cap.passed);
    assert_eq!(
        repeat_cap.failed_step.as_ref().unwrap().kind,
        StepFailureKind::LimitExceeded
    );

    let retry_cap = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- retry:\n    maxRetries: 21\n    commands:\n      - hideKeyboard\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!retry_cap.passed);
    assert_eq!(
        retry_cap.failed_step.as_ref().unwrap().kind,
        StepFailureKind::LimitExceeded
    );
}

// ============================================================================
// Category 7: UnsupportedCapability & UnsupportedCommand fail-closed
// ============================================================================

#[tokio::test]
async fn unsupported_capabilities_and_commands_fail_closed() {
    // 1. Parser fail-closed checks.
    let unsupported_flows = [
        ("runScript", "- runScript: login.js\n"),
        ("evalScript", "- evalScript: ${output.x = 1}\n"),
        ("assertScreenshot", "- assertScreenshot: golden.png\n"),
        ("assertNoDefectsWithAi", "- assertNoDefectsWithAi: {}\n"),
        ("startRecording", "- startRecording: out.mp4\n"),
        ("assertTrue", "- assertTrue: ${1 == 1}\n"),
        (
            "when.true",
            "- runFlow:\n    when:\n      true: ${foo == 'bar'}\n    commands:\n      - back\n",
        ),
        (
            "while.true",
            "- repeat:\n    while:\n      true: ${count < 3}\n    commands:\n      - back\n",
        ),
        ("magicTap", "- magicTap: \"Submit\"\n"),
    ];
    for (expected_cmd, yaml) in unsupported_flows {
        let err = parse_flow(yaml).expect_err(&format!("`{expected_cmd}` must fail closed"));
        assert!(
            matches!(err, FlowParseError::UnsupportedCommand { ref command, .. } if command == expected_cmd),
            "expected UnsupportedCommand({expected_cmd}), got {err:?}"
        );
    }

    // >2 YAML documents rejected.
    let three_docs = "appId: a\n---\n- launchApp\n---\n- stopApp\n";
    assert!(matches!(
        parse_flow(three_docs),
        Err(FlowParseError::TooManyDocuments(3))
    ));

    // 2. Driver without SystemBack or ClearKeychain fails closed with UnsupportedCapability.
    let ws = TempDir::new("unsupported-caps");
    let ios_like = MockDriver::with_caps(
        "ios",
        DriverCapabilities::with([DriverCapability::AccessibilityTree]),
    );
    let back_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &ios_like,
            "appId: dev.deja.checkout\n---\n- back\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!back_out.passed);
    assert_eq!(
        back_out.failed_step.as_ref().unwrap().kind,
        StepFailureKind::UnsupportedCapability
    );
    assert_eq!(
        back_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Runner)
    );
    assert!(!back_out.failure.as_ref().unwrap().repair_eligible);

    let kc_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &ios_like,
            "appId: dev.deja.checkout\n---\n- clearKeychain\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!kc_out.passed);
    assert_eq!(
        kc_out.failed_step.as_ref().unwrap().kind,
        StepFailureKind::UnsupportedCapability
    );
}

// ============================================================================
// Category 8: CancellationToken abort & flow timeout distinction
// ============================================================================

#[tokio::test]
async fn cancellation_token_aborts_promptly_and_skips_complete_hook() {
    let ws = TempDir::new("cancel-abort");
    let driver = Arc::new(MockDriver::full());
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "loading",
        "Loading...",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]));

    let yaml = r#"
appId: dev.deja.checkout
onFlowComplete:
  - stopApp
---
- extendedWaitUntil:
    visible: "NeverAppears"
    timeout: 10000
"#;

    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel_trigger.cancel();
    });

    let started = Instant::now();
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(driver.as_ref(), yaml, &cancel)
        .await;
    assert!(!out.passed);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        out.failed_step.as_ref().unwrap().kind,
        StepFailureKind::Cancelled
    );
    assert_eq!(
        out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Runner)
    );
    // onFlowComplete must be skipped on cancellation.
    assert!(
        !driver
            .events()
            .iter()
            .any(|e| matches!(e, DriverEvent::StopApp(_)))
    );

    // Distinct from overall flow_timeout -> FailureCategory::Timeout!
    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.flow_timeout = Some(Duration::from_millis(70));
    let timed_out = FlowExecutor::new(cfg)
        .execute_yaml(driver.as_ref(), yaml, &CancellationToken::new())
        .await;
    assert!(!timed_out.passed);
    assert_eq!(
        timed_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Timeout)
    );
}

// ============================================================================
// Category 9: DEJA-52 FailureClassifier precedence & assertion counts
// ============================================================================

#[tokio::test]
async fn deja52_failure_classifier_precedence_and_assertion_counts() {
    // 1. Passed -> None.
    assert!(
        FailureClassifier::classify(&ClassificationInput {
            passed: true,
            timed_out: false,
            failed_step: None,
            crash_signal: None,
            raw_artifact_path: None,
        })
        .is_none()
    );

    let crash = DriverCrashSignal {
        source: CrashSignalSource::CrashReport,
        report_file: Some("Checkout-2026-11-28.ips".to_string()),
        process: Some("Checkout".to_string()),
        detail: None,
    };

    // 2. ID-only assertVisible AND assertNotVisible -> ProductAssertion (repair_eligible: false).
    for (cmd, label) in [
        (
            "assertVisible",
            "Assert that id: order_confirmation is visible",
        ),
        (
            "assertVisible",
            "Assert that \"Order confirmed\" is visible",
        ),
        (
            "assertNotVisible",
            "Assert that id: error_banner is not visible",
        ),
    ] {
        let step = FailedStepContext {
            step_index: 3,
            command: cmd.to_string(),
            label: label.to_string(),
            kind: StepFailureKind::AssertionFailed,
            message: "assertion failed".to_string(),
            visible_context: None,
            crash_signal: None,
        };
        let f = FailureClassifier::classify(&ClassificationInput {
            passed: false,
            timed_out: false,
            failed_step: Some(&step),
            crash_signal: None,
            raw_artifact_path: Some("output.log".to_string()),
        })
        .unwrap();
        assert_eq!(f.category, FailureCategory::ProductAssertion);
        assert!(f.advisory);
        assert!(!f.repair_eligible);
    }

    // 3. Crash signal overrides selector/assertion/navigation symptoms.
    let sel_miss = FailedStepContext {
        step_index: 2,
        command: "tapOn".to_string(),
        label: "Tap on id: checkout_btn".to_string(),
        kind: StepFailureKind::SelectorNotFound,
        message: "not found".to_string(),
        visible_context: None,
        crash_signal: None,
    };
    let crash_classified = FailureClassifier::classify(&ClassificationInput {
        passed: false,
        timed_out: false,
        failed_step: Some(&sel_miss),
        crash_signal: Some(&crash),
        raw_artifact_path: None,
    })
    .unwrap();
    assert_eq!(crash_classified.category, FailureCategory::AppCrash);
    assert!(crash_classified.advisory);
    assert!(!crash_classified.repair_eligible);
    assert_eq!(
        crash_classified
            .crash
            .as_ref()
            .and_then(|c| c.report_file.as_deref()),
        Some("Checkout-2026-11-28.ips")
    );

    // 4. Without crash signal, tapOn selector miss -> Selector (repair_eligible: true).
    let sel_classified = FailureClassifier::classify(&ClassificationInput {
        passed: false,
        timed_out: false,
        failed_step: Some(&sel_miss),
        crash_signal: None,
        raw_artifact_path: None,
    })
    .unwrap();
    assert_eq!(sel_classified.category, FailureCategory::Selector);
    assert!(sel_classified.advisory);
    assert!(sel_classified.repair_eligible);

    // 5. LaunchApp / OpenLink navigation failure -> Navigation (repair_eligible: true).
    let nav_step = FailedStepContext {
        step_index: 0,
        command: "launchApp".to_string(),
        label: "Launch app \"dev.deja.checkout\"".to_string(),
        kind: StepFailureKind::NavigationFailed,
        message: "unable to launch".to_string(),
        visible_context: None,
        crash_signal: None,
    };
    let nav_classified = FailureClassifier::classify(&ClassificationInput {
        passed: false,
        timed_out: false,
        failed_step: Some(&nav_step),
        crash_signal: None,
        raw_artifact_path: None,
    })
    .unwrap();
    assert_eq!(nav_classified.category, FailureCategory::Navigation);
    assert!(nav_classified.advisory);
    assert!(nav_classified.repair_eligible);

    // 6. ExtendedWaitUntil -> Timeout (repair_eligible: false).
    let wait_step = FailedStepContext {
        step_index: 1,
        command: "extendedWaitUntil".to_string(),
        label: "Extended wait until \"Ready\" is visible".to_string(),
        kind: StepFailureKind::AssertionFailed,
        message: "timed out".to_string(),
        visible_context: None,
        crash_signal: None,
    };
    let wait_classified = FailureClassifier::classify(&ClassificationInput {
        passed: false,
        timed_out: false,
        failed_step: Some(&wait_step),
        crash_signal: None,
        raw_artifact_path: None,
    })
    .unwrap();
    assert_eq!(wait_classified.category, FailureCategory::Timeout);
    assert!(!wait_classified.repair_eligible);

    // 7. End-to-end crash check inside executor when launchApp fails + driver reports crash.
    let ws = TempDir::new("crash-e2e");
    let crash_driver = MockDriver::full();
    crash_driver.state.lock().unwrap().launch_error = Some("app died".to_string());
    crash_driver.state.lock().unwrap().crash_signal = Some(crash);
    let crash_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &crash_driver,
            "appId: dev.deja.checkout\n---\n- launchApp\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!crash_out.passed);
    assert_eq!(
        crash_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::AppCrash)
    );

    // 7b. Non-transient hierarchy transport error -> Runner (repair_eligible: false).
    let transport_driver = MockDriver::full();
    transport_driver.push_hierarchy_err("socket reset by peer");
    let transport_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &transport_driver,
            "appId: dev.deja.checkout\n---\n- assertVisible: \"Checkout\"\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!transport_out.passed);
    assert_eq!(
        transport_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Runner)
    );
    assert!(!transport_out.failure.as_ref().unwrap().repair_eligible);

    // 7c. OpenLink navigation failure -> Navigation (repair_eligible: true).
    let nav_driver = MockDriver::full();
    nav_driver.state.lock().unwrap().open_link_error = Some("invalid scheme".to_string());
    let link_out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &nav_driver,
            "appId: dev.deja.checkout\n---\n- openLink: \"bad://link\"\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!link_out.passed);
    assert_eq!(
        link_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Navigation)
    );
    assert!(link_out.failure.as_ref().unwrap().repair_eligible);

    // 8. Assertion counts on YAML and multi-file FlowClosure.
    let single_yaml = r#"
appId: dev.deja.checkout
---
- launchApp
- extendedWaitUntil:
    visible: "Cart"
    timeout: 3000
- assertVisible:
    id: cart_total
- assertNotVisible: "Empty"
"#;
    assert_eq!(count_assertions_in_flow_yaml(single_yaml).unwrap(), 2);

    fs::write(
        ws.path().join("main.yaml"),
        "appId: dev.deja.checkout\n---\n- runFlow: sub.yaml\n- assertVisible: \"Done\"\n",
    )
    .unwrap();
    fs::write(
        ws.path().join("sub.yaml"),
        "- assertVisible:\n    id: step1\n- assertNotVisible: \"Err\"\n",
    )
    .unwrap();
    let closure = collect_flow_closure(ws.path(), Path::new("main.yaml")).unwrap();
    assert_eq!(count_assertions_in_closure(&closure).unwrap(), 3);
}

// ============================================================================
// Category 10: Single, attempt, and merged JUnit redaction & errored fallback
// ============================================================================

#[test]
fn junit_single_attempts_merged_redaction_and_errored_fallback() {
    let ws = TempDir::new("junit");
    let junit_path = ws.path().join("reports/junit.xml");

    // 1. prepare_junit clears stale report.
    fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
    fs::write(&junit_path, "<stale/>").unwrap();
    prepare_junit(&junit_path).unwrap();
    assert!(!junit_path.exists());

    // 2. write_errored_junit writes atomic failure fallback.
    write_errored_junit(&junit_path, "checkout flow", "boom\x00\x07\nline2").unwrap();
    let errored_xml = fs::read_to_string(&junit_path).unwrap();
    assert!(reports_failure(&errored_xml));
    assert!(errored_xml.contains("status=\"ERROR\""));
    assert!(errored_xml.contains("value=\"deja-synthesized\""));
    assert!(!errored_xml.contains('\0'));
    assert!(!errored_xml.contains('\x07'));

    // 3. Entity-decoded pre-redaction, quoted `>`, structural attribute preservation, CDATA `]]>` split.
    let raw_secret = format!("{}live_1234567890abcdefghij", "sk-");
    let entity_secret = "&#x73;k-live_1234567890abcdefghij";
    let dirty_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="Suite > /tmp/ws-secret" tests="1" failures="1" errors="0" skipped="0" time="1.250">
    <testcase id="case1" name="a > b {entity_secret}" classname="cls" time="1.250" status="ERROR">
      <failure message="leaked {entity_secret} &amp; /tmp/ws-secret">
        body {entity_secret}
      </failure>
      <system-out><![CDATA[log {raw_secret}]]></system-out>
      <!-- comment {raw_secret} -- ok -->
    </testcase>
  </testsuite>
</testsuites>
"#
    );

    let redactor = create_redactor("/tmp/ws-secret", None, &["1".to_string()]);
    let provider = provider_key_redactor(std::iter::empty::<(&str, &str)>());
    let cleaned = redact_junit_with_redactors(&dirty_xml, &redactor, &provider);

    assert!(!cleaned.contains(&raw_secret));
    assert!(!cleaned.contains(entity_secret));
    assert!(!cleaned.contains("/tmp/ws-secret"));
    // Structural attributes preserved even though fixture value "1" was in redactor!
    assert!(cleaned.contains("tests=\"1\""));
    assert!(cleaned.contains("failures=\"1\""));
    assert!(cleaned.contains("time=\"1.250\""));
    assert!(cleaned.contains("status=\"ERROR\""));
    // CDATA `]]>` split guard prevents redacted content from breaking out of CDATA.
    let cdata_guarded = redact_junit("<system-out><![CDATA[TOKEN]]></system-out>", |s| {
        s.replace("TOKEN", "break]]>escape")
    });
    assert_eq!(
        cdata_guarded,
        "<system-out><![CDATA[break]]]]><![CDATA[>escape]]></system-out>"
    );
    // Custom redact closure smoke test preserving XML declaration.
    let via_fn = redact_junit(&cleaned, |s| s.to_string());
    assert!(via_fn.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));

    // 4. synthesized_junit pass vs fail + synthesized_attempts_junit.
    let pass_xml = synthesized_junit(&SynthesizedJunitInput {
        goal: "complete checkout",
        passed: true,
        reason: "",
        duration_ms: 1500,
    });
    assert!(!reports_failure(&pass_xml));
    assert!(pass_xml.contains("status=\"SUCCESS\""));

    let attempts = vec![
        RunAttempt {
            attempt: 1,
            passed: false,
            started_at: "2026-11-28T00:00:00Z".to_string(),
            ended_at: "2026-11-28T00:00:01Z".to_string(),
            duration_ms: 1000,
            failure: Some(WalkFailure {
                category: FailureCategory::AppCrash,
                advisory: true,
                repair_eligible: false,
                failing_step: Some("Tap on \"Checkout\"".to_string()),
                raw_artifact_path: Some(".deja/attempts/1/output.log".to_string()),
                crash: Some(deja_core::CrashSignal {
                    source: CrashSignalSource::CrashReport,
                    report_file: Some("Checkout.ips".to_string()),
                    process: Some("Checkout".to_string()),
                }),
            }),
            artifacts: Vec::new(),
        },
        RunAttempt {
            attempt: 2,
            passed: true,
            started_at: "2026-11-28T00:00:02Z".to_string(),
            ended_at: "2026-11-28T00:00:03Z".to_string(),
            duration_ms: 800,
            failure: None,
            artifacts: Vec::new(),
        },
    ];
    let attempts_xml = synthesized_attempts_junit(&SynthesizedAttemptsJunitInput {
        goal: "complete checkout",
        outcome: "flaky",
        attempts: &attempts,
        duration_ms: 1800,
    });
    assert!(reports_failure(&attempts_xml));
    assert!(attempts_xml.contains("tests=\"2\" failures=\"1\""));
    assert!(attempts_xml.contains("outcome\" value=\"flaky\""));
    assert!(attempts_xml.contains("crash signal: crash-report Checkout.ips"));

    // 5. merge_junit_suites aggregates counts & times across suites without CDATA confusion.
    let merged = merge_junit_suites(&[pass_xml.as_str(), attempts_xml.as_str()]);
    assert!(
        merged.contains("tests=\"3\" failures=\"1\" errors=\"0\" skipped=\"0\" time=\"3.300\"")
    );
    assert_eq!(merged.matches("<testsuite ").count(), 2);
}
