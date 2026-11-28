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
    ActAction, ActOutcome, ActPayload, ClassificationInput, CompiledSelector,
    DEFAULT_HOLD_SCROLL_MS, DriverCapabilities, DriverCapability, DriverCrashSignal, DriverError,
    DriverFuture, EnvStack, FailedStepContext, FailureClassifier, FlowExecutionConfig,
    FlowExecutor, FlowParseError, KeyPress, LaunchOptions, MAX_INTERPOLATED_BYTES,
    ObservationSource, OpenLinkOptions, PlatformDriver, PointSpec, Selector, SelectorError,
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
    OpenLink {
        url: String,
        auto_verify: Option<bool>,
        browser: Option<bool>,
    },
    AddMedia(Vec<PathBuf>),
    CheckAppCrash(String),
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
        hold_duration_ms: Option<u64>,
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
    crash_signals_by_app: BTreeMap<String, DriverCrashSignal>,
    hierarchy_delay: Option<Duration>,
    fail_hierarchy_once: bool,
    fail_all_hierarchies_transient: bool,
    /// Fails the first N taps with Selector-invisible screen before switching.
    fail_tap_count: usize,
    tap_calls: usize,
    /// Optional scripted outcomes returned by overridden `act_and_observe`.
    scripted_act_outcomes: VecDeque<ActOutcome>,
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
        opts: &'a OpenLinkOptions,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ()> {
        Box::pin(async move {
            let mut st = self.state.lock().unwrap();
            st.events.push(DriverEvent::OpenLink {
                url: url.to_string(),
                auto_verify: opts.auto_verify,
                browser: opts.browser,
            });
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
            if st.fail_all_hierarchies_transient {
                return Err(DriverError::Transport {
                    operation: "hierarchy".to_string(),
                    detail: "simulated persistent transient hierarchy error".to_string(),
                    transient: true,
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
                hold_duration_ms: params.hold_duration_ms,
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
        app_id: &'a str,
        _since_epoch_ms: u64,
        _cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, Option<DriverCrashSignal>> {
        Box::pin(async move {
            let mut st = self.state.lock().unwrap();
            st.events
                .push(DriverEvent::CheckAppCrash(app_id.to_string()));
            Ok(st
                .crash_signals_by_app
                .get(app_id)
                .cloned()
                .or_else(|| st.crash_signal.clone()))
        })
    }

    fn shutdown<'a>(&'a self) -> DriverFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn act_and_observe<'a>(
        &'a self,
        req: &'a ActPayload,
        cancel: &'a CancellationToken,
    ) -> DriverFuture<'a, ActOutcome> {
        if let Some(scripted) = self.state.lock().unwrap().scripted_act_outcomes.pop_front() {
            return Box::pin(async move { Ok(scripted) });
        }
        Box::pin(async move {
            // Delegate to standard primitive behavior + post-action observation when unscripted.
            let started = Instant::now();
            match &req.action {
                ActAction::Tap { x, y } => {
                    self.tap_point(&req.app_id, UiPoint::new(*x, *y), cancel)
                        .await?;
                }
                ActAction::LongPress { x, y, duration_ms } => {
                    self.long_press_point(&req.app_id, UiPoint::new(*x, *y), *duration_ms, cancel)
                        .await?;
                }
                ActAction::DoubleTap { x, y } => {
                    self.double_tap_point(&req.app_id, UiPoint::new(*x, *y), cancel)
                        .await?;
                }
                ActAction::Swipe {
                    start,
                    end,
                    duration_ms,
                    hold_duration_ms,
                } => {
                    self.swipe(
                        &SwipeParams {
                            app_id: req.app_id.clone(),
                            start: *start,
                            end: *end,
                            duration_ms: *duration_ms,
                            hold_duration_ms: *hold_duration_ms,
                        },
                        cancel,
                    )
                    .await?;
                }
                ActAction::InputText { text } => {
                    self.input_text(&req.app_id, text, cancel).await?;
                }
                ActAction::EraseText { count } => {
                    self.erase_text(&req.app_id, *count, cancel).await?;
                }
                ActAction::PressKey {
                    key,
                    modifiers,
                    count,
                } => {
                    self.press_key(
                        &req.app_id,
                        &KeyPress {
                            key: key.clone(),
                            modifiers: modifiers.clone(),
                            count: *count,
                        },
                        cancel,
                    )
                    .await?;
                }
                ActAction::HideKeyboard => self.hide_keyboard(&req.app_id, cancel).await?,
                ActAction::Back => self.back(&req.app_id, cancel).await?,
            }
            let (hierarchy, hierarchy_error) = if req.return_hierarchy {
                match self
                    .hierarchy(
                        &req.app_id,
                        req.include_system_ui,
                        req.known_hash.as_deref(),
                        cancel,
                    )
                    .await
                {
                    Ok(h) => (Some(h), None),
                    Err(DriverError::Cancelled) => return Err(DriverError::Cancelled),
                    Err(err) => (None, Some(err.to_string())),
                }
            } else {
                (None, None)
            };
            Ok(ActOutcome {
                action_ok: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
                hierarchy,
                hierarchy_error,
            })
        })
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
    let buy_visible = CompiledSelector::compile(&Selector::by_text("Buy"))
        .unwrap()
        .match_all(&tie_nodes);
    let visible_ordered: Vec<&str> = buy_visible.iter().map(|n| n.identifier.as_str()).collect();
    assert_eq!(visible_ordered, vec!["dup_hit_a", "dup_hit_b", "dup_vis"]);

    let buy_all_raw = CompiledSelector::compile(&Selector::by_text("Buy"))
        .unwrap()
        .match_all_including_offscreen(&tie_nodes);
    let raw_ordered: Vec<&str> = buy_all_raw.iter().map(|n| n.identifier.as_str()).collect();
    assert_eq!(
        raw_ordered,
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
            failed_app_id: None,
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
        failed_app_id: None,
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
        failed_app_id: None,
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
        failed_app_id: None,
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

// ============================================================================
// Codex Cross-Review Round 1 Regression Tests (Findings 1-14)
// ============================================================================

#[tokio::test]
async fn round1_redacts_failure_labels_in_outcomes_and_classifier() {
    let ws = TempDir::new("round1-redact-labels");
    let driver = MockDriver::full();
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]));

    let fake_key = format!("{}live_0123456789abcdefghijkl", "sk-");
    let secret_url = "https://alice:superSecretPass99@example.com/pay";
    let ws_str = ws.path().to_string_lossy().into_owned();

    let redactor = create_redactor(&ws_str, None, &["fixtureSecretVal".to_string()]);
    let provider = provider_key_redactor([("API_TOKEN", "envSecretToken987654")]);

    // 1. Generated assertVisible label containing secrets + workspace path + fixture value.
    let yaml_generated = format!(
        r#"
appId: dev.deja.checkout
onFlowComplete:
  - assertVisible:
      text: "Hook {fake_key}"
      label: "Hook label {secret_url}"
---
- assertVisible: "Expected {fake_key} fixtureSecretVal at {ws_str}"
"#
    );
    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(40);
    cfg.redactor = redactor.clone();
    cfg.provider_redactor = provider.clone();
    let out = FlowExecutor::new(cfg)
        .execute_yaml(&driver, &yaml_generated, &CancellationToken::new())
        .await;
    assert!(!out.passed);
    let serialized = serde_json::to_string(&out).unwrap();
    assert!(
        !serialized.contains(&fake_key),
        "provider key leaked: {serialized}"
    );
    assert!(
        !serialized.contains("fixtureSecretVal"),
        "fixture secret leaked: {serialized}"
    );
    assert!(
        !serialized.contains(&ws_str),
        "workspace path leaked: {serialized}"
    );
    assert!(
        !serialized.contains("superSecretPass99"),
        "URL password in hook label leaked: {serialized}"
    );
    assert!(
        out.failure
            .as_ref()
            .and_then(|f| f.failing_step.as_deref())
            .is_some_and(
                |s| s.contains("<redacted-secret>") && s.contains("<redacted-fixture-value>")
            )
    );

    // 2. User-provided `label:` on tapOn containing URL password + fixture value.
    let yaml_custom_label = format!(
        r#"
appId: dev.deja.checkout
---
- tapOn:
    id: missing_btn
    label: "Tap {secret_url} with fixtureSecretVal"
"#
    );
    let mut cfg2 = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg2.default_step_timeout = Duration::from_millis(40);
    cfg2.redactor = redactor;
    cfg2.provider_redactor = provider;
    let out2 = FlowExecutor::new(cfg2)
        .execute_yaml(&driver, &yaml_custom_label, &CancellationToken::new())
        .await;
    assert!(!out2.passed);
    let serialized2 = serde_json::to_string(&out2).unwrap();
    assert!(!serialized2.contains("superSecretPass99"));
    assert!(!serialized2.contains("fixtureSecretVal"));
    assert!(
        out2.failed_step
            .as_ref()
            .is_some_and(|s| s.label.contains("<redacted-secret>")
                && s.label.contains("<redacted-fixture-value>"))
    );
}

#[tokio::test]
async fn round1_offscreen_nodes_excluded_before_index_across_visibility_checks() {
    let ws = TempDir::new("round1-offscreen");
    let driver = MockDriver::full();

    let mut off_row = node(
        1,
        None,
        "row_btn",
        "Row",
        UiRect::new(0.0, -200.0, 100.0, 40.0),
    );
    off_row.is_on_screen = false;
    let on_row_first = node(
        2,
        None,
        "row_btn",
        "Row",
        UiRect::new(20.0, 100.0, 100.0, 40.0),
    );
    let on_row_second = node(
        3,
        None,
        "row_btn",
        "Row",
        UiRect::new(20.0, 200.0, 100.0, 40.0),
    );
    let mut off_only = node(
        4,
        None,
        "only_offscreen",
        "Offscreen Only",
        UiRect::new(0.0, 1500.0, 100.0, 40.0),
    );
    off_only.is_on_screen = false;

    driver.set_screen(hierarchy_of(vec![
        off_row,
        on_row_first,
        on_row_second,
        off_only,
    ]));

    // 1. tapOn with index: 0 skips off-screen node at tree pos 0 and taps on_row_first (center 70, 120).
    // 2. assertNotVisible on off-screen-only element passes.
    // 3. when.visible on off-screen element is skipped; when.notVisible runs.
    let yaml_ok = r#"
appId: dev.deja.checkout
---
- tapOn:
    id: row_btn
    index: 0
- assertNotVisible:
    id: only_offscreen
- runFlow:
    when:
      visible:
        id: only_offscreen
    commands:
      - inputText: should-not-run
- runFlow:
    when:
      notVisible:
        id: only_offscreen
    commands:
      - inputText: offscreen-is-not-visible
"#;
    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(80);
    let out = FlowExecutor::new(cfg.clone())
        .execute_yaml(&driver, yaml_ok, &CancellationToken::new())
        .await;
    assert!(out.passed, "{:#?}", out.failed_step);
    let events = driver.events();
    assert!(events.contains(&DriverEvent::Tap(70.0, 120.0)));
    assert!(events.contains(&DriverEvent::InputText(
        "offscreen-is-not-visible".to_string()
    )));
    assert!(!events.contains(&DriverEvent::InputText("should-not-run".to_string())));

    // 4. assertVisible on off-screen-only element MUST fail!
    let out_fail = FlowExecutor::new(cfg)
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- assertVisible:\n    id: only_offscreen\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!out_fail.passed);
    assert_eq!(
        out_fail.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::ProductAssertion)
    );
}

#[tokio::test]
async fn round1_literal_regex_syntax_and_url_only_web_checkout() {
    let ws = TempDir::new("round1-web-checkout");
    let driver = MockDriver::with_caps(
        "web",
        DriverCapabilities::with([DriverCapability::AccessibilityTree]),
    );

    // Exact copy of the checked-in `spikes/002-web-deja/flows/checkout.yaml` shape:
    // URL-only header (no `appId:`), multi-byte UTF-8, and `$5.00 · 6 stickers` / `Total: $5.00`!
    let login_screen = hierarchy_of(vec![
        node(
            0,
            None,
            "screen_login",
            "Déjà Web Shop",
            UiRect::new(0.0, 0.0, 1280.0, 720.0),
        ),
        node(
            1,
            Some(0),
            "username",
            "qa@example.com",
            UiRect::new(100.0, 100.0, 200.0, 40.0),
        ),
        node(
            2,
            Some(0),
            "password",
            "Password",
            UiRect::new(100.0, 160.0, 200.0, 40.0),
        ),
        node(
            3,
            Some(0),
            "login_button",
            "Sign in",
            UiRect::new(100.0, 220.0, 120.0, 40.0),
        ),
    ]);
    let catalog_screen = hierarchy_of(vec![
        node(
            0,
            None,
            "screen_catalog",
            "Mochi sticker pack",
            UiRect::new(0.0, 0.0, 1280.0, 720.0),
        ),
        node(
            1,
            Some(0),
            "price_line",
            "$5.00 · 6 stickers",
            UiRect::new(100.0, 120.0, 220.0, 30.0),
        ),
        node(
            2,
            Some(0),
            "usd_note",
            "$5.00 (USD) [promo",
            UiRect::new(100.0, 160.0, 220.0, 30.0),
        ),
        node(
            3,
            Some(0),
            "add_to_cart",
            "Add to cart",
            UiRect::new(100.0, 220.0, 150.0, 40.0),
        ),
    ]);
    let cart_screen = hierarchy_of(vec![
        node(
            0,
            None,
            "screen_cart",
            "1 × Mochi sticker pack",
            UiRect::new(0.0, 0.0, 1280.0, 720.0),
        ),
        node(
            1,
            Some(0),
            "cart_total",
            "Total: $5.00",
            UiRect::new(100.0, 160.0, 200.0, 30.0),
        ),
        node(
            2,
            Some(0),
            "checkout_button",
            "Checkout",
            UiRect::new(100.0, 220.0, 150.0, 40.0),
        ),
    ]);
    let done_screen = hierarchy_of(vec![
        node(
            0,
            None,
            "order_confirmation",
            "Order confirmed",
            UiRect::new(0.0, 0.0, 1280.0, 720.0),
        ),
        node(
            1,
            Some(0),
            "order_summary",
            "Order WC-1001 · Mochi sticker pack",
            UiRect::new(100.0, 140.0, 320.0, 30.0),
        ),
    ]);

    driver.set_screen(login_screen.clone());
    driver.push_tap_screen(login_screen.clone());
    driver.push_tap_screen(login_screen);
    driver.push_tap_screen(catalog_screen);
    driver.push_tap_screen(cart_screen);
    driver.push_tap_screen(done_screen);

    let web_checkout_yaml = r#"
url: http://127.0.0.1:4174
---
- openLink: http://127.0.0.1:4174
- assertVisible:
    id: screen_login
- assertVisible: Déjà Web Shop
- tapOn:
    id: username
- inputText: qa@example.com
- assertVisible: qa@example.com
- tapOn:
    id: password
- inputText: testing123
- tapOn:
    id: login_button
- assertVisible:
    id: screen_catalog
- assertVisible: Mochi sticker pack
- assertVisible: $5.00 · 6 stickers
- assertVisible: "$5.00 (USD) [promo"
- tapOn:
    id: add_to_cart
- assertVisible:
    id: screen_cart
- assertVisible: 1 × Mochi sticker pack
- assertVisible: "Total: $5.00"
- tapOn:
    id: checkout_button
- assertVisible:
    id: order_confirmation
- assertVisible: Order confirmed
- assertVisible: "Order WC-1001 · Mochi sticker pack"
"#;

    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(300);
    let out = FlowExecutor::new(cfg.clone())
        .execute_yaml(&driver, web_checkout_yaml, &CancellationToken::new())
        .await;
    assert!(
        out.passed,
        "URL-only web checkout failed: {:#?}",
        out.failed_step
    );
    assert_eq!(out.app_id.as_deref(), Some("http://127.0.0.1:4174"));

    // Bare 1-doc web flow (no `appId:` and no `url:`) also works on `platform == "web"`.
    let bare_web = FlowExecutor::new(cfg)
        .execute_yaml(
            &driver,
            "- assertVisible: Order confirmed\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(bare_web.passed, "{:#?}", bare_web.failed_step);
}

#[tokio::test]
async fn round1_unchanged_cache_hit_preserves_fresh_truncation_metadata() {
    let ws = TempDir::new("round1-unchanged-trunc");
    let driver = MockDriver::full();

    let complete = hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]);
    let hash = complete.hierarchy_hash.clone();

    let unchanged_truncated = UiHierarchy {
        app_id: "dev.deja.checkout".to_string(),
        state: "running_background".to_string(),
        viewport: Some(UiSize::new(390.0, 844.0)),
        truncated: true,
        hierarchy_hash: hash,
        unchanged: true,
        nodes: Vec::new(),
    };
    driver.set_screen(unchanged_truncated);

    let mut req = WaitSelectorRequest::not_visible(
        "dev.deja.checkout",
        Selector::by_id("missing_banner"),
        Duration::from_millis(70),
    );
    req.initial_hierarchy = Some(complete.clone());

    let outcome = wait_local(&driver, &req, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !outcome.matched,
        "unchanged: true with truncated: true MUST NOT falsely pass assertNotVisible"
    );
    let returned = outcome.hierarchy.expect("hierarchy returned");
    assert!(
        returned.truncated,
        "fresh truncated: true must be preserved"
    );
    assert_eq!(returned.state, "running_background");
    assert_eq!(returned.nodes.len(), 1, "cached nodes must still be reused");

    // Also verify `fetch_fresh_hierarchy` (via `when.notVisible`) preserves `truncated: true`.
    let driver2 = MockDriver::full();
    driver2.push_hierarchy(complete.clone());
    driver2.push_hierarchy(UiHierarchy {
        app_id: "dev.deja.checkout".to_string(),
        state: "running_foreground".to_string(),
        viewport: None,
        truncated: true,
        hierarchy_hash: complete.hierarchy_hash,
        unchanged: true,
        nodes: Vec::new(),
    });
    let yaml = r#"
appId: dev.deja.checkout
---
- assertVisible: "Home"
- runFlow:
    when:
      notVisible: "MissingBanner"
    commands:
      - inputText: should-not-run-on-truncated
"#;
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver2, yaml, &CancellationToken::new())
        .await;
    assert!(out.passed, "{:#?}", out.failed_step);
    assert!(!driver2.events().contains(&DriverEvent::InputText(
        "should-not-run-on-truncated".to_string()
    )));
}

#[test]
fn round1_junit_multibyte_utf8_scans_without_panicking() {
    let utf8_pass = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="Déjà — café" tests="1" failures="0" errors="0" skipped="0" time="0.500">
    <testcase id="c1" name="Déjà checkout — €5" classname="web" time="0.500" status="SUCCESS">
      <system-out>Déjà — café 🎉</system-out>
      <![CDATA[Déjà — café 🎉]]>
      <!-- Déjà — café -->
    </testcase>
  </testsuite>
</testsuites>
"#;
    let utf8_fail = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="Café suite" tests="1" failures="1" errors="0" skipped="0" time="0.750">
    <testcase id="c2" name="Order €5" classname="web" time="0.750" status="ERROR">
      <failure message="Déjà — café">expected Déjà — café</failure>
      <system-out>Déjà — café</system-out>
    </testcase>
  </testsuite>
</testsuites>
"#;

    assert!(!reports_failure(utf8_pass));
    assert!(reports_failure(utf8_fail));

    let merged = merge_junit_suites(&[utf8_pass, utf8_fail]);
    assert!(
        merged.contains("tests=\"2\" failures=\"1\" errors=\"0\" skipped=\"0\" time=\"1.250\"")
    );
    assert!(merged.contains("Déjà — café 🎉"));
    assert!(reports_failure(&merged));

    let redacted = redact_junit(utf8_pass, |s| s.replace("café", "espresso"));
    assert!(redacted.contains("Déjà — espresso 🎉"));
}

#[tokio::test]
async fn round1_header_env_interpolates_app_id_and_child_flow_scopes_app_id() {
    let ws = TempDir::new("round1-header-app-scope");
    fs::write(
        ws.path().join("child.yaml"),
        r#"
appId: dev.deja.child
---
- launchApp
"#,
    )
    .unwrap();
    let parent_yaml = r#"
appId: ${TARGET_APP}
env:
  TARGET_APP: dev.deja.parent
---
- launchApp
- runFlow: child.yaml
- stopApp
"#;
    fs::write(ws.path().join("parent.yaml"), parent_yaml).unwrap();

    // 1. Via `execute_yaml`: root `appId` interpolated after header `env`; child scopes & restores parent!
    let driver_yaml = MockDriver::full();
    let out_yaml = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver_yaml, parent_yaml, &CancellationToken::new())
        .await;
    assert!(out_yaml.passed, "{:#?}", out_yaml.failed_step);
    let events_yaml = driver_yaml.events();
    assert!(events_yaml.contains(&DriverEvent::LaunchApp {
        app_id: "dev.deja.parent".to_string(),
        clear_state: false,
        clear_keychain: false,
    }));
    assert!(events_yaml.contains(&DriverEvent::LaunchApp {
        app_id: "dev.deja.child".to_string(),
        clear_state: false,
        clear_keychain: false,
    }));
    assert!(events_yaml.contains(&DriverEvent::StopApp("dev.deja.parent".to_string())));

    // 2. Via `execute_file`: identical child appId scoping & restoration!
    let driver_file = MockDriver::full();
    let out_file = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_file(
            &driver_file,
            Path::new("parent.yaml"),
            &CancellationToken::new(),
        )
        .await;
    assert!(out_file.passed, "{:#?}", out_file.failed_step);
    assert_eq!(driver_file.events(), events_yaml);

    // 3. Explicit `config.app_id_override` takes precedence over root header `appId`.
    let driver_override = MockDriver::full();
    let mut cfg_override = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg_override.app_id_override = Some("dev.deja.override".to_string());
    let out_override = FlowExecutor::new(cfg_override)
        .execute_yaml(&driver_override, parent_yaml, &CancellationToken::new())
        .await;
    assert!(out_override.passed, "{:#?}", out_override.failed_step);
    let events_override = driver_override.events();
    assert!(events_override.contains(&DriverEvent::LaunchApp {
        app_id: "dev.deja.override".to_string(),
        clear_state: false,
        clear_keychain: false,
    }));
    assert!(events_override.contains(&DriverEvent::LaunchApp {
        app_id: "dev.deja.child".to_string(),
        clear_state: false,
        clear_keychain: false,
    }));
    assert!(events_override.contains(&DriverEvent::StopApp("dev.deja.override".to_string())));
}

#[tokio::test]
async fn round1_selector_deadline_bounds_slow_hierarchy_and_zero_timeout_single_probe() {
    let slow_driver = MockDriver::full();
    slow_driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "target",
        "Target",
        UiRect::new(0.0, 0.0, 100.0, 40.0),
    )]));
    slow_driver.state.lock().unwrap().hierarchy_delay = Some(Duration::from_millis(250));

    // In-flight hierarchy taking 250ms MUST be cut off at the 60ms selector deadline!
    let req = WaitSelectorRequest::visible(
        "dev.deja.checkout",
        Selector::by_id("target"),
        Duration::from_millis(60),
    );
    let started = Instant::now();
    let outcome = wait_local(&slow_driver, &req, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !outcome.matched,
        "match completing after deadline must be rejected"
    );
    assert!(
        started.elapsed() < Duration::from_millis(180),
        "wait_local exceeded deadline bound: {:?}",
        started.elapsed()
    );

    // `timeout == 0` preserves a single immediate probe!
    let fast_driver = MockDriver::full();
    fast_driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "target",
        "Target",
        UiRect::new(0.0, 0.0, 100.0, 40.0),
    )]));
    let zero_req = WaitSelectorRequest::visible(
        "dev.deja.checkout",
        Selector::by_id("target"),
        Duration::ZERO,
    );
    let single = wait_local(&fast_driver, &zero_req, &CancellationToken::new())
        .await
        .unwrap();
    assert!(single.matched);
    assert_eq!(single.polls, 1);
}

#[test]
fn round1_nested_relational_selector_honors_index() {
    let row0 = node(1, None, "row", "", UiRect::new(0.0, 100.0, 390.0, 60.0));
    let btn0 = node(
        2,
        Some(1),
        "buy_a",
        "Buy",
        UiRect::new(20.0, 110.0, 100.0, 40.0),
    );
    let row1 = node(3, None, "row", "", UiRect::new(0.0, 180.0, 390.0, 60.0));
    let btn1 = node(
        4,
        Some(3),
        "buy_b",
        "Buy",
        UiRect::new(20.0, 190.0, 100.0, 40.0),
    );
    let nodes = [row0, btn0, row1, btn1];

    let sel = Selector {
        text: Some("Buy".to_string()),
        child_of: Some(Box::new(Selector {
            id: Some("row".to_string()),
            index: Some(1),
            ..Selector::default()
        })),
        ..Selector::default()
    };
    let matched = CompiledSelector::compile(&sel)
        .unwrap()
        .match_one(&nodes)
        .expect("should match child of 2nd row");
    assert_eq!(matched.identifier, "buy_b");
}

#[tokio::test]
async fn round1_compound_act_propagates_action_ok_false_and_preserves_partial_success() {
    let ws = TempDir::new("round1-compound-act");
    let driver = MockDriver::with_caps(
        "ios",
        DriverCapabilities::with([
            DriverCapability::AccessibilityTree,
            DriverCapability::CompoundAct,
        ]),
    );
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "submit_btn",
        "Submit",
        UiRect::new(20.0, 200.0, 100.0, 44.0),
    )]));

    // 1. `action_ok: false` MUST fail execution instead of silently succeeding.
    driver
        .state
        .lock()
        .unwrap()
        .scripted_act_outcomes
        .push_back(ActOutcome {
            action_ok: false,
            elapsed_ms: 5,
            hierarchy: None,
            hierarchy_error: Some("synthesized tap failed".to_string()),
        });
    let out_fail = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- tapOn:\n    id: submit_btn\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!out_fail.passed);
    assert_eq!(
        out_fail.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Runner)
    );
    assert!(
        out_fail
            .failed_step
            .as_ref()
            .is_some_and(|s| s.message.contains("synthesized tap failed"))
    );

    // 2. `action_ok: true` with `hierarchy_error: Some(...)` preserves partial success.
    driver
        .state
        .lock()
        .unwrap()
        .scripted_act_outcomes
        .push_back(ActOutcome {
            action_ok: true,
            elapsed_ms: 5,
            hierarchy: None,
            hierarchy_error: Some("ax snapshot timed out".to_string()),
        });
    let out_ok = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- tapOn:\n    id: submit_btn\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(out_ok.passed, "{:#?}", out_ok.failed_step);
}

#[tokio::test]
async fn round1_action_settled_fail_fast_vs_explicit_wait_timeout() {
    let ws = TempDir::new("round1-fail-fast");
    let driver = MockDriver::full();
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]));

    // 1. Missing `tapOn` target fails fast on settled screen well before 5s timeout!
    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_secs(5);
    cfg.action_fail_fast_settle_polls = Some(3);
    let started_tap = Instant::now();
    let tap_out = FlowExecutor::new(cfg.clone())
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- tapOn:\n    id: nonexistent_btn\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!tap_out.passed);
    assert_eq!(
        tap_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Selector)
    );
    assert!(
        started_tap.elapsed() < Duration::from_millis(600),
        "missing tap target should fail fast on settled screen, took {:?}",
        started_tap.elapsed()
    );

    // 2. Explicit `extendedWaitUntil` ignores action fail-fast and waits the full timeout!
    let started_wait = Instant::now();
    let wait_out = FlowExecutor::new(cfg.clone())
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- extendedWaitUntil:\n    visible: \"Later\"\n    timeout: 180\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!wait_out.passed);
    assert_eq!(
        wait_out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Timeout)
    );
    assert!(
        started_wait.elapsed() >= Duration::from_millis(150),
        "explicit extendedWaitUntil must not fail fast early: {:?}",
        started_wait.elapsed()
    );

    // 3. Truncated screen NEVER triggers settled fail-fast on tapOn!
    let trunc_driver = MockDriver::full();
    let mut trunc = hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(0.0, 0.0, 390.0, 844.0),
    )]);
    trunc.truncated = true;
    trunc_driver.set_screen(trunc);
    let mut short_cfg = cfg;
    short_cfg.default_step_timeout = Duration::from_millis(160);
    let started_trunc = Instant::now();
    let trunc_out = FlowExecutor::new(short_cfg)
        .execute_yaml(
            &trunc_driver,
            "appId: dev.deja.checkout\n---\n- tapOn:\n    id: nonexistent_btn\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!trunc_out.passed);
    assert!(
        started_trunc.elapsed() >= Duration::from_millis(130),
        "truncated screen must not fail fast early: {:?}",
        started_trunc.elapsed()
    );
}

#[tokio::test]
async fn round1_retry_tap_if_no_change_and_hold_scroll_capability() {
    let ws = TempDir::new("round1-retry-tap-and-hold");

    // 1. `retryTapIfNoChange: true` with unchanged screen after 1st tap -> issues 1 bounded retry tap!
    let driver_no_change = MockDriver::full();
    let screen_a = hierarchy_of(vec![node(
        0,
        None,
        "flaky_btn",
        "Continue",
        UiRect::new(20.0, 300.0, 200.0, 44.0),
    )]);
    let screen_b = hierarchy_of(vec![node(
        0,
        None,
        "next_screen",
        "Done",
        UiRect::new(20.0, 300.0, 200.0, 44.0),
    )]);
    driver_no_change.set_screen(screen_a.clone());
    driver_no_change.push_tap_screen(screen_a.clone());
    driver_no_change.push_tap_screen(screen_b.clone());

    let retry_yaml = r#"
appId: dev.deja.checkout
---
- tapOn:
    id: flaky_btn
    retryTapIfNoChange: true
- assertVisible:
    id: next_screen
"#;
    let out_retry = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver_no_change, retry_yaml, &CancellationToken::new())
        .await;
    assert!(out_retry.passed, "{:#?}", out_retry.failed_step);
    let taps_no_change = driver_no_change
        .events()
        .into_iter()
        .filter(|e| matches!(e, DriverEvent::Tap(_, _)))
        .count();
    assert_eq!(
        taps_no_change, 2,
        "expected 1 initial tap + 1 bounded retry tap"
    );
    assert_eq!(out_retry.timing.steps[0].retries, 1);

    // When 1st tap immediately changes the screen, NO retry tap is issued.
    let driver_changed = MockDriver::full();
    driver_changed.set_screen(screen_a);
    driver_changed.push_tap_screen(screen_b);
    let out_changed = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver_changed, retry_yaml, &CancellationToken::new())
        .await;
    assert!(out_changed.passed, "{:#?}", out_changed.failed_step);
    let taps_changed = driver_changed
        .events()
        .into_iter()
        .filter(|e| matches!(e, DriverEvent::Tap(_, _)))
        .count();
    assert_eq!(taps_changed, 1);
    assert_eq!(out_changed.timing.steps[0].retries, 0);

    // 2. `HoldScroll` capability: `scrollUntilVisible` passes `hold_duration_ms: Some(150)`,
    // while fast `swipe` keeps `hold_duration_ms: None`!
    let hold_driver = MockDriver::with_caps(
        "ios",
        DriverCapabilities::with([
            DriverCapability::AccessibilityTree,
            DriverCapability::HoldScroll,
        ]),
    );
    hold_driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "top_row",
        "Top",
        UiRect::new(0.0, 100.0, 390.0, 40.0),
    )]));
    hold_driver.push_swipe_screen(hierarchy_of(vec![node(
        1,
        None,
        "found_row",
        "Found",
        UiRect::new(20.0, 400.0, 350.0, 40.0),
    )]));

    let scroll_and_swipe = r#"
appId: dev.deja.checkout
---
- scrollUntilVisible:
    element:
      id: found_row
    direction: DOWN
    timeout: 2000
- swipe:
    direction: UP
"#;
    let out_hold = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&hold_driver, scroll_and_swipe, &CancellationToken::new())
        .await;
    assert!(out_hold.passed, "{:#?}", out_hold.failed_step);
    let hold_values: Vec<Option<u64>> = hold_driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::Swipe {
                hold_duration_ms, ..
            } => Some(hold_duration_ms),
            _ => None,
        })
        .collect();
    assert_eq!(
        hold_values,
        vec![Some(DEFAULT_HOLD_SCROLL_MS), None],
        "scrollUntilVisible must set hold_duration_ms=Some(150) on HoldScroll drivers while fast swipe stays None"
    );

    // Driver WITHOUT `HoldScroll` keeps `hold_duration_ms: None` even for `scrollUntilVisible`.
    let plain_driver = MockDriver::full();
    plain_driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "top_row",
        "Top",
        UiRect::new(0.0, 100.0, 390.0, 40.0),
    )]));
    plain_driver.push_swipe_screen(hierarchy_of(vec![node(
        1,
        None,
        "found_row",
        "Found",
        UiRect::new(20.0, 400.0, 350.0, 40.0),
    )]));
    let out_plain = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&plain_driver, scroll_and_swipe, &CancellationToken::new())
        .await;
    assert!(out_plain.passed, "{:#?}", out_plain.failed_step);
    let plain_holds: Vec<Option<u64>> = plain_driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::Swipe {
                hold_duration_ms, ..
            } => Some(hold_duration_ms),
            _ => None,
        })
        .collect();
    assert_eq!(plain_holds, vec![None, None]);
}

// ============================================================================
// Codex Cross-Review Round 2 Regression Tests (Findings 1-9)
// ============================================================================

#[cfg(unix)]
#[tokio::test]
async fn round2_lexical_symlink_resolution_matches_flow_closure() {
    use std::os::unix::fs::symlink;

    let ws = TempDir::new("round2-lexical-symlink");
    fs::create_dir_all(ws.path().join("real")).unwrap();
    fs::create_dir_all(ws.path().join("alias")).unwrap();

    fs::write(
        ws.path().join("real/entry.yaml"),
        r#"
appId: dev.deja.checkout
---
- runFlow: helper.yaml
- addMedia:
    - pic.png
"#,
    )
    .unwrap();
    fs::write(
        ws.path().join("real/helper.yaml"),
        "- inputText: from-real-dir\n",
    )
    .unwrap();
    fs::write(ws.path().join("real/pic.png"), b"REAL").unwrap();

    fs::write(
        ws.path().join("alias/helper.yaml"),
        "- inputText: from-alias-dir\n",
    )
    .unwrap();
    fs::write(ws.path().join("alias/pic.png"), b"ALIAS").unwrap();

    symlink("../real/entry.yaml", ws.path().join("alias/entry.yaml")).unwrap();
    fs::write(
        ws.path().join("root.yaml"),
        "appId: dev.deja.checkout\n---\n- runFlow: alias/entry.yaml\n",
    )
    .unwrap();

    // Closure collector resolves relative references from the symlink's own lexical directory.
    let closure = collect_flow_closure(ws.path(), Path::new("alias/entry.yaml"))
        .expect("closure for symlinked entry");
    let rels: Vec<String> = closure.files.iter().map(|f| f.source.clone()).collect();
    assert!(
        rels.iter().any(|r| r == "alias/helper.yaml"),
        "expected alias/helper.yaml in closure, got {rels:?}"
    );
    assert!(
        rels.iter().any(|r| r == "alias/pic.png"),
        "expected alias/pic.png in closure, got {rels:?}"
    );
    assert!(
        !rels.iter().any(|r| r == "real/helper.yaml"),
        "must not resolve helper against symlink target directory: {rels:?}"
    );

    let canonical_alias_pic = fs::canonicalize(ws.path().join("alias/pic.png")).unwrap();

    // 1. Direct root `execute_file` on in-workspace symlink resolves `helper.yaml` & `pic.png`
    // relative to `alias/`, matching `collect_flow_closure`.
    let driver_root = MockDriver::full();
    let out_root = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_file(
            &driver_root,
            Path::new("alias/entry.yaml"),
            &CancellationToken::new(),
        )
        .await;
    assert!(out_root.passed, "{:#?}", out_root.failed_step);
    let typed_root: Vec<String> = driver_root
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::InputText(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(typed_root, vec!["from-alias-dir"]);
    let media_root: Vec<PathBuf> = driver_root
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::AddMedia(paths) => Some(paths),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(media_root, vec![canonical_alias_pic.clone()]);

    // 2. Nested `runFlow: alias/entry.yaml` from `root.yaml` also resolves `helper.yaml` & `pic.png`
    // relative to `alias/` (NOT `real/`).
    let driver_nested = MockDriver::full();
    let out_nested = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_file(
            &driver_nested,
            Path::new("root.yaml"),
            &CancellationToken::new(),
        )
        .await;
    assert!(out_nested.passed, "{:#?}", out_nested.failed_step);
    let typed_nested: Vec<String> = driver_nested
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::InputText(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(typed_nested, vec!["from-alias-dir"]);
    let media_nested: Vec<PathBuf> = driver_nested
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::AddMedia(paths) => Some(paths),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(media_nested, vec![canonical_alias_pic]);
}

#[tokio::test]
async fn round2_ios_json_offscreen_state_preserved() {
    let ws = TempDir::new("round2-ios-offscreen");

    // Runner-shaped JSON payload with:
    // - explicit clipped off-screen node (`isOnScreen: false`)
    // - legacy payload node omitting `isOnScreen` with `hittable: false`
    // - disabled on-screen node (`isOnScreen: true, hittable: false, enabled: false`)
    // - normal on-screen hittable node
    let runner_json = r#"{
        "appId": "dev.deja.checkout",
        "state": "running_foreground",
        "viewport": {"width": 390.0, "height": 844.0},
        "truncated": false,
        "hierarchyHash": "",
        "unchanged": false,
        "nodes": [
            {
                "index": 0,
                "depth": 1,
                "source": "app",
                "elementType": "Button",
                "identifier": "clipped_btn",
                "label": "Clipped Offscreen",
                "title": "",
                "enabled": true,
                "selected": false,
                "hasFocus": false,
                "hittable": false,
                "isOnScreen": false,
                "frame": {"x": 20.0, "y": 1400.0, "width": 200.0, "height": 44.0}
            },
            {
                "index": 1,
                "depth": 1,
                "source": "app",
                "elementType": "Button",
                "identifier": "legacy_clipped_btn",
                "label": "Legacy Offscreen",
                "title": "",
                "enabled": true,
                "selected": false,
                "hasFocus": false,
                "hittable": false,
                "frame": {"x": 20.0, "y": 1500.0, "width": 200.0, "height": 44.0}
            },
            {
                "index": 2,
                "depth": 1,
                "source": "app",
                "elementType": "Button",
                "identifier": "disabled_visible_btn",
                "label": "Disabled Visible",
                "title": "",
                "enabled": false,
                "selected": false,
                "hasFocus": false,
                "hittable": false,
                "isOnScreen": true,
                "frame": {"x": 20.0, "y": 240.0, "width": 200.0, "height": 44.0}
            },
            {
                "index": 3,
                "depth": 1,
                "source": "app",
                "elementType": "Button",
                "identifier": "normal_btn",
                "label": "Continue",
                "title": "",
                "enabled": true,
                "selected": false,
                "hasFocus": false,
                "hittable": true,
                "isOnScreen": true,
                "frame": {"x": 20.0, "y": 320.0, "width": 200.0, "height": 44.0}
            }
        ]
    }"#;

    let mut decoded: UiHierarchy =
        serde_json::from_str(runner_json).expect("decode runner-shaped UiHierarchy");
    decoded.hierarchy_hash = decoded.hierarchy_hash_hex();
    assert!(!decoded.nodes[0].is_on_screen);
    assert!(!decoded.nodes[1].is_on_screen);
    assert!(decoded.nodes[2].is_on_screen);
    assert!(decoded.nodes[3].is_on_screen);

    let driver = MockDriver::full();
    driver.set_screen(decoded);

    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(60);

    // Clipped off-screen node (`isOnScreen: false`) fails `assertVisible`.
    let vis_fail = FlowExecutor::new(cfg.clone())
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- assertVisible:\n    id: clipped_btn\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!vis_fail.passed);
    assert_eq!(
        vis_fail.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::ProductAssertion)
    );

    // Legacy omitted `isOnScreen` with `hittable: false` also fails `assertVisible`.
    let legacy_fail = FlowExecutor::new(cfg.clone())
        .execute_yaml(
            &driver,
            "appId: dev.deja.checkout\n---\n- assertVisible:\n    id: legacy_clipped_btn\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!legacy_fail.passed);

    // Both off-screen nodes pass `assertNotVisible`, while explicit `isOnScreen: true`
    // disabled node passes `assertVisible: { id: disabled_visible_btn, enabled: false }`.
    let pass_yaml = r#"
appId: dev.deja.checkout
---
- assertNotVisible:
    id: clipped_btn
- assertNotVisible:
    id: legacy_clipped_btn
- assertVisible:
    id: disabled_visible_btn
    enabled: false
- assertVisible:
    id: normal_btn
"#;
    let pass_out = FlowExecutor::new(cfg)
        .execute_yaml(&driver, pass_yaml, &CancellationToken::new())
        .await;
    assert!(pass_out.passed, "{:#?}", pass_out.failed_step);
}

#[tokio::test]
async fn round2_known_hash_bypassed_for_unhashed_checked_and_system_ui() {
    let ws = TempDir::new("round2-known-hash-bypass");
    let driver = MockDriver::full();
    driver.state.lock().unwrap().honour_known_hash = true;

    let unchecked_node = node(
        0,
        None,
        "agree_chk",
        "Accept terms",
        UiRect::new(20.0, 200.0, 240.0, 44.0),
    );
    let mut checked_node = unchecked_node.clone();
    checked_node.checked = true;

    let screen_unchecked = hierarchy_of(vec![unchecked_node]);
    let screen_checked = hierarchy_of(vec![checked_node]);
    assert_eq!(
        screen_unchecked.hierarchy_hash, screen_checked.hierarchy_hash,
        "checked-only flip intentionally produces identical FNV-1a hierarchyHash"
    );

    driver.set_screen(screen_unchecked);
    driver.push_tap_screen(screen_checked);

    let yaml = r#"
appId: dev.deja.checkout
---
- assertVisible:
    id: agree_chk
    checked: false
- tapOn:
    id: agree_chk
- assertVisible:
    id: agree_chk
    checked: true
"#;
    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(200);
    let out = FlowExecutor::new(cfg)
        .execute_yaml(&driver, yaml, &CancellationToken::new())
        .await;
    assert!(
        out.passed,
        "checked-only flip must bypass knownHash cache and pass: {:#?}",
        out.failed_step
    );

    let known_hashes: Vec<Option<String>> = driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::Hierarchy { known_hash } => Some(known_hash),
            _ => None,
        })
        .collect();
    assert!(
        known_hashes.len() >= 3,
        "expected hierarchy calls for both checked assertions + tap"
    );
    assert!(
        known_hashes.last().unwrap().is_none(),
        "selector with `checked: true` must send known_hash = None, got {known_hashes:?}"
    );

    // Nested relational selector with `checked` also flags `uses_unhashed_fields`.
    let nested_checked = Selector {
        text: Some("Label".to_string()),
        child_of: Some(Box::new(Selector {
            id: Some("row".to_string()),
            checked: Some(true),
            ..Selector::default()
        })),
        ..Selector::default()
    };
    assert!(nested_checked.uses_unhashed_fields());

    // `include_system_ui = true` also bypasses `known_hash` on plain selectors.
    let sys_driver = MockDriver::full();
    sys_driver.state.lock().unwrap().honour_known_hash = true;
    sys_driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "ready",
        "Ready",
        UiRect::new(20.0, 200.0, 120.0, 44.0),
    )]));
    let mut sys_cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    sys_cfg.include_system_ui = true;
    let sys_out = FlowExecutor::new(sys_cfg)
        .execute_yaml(
            &sys_driver,
            "appId: dev.deja.checkout\n---\n- assertVisible: \"Ready\"\n- assertVisible: \"Ready\"\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(sys_out.passed, "{:#?}", sys_out.failed_step);
    let sys_hashes: Vec<Option<String>> = sys_driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::Hierarchy { known_hash } => Some(known_hash),
            _ => None,
        })
        .collect();
    assert!(
        sys_hashes.iter().all(Option::is_none),
        "include_system_ui must always bypass known_hash: {sys_hashes:?}"
    );
}

#[tokio::test]
async fn round2_scroll_until_visible_enforces_deadline_strictly() {
    let ws = TempDir::new("round2-scroll-deadline");
    let driver = MockDriver::full();
    driver.state.lock().unwrap().hierarchy_delay = Some(Duration::from_millis(40));
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "terms_btn",
        "Terms",
        UiRect::new(20.0, 790.0, 350.0, 40.0),
    )]));

    let yaml = r#"
appId: dev.deja.checkout
---
- scrollUntilVisible:
    element:
      id: terms_btn
    direction: DOWN
    timeout: 1
    centerElement: true
"#;
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver, yaml, &CancellationToken::new())
        .await;
    assert!(
        !out.passed,
        "1ms timeout with 40ms hierarchy fetch must fail with timeout instead of accepting late match"
    );
    assert_eq!(
        out.failed_step.as_ref().unwrap().kind,
        StepFailureKind::Timeout
    );
    assert_eq!(
        out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Timeout)
    );
    let swipes = driver
        .events()
        .into_iter()
        .filter(|e| matches!(e, DriverEvent::Swipe { .. }))
        .count();
    assert_eq!(
        swipes, 0,
        "timed-out scrollUntilVisible must not issue any discovery or centering swipe"
    );
}

#[tokio::test]
async fn round2_wait_local_returns_transport_error_when_no_fresh_observation() {
    let ws = TempDir::new("round2-wait-transport");
    let screen = hierarchy_of(vec![node(
        0,
        None,
        "home",
        "Home",
        UiRect::new(20.0, 200.0, 200.0, 44.0),
    )]);

    // 1. Direct `wait_local`: seeded with prior-step `initial_hierarchy`, every fetch fails transiently.
    let direct_driver = MockDriver::full();
    direct_driver
        .state
        .lock()
        .unwrap()
        .fail_all_hierarchies_transient = true;
    let mut req = WaitSelectorRequest::visible(
        "dev.deja.checkout",
        Selector::by_id("missing_elem"),
        Duration::from_millis(60),
    );
    req.initial_hierarchy = Some(screen.clone());
    let err = wait_local(&direct_driver, &req, &CancellationToken::new())
        .await
        .expect_err("all-transient fetch failure must return DriverError::Transport, not miss");
    assert!(
        matches!(
            err,
            DriverError::Transport {
                transient: true,
                ..
            }
        ),
        "expected transient DriverError::Transport, got {err:?}"
    );

    // 2. End-to-end via `FlowExecutor`: step 1 populates `last_hierarchy`, step 2 hits
    // transient transport errors on every poll -> classified as Runner (Transport), NOT ProductAssertion.
    let exec_driver = MockDriver::full();
    exec_driver.push_hierarchy(screen);
    exec_driver
        .state
        .lock()
        .unwrap()
        .fail_all_hierarchies_transient = true;
    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(60);
    let out = FlowExecutor::new(cfg)
        .execute_yaml(
            &exec_driver,
            "appId: dev.deja.checkout\n---\n- assertVisible:\n    id: home\n- assertVisible:\n    id: missing_elem\n",
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.passed);
    assert_eq!(
        out.failed_step.as_ref().unwrap().kind,
        StepFailureKind::Transport
    );
    assert_eq!(
        out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::Runner)
    );
}

#[tokio::test]
async fn round2_child_subflow_crash_probed_for_child_app_id() {
    let ws = TempDir::new("round2-child-crash");
    fs::write(
        ws.path().join("child.yaml"),
        r#"
appId: dev.deja.child
---
- tapOn:
    id: missing_in_child
"#,
    )
    .unwrap();
    fs::write(
        ws.path().join("parent.yaml"),
        r#"
appId: dev.deja.parent
---
- runFlow: child.yaml
"#,
    )
    .unwrap();

    let driver = MockDriver::full();
    driver.set_screen(hierarchy_of(vec![node(
        0,
        None,
        "other",
        "Other",
        UiRect::new(20.0, 200.0, 200.0, 44.0),
    )]));
    driver.state.lock().unwrap().crash_signals_by_app.insert(
        "dev.deja.child".to_string(),
        DriverCrashSignal {
            source: CrashSignalSource::CrashReport,
            report_file: Some("ChildApp-2026-11-28.ips".to_string()),
            process: Some("ChildApp".to_string()),
            detail: Some("EXC_BAD_ACCESS".to_string()),
        },
    );

    let mut cfg = FlowExecutionConfig::new(ws.path().to_path_buf());
    cfg.default_step_timeout = Duration::from_millis(80);
    let out = FlowExecutor::new(cfg)
        .execute_file(&driver, Path::new("parent.yaml"), &CancellationToken::new())
        .await;
    assert!(!out.passed);

    let probed_apps: Vec<String> = driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::CheckAppCrash(app) => Some(app),
            _ => None,
        })
        .collect();
    assert_eq!(
        probed_apps,
        vec!["dev.deja.child"],
        "crash check must probe child subflow's app_id, not restored parent"
    );
    assert_eq!(
        out.failure.as_ref().map(|f| f.category),
        Some(FailureCategory::AppCrash)
    );
    assert_eq!(
        out.failed_step
            .as_ref()
            .and_then(|s| s.failed_app_id.as_deref()),
        Some("dev.deja.child")
    );
    assert_eq!(
        out.app_id.as_deref(),
        Some("dev.deja.parent"),
        "parent appId scope must still be restored after child subflow exits"
    );
}

#[tokio::test]
async fn round2_retry_tap_if_no_change_refreshes_stale_snapshot_before_point_tap() {
    let ws = TempDir::new("round2-retry-point-tap");
    // MockDriver::full() intentionally does NOT advertise CompoundAct, so tap 1 mutates the
    // screen without returning a fresh hierarchy (leaving `last_hierarchy` stale on screen A).
    let driver = MockDriver::full();
    let screen_a = hierarchy_of(vec![node(
        0,
        None,
        "to_b",
        "Go to B",
        UiRect::new(20.0, 200.0, 200.0, 44.0),
    )]);
    let screen_b = hierarchy_of(vec![node(
        0,
        None,
        "screen_b",
        "Screen B",
        UiRect::new(20.0, 200.0, 200.0, 44.0),
    )]);
    driver.set_screen(screen_a);
    // Tap 1 (id: to_b) transitions screen A -> screen B.
    // Tap 2 (point: 50%, 50%) leaves screen B unchanged, so retryTapIfNoChange MUST fire tap 3!
    driver.push_tap_screen(screen_b);

    let yaml = r#"
appId: dev.deja.checkout
---
- assertVisible:
    id: to_b
- tapOn:
    id: to_b
- tapOn:
    point: 50%, 50%
    retryTapIfNoChange: true
"#;
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver, yaml, &CancellationToken::new())
        .await;
    assert!(out.passed, "{:#?}", out.failed_step);

    let taps: Vec<(f64, f64)> = driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::Tap(x, y) => Some((x, y)),
            _ => None,
        })
        .collect();
    assert_eq!(
        taps.len(),
        3,
        "expected 1 tap for to_b + 1 initial point tap + 1 retry point tap on unchanged screen B, got {taps:?}"
    );
    assert_eq!(
        out.timing.steps[2].retries, 1,
        "point tap with retryTapIfNoChange must refresh stale pre-tap hierarchy and retry once"
    );
}

#[tokio::test]
async fn round2_open_link_forwards_browser_and_auto_verify_options() {
    let ws = TempDir::new("round2-open-link-opts");
    let driver = MockDriver::full();
    let yaml = r#"
appId: dev.deja.checkout
---
- openLink:
    link: https://example.com/offer
    browser: true
    autoVerify: true
- openLink: https://example.com/simple
"#;
    let out = FlowExecutor::new(FlowExecutionConfig::new(ws.path().to_path_buf()))
        .execute_yaml(&driver, yaml, &CancellationToken::new())
        .await;
    assert!(out.passed, "{:#?}", out.failed_step);

    let open_links: Vec<(String, Option<bool>, Option<bool>)> = driver
        .events()
        .into_iter()
        .filter_map(|e| match e {
            DriverEvent::OpenLink {
                url,
                auto_verify,
                browser,
            } => Some((url, auto_verify, browser)),
            _ => None,
        })
        .collect();
    assert_eq!(
        open_links,
        vec![
            (
                "https://example.com/offer".to_string(),
                Some(true),
                Some(true)
            ),
            ("https://example.com/simple".to_string(), None, None),
        ]
    );
}

#[test]
fn round2_junit_redacts_unicode_attribute_names() {
    let redactor = create_redactor("/tmp/ws-junit", None, &["fixture-secret".to_string()]);
    let provider = provider_key_redactor(std::iter::empty::<(&str, &str)>());

    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="suite" tests="1" failures="0" time="1.250">
    <testcase id="case1" 名time="fixture-secret" 名status="ERROR" time="1.250" status="SUCCESS"/>
  </testsuite>
</testsuites>"#;

    // Unicode attribute `名status="ERROR"` must not be misread as structural `status="ERROR"`.
    assert!(!reports_failure(xml));

    let redacted = redact_junit_with_redactors(xml, &redactor, &provider);
    assert!(
        !redacted.contains("fixture-secret"),
        "Unicode-named attribute value must be redacted: {redacted}"
    );
    assert!(
        redacted.contains("名time=\"&lt;redacted-fixture-value&gt;\""),
        "Unicode attribute name should be preserved with XML-escaped redacted value: {redacted}"
    );
    assert!(
        redacted.contains("time=\"1.250\"") && redacted.contains("status=\"SUCCESS\""),
        "exact structural attributes must remain untouched: {redacted}"
    );
}
