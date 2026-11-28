package dev.deja.agent;

import android.app.ActivityManager;
import android.app.UiAutomation;
import android.content.Context;
import android.os.Build;
import android.os.Process;
import android.os.SystemClock;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.locks.ReentrantLock;
import java.util.regex.Pattern;
import java.util.regex.PatternSyntaxException;

/**
 * Loopback-only HTTP/1.1 server for the Déjà Android UiAutomator2 agent.
 *
 * Enforces:
 * - Strict {@code 127.0.0.1} bind and loopback peer check
 * - Bounded request headers ({@code <= 16 KiB}) and body ({@code <= 1 MiB})
 * - Constant-time {@code MessageDigest.isEqual} validation on {@code X-Deja-Token} for all
 *   routes except {@code GET /health}
 * - {@code ReentrantLock busyLock} returning {@code 409 Conflict} when another UI operation is active
 */
public final class HttpServer {
    public static final String AGENT_NAME = "deja-android-agent";
    public static final int PROTOCOL_VERSION = 1;
    public static final int MAX_HEADER_BYTES = 16 * 1024;
    public static final int MAX_BODY_BYTES = 1024 * 1024;
    private static final int SOCKET_TIMEOUT_MS = 15_000;
    private static final int[] WAIT_BACKOFF_MS = new int[]{15, 25, 40, 65, 100, 150};

    private final Context context;
    private final UiAutomation uiAutomation;
    private final UiActions actions;
    private final byte[] expectedTokenBytes;
    private final String tokenDigest;
    private final int requestedPort;

    private final ReentrantLock busyLock = new ReentrantLock();
    private final CountDownLatch shutdownLatch = new CountDownLatch(1);
    private final ExecutorService workers = Executors.newFixedThreadPool(4);

    private volatile boolean running = false;
    private volatile ServerSocket serverSocket;
    private Thread acceptThread;

    public HttpServer(Context context, UiAutomation uiAutomation, String authToken, int requestedPort) {
        this.context = context;
        this.uiAutomation = uiAutomation;
        this.actions = new UiActions(uiAutomation);
        this.expectedTokenBytes = authToken.getBytes(StandardCharsets.UTF_8);
        this.tokenDigest = sha256Prefix16(this.expectedTokenBytes);
        this.requestedPort = requestedPort;
    }

    public String getTokenDigest() {
        return tokenDigest;
    }

    public int start() throws IOException {
        ServerSocket ss = new ServerSocket();
        ss.setReuseAddress(true);
        ss.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), requestedPort), 16);
        this.serverSocket = ss;
        this.running = true;

        int boundPort = ss.getLocalPort();
        Thread t = new Thread(new Runnable() {
            @Override
            public void run() {
                acceptLoop(ss);
            }
        }, "deja-agent-accept");
        this.acceptThread = t;
        t.start();
        return boundPort;
    }

    public void awaitShutdown() throws InterruptedException {
        shutdownLatch.await();
    }

    public void stop() {
        running = false;
        ServerSocket ss = this.serverSocket;
        this.serverSocket = null;
        if (ss != null) {
            try {
                ss.close();
            } catch (IOException ignored) {
                // Best-effort socket close.
            }
        }
        workers.shutdownNow();
        shutdownLatch.countDown();
    }

    private void acceptLoop(ServerSocket ss) {
        try {
            while (running && !ss.isClosed()) {
                final Socket client;
                try {
                    client = ss.accept();
                } catch (IOException e) {
                    if (!running) {
                        break;
                    }
                    continue;
                }
                InetAddress peer = client.getInetAddress();
                if (peer == null || !peer.isLoopbackAddress()) {
                    safeClose(client);
                    continue;
                }
                try {
                    workers.execute(new Runnable() {
                        @Override
                        public void run() {
                            handleConnection(client);
                        }
                    });
                } catch (Throwable rejected) {
                    safeClose(client);
                }
            }
        } finally {
            shutdownLatch.countDown();
        }
    }

    private void handleConnection(Socket socket) {
        try {
            socket.setSoTimeout(SOCKET_TIMEOUT_MS);
            socket.setTcpNoDelay(true);
            BufferedInputStream in = new BufferedInputStream(socket.getInputStream(), 8192);
            OutputStream out = socket.getOutputStream();

            HttpRequest req = readRequest(in);
            if (req == null) {
                return;
            }
            if (req.parseErrorStatus > 0) {
                writeJsonError(out, req.parseErrorStatus, req.parseErrorMessage);
                return;
            }

            HttpResponse resp = dispatch(req);
            writeResponse(out, resp);
        } catch (IOException ignored) {
            // Connection dropped by peer or timed out.
        } catch (Throwable t) {
            try {
                writeJsonError(socket.getOutputStream(), 500, "internal server error: " + t.getMessage());
            } catch (IOException ignored) {
                // Socket already closed.
            }
        } finally {
            safeClose(socket);
        }
    }

    private HttpResponse dispatch(HttpRequest req) throws JSONException {
        String method = req.method;
        String path = req.path;

        if ("GET".equals(method) && "/health".equals(path)) {
            return handleHealth();
        }

        // All routes except GET /health require constant-time X-Deja-Token verification.
        String provided = req.headers.get("x-deja-token");
        byte[] providedBytes = provided != null
                ? provided.getBytes(StandardCharsets.UTF_8)
                : new byte[0];
        if (!MessageDigest.isEqual(providedBytes, expectedTokenBytes)) {
            return HttpResponse.error(401, "unauthorized");
        }

        if ("POST".equals(method) && "/shutdown".equals(path)) {
            JSONObject ok = new JSONObject();
            ok.put("ok", true);
            new Thread(new Runnable() {
                @Override
                public void run() {
                    SystemClock.sleep(25);
                    stop();
                }
            }, "deja-agent-shutdown").start();
            return HttpResponse.json(200, ok);
        }

        if (!"POST".equals(method)) {
            return HttpResponse.error(404, "unknown route " + method + " " + path);
        }

        if (!busyLock.tryLock()) {
            return HttpResponse.error(409, "agent busy with another in-flight operation");
        }
        try {
            switch (path) {
                case "/hierarchy":
                    return handleHierarchy(req.body);
                case "/act":
                    return handleAct(req.body);
                case "/waitForSelector":
                    return handleWaitForSelector(req.body);
                case "/appState":
                    return handleAppState(req.body);
                default:
                    return HttpResponse.error(404, "unknown route " + method + " " + path);
            }
        } finally {
            busyLock.unlock();
        }
    }

    private HttpResponse handleHealth() throws JSONException {
        HierarchySnapshot.DisplaySize display = HierarchySnapshot.getDisplaySize(context, uiAutomation);
        JSONObject displayObj = new JSONObject();
        displayObj.put("width", display.width);
        displayObj.put("height", display.height);

        JSONObject body = new JSONObject();
        body.put("status", "ok");
        body.put("agent", AGENT_NAME);
        body.put("protocolVersion", PROTOCOL_VERSION);
        body.put("pid", Process.myPid());
        body.put("sdkInt", Build.VERSION.SDK_INT);
        body.put("tokenDigest", tokenDigest);
        body.put("display", displayObj);
        return HttpResponse.json(200, body);
    }

    private HttpResponse handleHierarchy(byte[] rawBody) throws JSONException {
        JSONObject json = parseJsonBody(rawBody, true);
        if (json == null) {
            return HttpResponse.error(400, "invalid JSON body");
        }
        String appId = json.optString("appId", "");
        boolean includeSystemUi = json.optBoolean("includeSystemUi", false);
        int maxDepth = json.optInt("maxDepth", HierarchySnapshot.DEFAULT_MAX_DEPTH);

        String knownHash = null;
        if (json.has("knownHash") && !json.isNull("knownHash")) {
            knownHash = json.optString("knownHash", "");
            if (!isValidHex16(knownHash)) {
                return HttpResponse.error(400, "knownHash must be a 16-character hexadecimal string");
            }
            knownHash = knownHash.toLowerCase(Locale.US);
        }

        HierarchySnapshot.CaptureResult capture = HierarchySnapshot.capture(
                context,
                uiAutomation,
                appId,
                includeSystemUi,
                maxDepth
        );
        return HttpResponse.json(200, capture.toJson(knownHash));
    }

    private HttpResponse handleAct(byte[] rawBody) throws JSONException {
        JSONObject json = parseJsonBody(rawBody, false);
        if (json == null) {
            return HttpResponse.error(400, "invalid JSON body");
        }
        String action = json.optString("action", "").trim();
        if (action.isEmpty()) {
            return HttpResponse.error(400, "missing action");
        }

        String appId = json.optString("appId", "");
        int postActionDelayMs = Math.max(0, Math.min(5_000, json.optInt("postActionDelayMs", 0)));
        boolean returnHierarchy = json.optBoolean("returnHierarchy", false);
        boolean includeSystemUi = json.optBoolean("includeSystemUi", false);
        int maxDepth = json.optInt("maxDepth", HierarchySnapshot.DEFAULT_MAX_DEPTH);

        String knownHash = null;
        if (json.has("knownHash") && !json.isNull("knownHash")) {
            knownHash = json.optString("knownHash", "");
            if (!isValidHex16(knownHash)) {
                return HttpResponse.error(400, "knownHash must be a 16-character hexadecimal string");
            }
            knownHash = knownHash.toLowerCase(Locale.US);
        }

        long started = SystemClock.uptimeMillis();
        Boolean hadKeyboard = null;

        try {
            switch (action) {
                case "tap": {
                    double x = requireFiniteNonNegative(json, "x");
                    double y = requireFiniteNonNegative(json, "y");
                    int count = Math.max(1, Math.min(10, json.optInt("count", 1)));
                    int durationMs = json.optInt("durationMs", 0);
                    if (durationMs > 0) {
                        actions.longPress((float) x, (float) y, durationMs);
                    } else if (count == 2) {
                        actions.doubleTap((float) x, (float) y);
                    } else {
                        for (int i = 0; i < count; i++) {
                            actions.tap((float) x, (float) y);
                        }
                    }
                    break;
                }
                case "doubleTap": {
                    double x = requireFiniteNonNegative(json, "x");
                    double y = requireFiniteNonNegative(json, "y");
                    actions.doubleTap((float) x, (float) y);
                    break;
                }
                case "longPress": {
                    double x = requireFiniteNonNegative(json, "x");
                    double y = requireFiniteNonNegative(json, "y");
                    int durationMs = json.optInt("durationMs", 1_000);
                    actions.longPress((float) x, (float) y, durationMs);
                    break;
                }
                case "swipe": {
                    double startX = requireFiniteNonNegative(json, "startX");
                    double startY = requireFiniteNonNegative(json, "startY");
                    double endX = requireFiniteNonNegative(json, "endX");
                    double endY = requireFiniteNonNegative(json, "endY");
                    int durationMs = json.optInt("durationMs", 300);
                    int holdDurationMs = json.optInt("holdDurationMs", 0);
                    actions.swipe(
                            (float) startX,
                            (float) startY,
                            (float) endX,
                            (float) endY,
                            durationMs,
                            holdDurationMs
                    );
                    break;
                }
                case "type": {
                    if (!json.has("text") || json.isNull("text")) {
                        return HttpResponse.error(400, "missing text");
                    }
                    String text = json.getString("text");
                    if (text.getBytes(StandardCharsets.UTF_8).length > 16_384) {
                        return HttpResponse.error(400, "text exceeds maximum length");
                    }
                    actions.typeText(text);
                    break;
                }
                case "erase": {
                    int count = json.optInt("count", 1);
                    if (count < 0) {
                        return HttpResponse.error(400, "erase count must be non-negative");
                    }
                    actions.erase(count);
                    break;
                }
                case "key": {
                    String key = json.optString("key", "").trim();
                    if (key.isEmpty()) {
                        return HttpResponse.error(400, "missing key");
                    }
                    List<String> modifiers = new ArrayList<>();
                    JSONArray modsArr = json.optJSONArray("modifiers");
                    if (modsArr != null) {
                        for (int i = 0; i < modsArr.length(); i++) {
                            String m = modsArr.optString(i, "");
                            if (!m.isEmpty()) {
                                modifiers.add(m);
                            }
                        }
                    }
                    int count = json.optInt("count", 1);
                    actions.pressKey(key, modifiers, count);
                    break;
                }
                case "back": {
                    actions.back();
                    break;
                }
                case "hideKeyboard": {
                    hadKeyboard = actions.hideKeyboard();
                    break;
                }
                default:
                    return HttpResponse.error(400, "unsupported action: " + action);
            }
        } catch (IllegalArgumentException badArg) {
            return HttpResponse.error(400, badArg.getMessage());
        } catch (RuntimeException runtimeErr) {
            return HttpResponse.error(500, "action `" + action + "` failed: " + runtimeErr.getMessage());
        }

        if (postActionDelayMs > 0) {
            SystemClock.sleep(postActionDelayMs);
        }

        JSONObject resp = new JSONObject();
        resp.put("ok", true);
        if (hadKeyboard != null) {
            resp.put("hadKeyboard", hadKeyboard.booleanValue());
        }

        if (returnHierarchy) {
            try {
                HierarchySnapshot.CaptureResult capture = HierarchySnapshot.capture(
                        context,
                        uiAutomation,
                        appId,
                        includeSystemUi,
                        maxDepth
                );
                resp.put("hierarchy", capture.toJson(knownHash));
            } catch (Throwable snapErr) {
                // Action already succeeded; surface observation failure separately so callers never replay.
                resp.put("hierarchyError", snapErr.getMessage() != null ? snapErr.getMessage() : snapErr.toString());
            }
        }

        resp.put("elapsedMs", Math.max(0L, SystemClock.uptimeMillis() - started));
        return HttpResponse.json(200, resp);
    }

    private HttpResponse handleWaitForSelector(byte[] rawBody) throws JSONException {
        JSONObject json = parseJsonBody(rawBody, false);
        if (json == null) {
            return HttpResponse.error(400, "invalid JSON body");
        }
        String appId = json.optString("appId", "");
        String id = optNullableString(json, "id");
        if (id == null) {
            id = optNullableString(json, "identifier");
        }
        String text = optNullableString(json, "text");
        boolean useRegex = json.optBoolean("regex", false);
        String textMatch = json.optString("textMatch", useRegex ? "regex" : "exact").toLowerCase(Locale.US);
        if ("regex".equals(textMatch)) {
            useRegex = true;
        } else if (!"exact".equals(textMatch) && !"contains".equals(textMatch)) {
            return HttpResponse.error(400, "textMatch must be 'exact', 'contains', or 'regex'");
        }

        boolean notVisible = json.optBoolean("notVisible", false);
        boolean waitStable = json.optBoolean("waitStable", false);
        if (id == null && text == null && !(waitStable && !notVisible)) {
            return HttpResponse.error(
                    400,
                    "waitForSelector requires id or text (or waitStable=true without notVisible)"
            );
        }

        Pattern idPattern = null;
        Pattern textPattern = null;
        if (useRegex) {
            try {
                if (id != null) {
                    idPattern = Pattern.compile("^(?:" + id + ")$");
                }
                if (text != null) {
                    textPattern = Pattern.compile("^(?:" + text + ")$");
                }
            } catch (PatternSyntaxException pse) {
                return HttpResponse.error(400, "invalid selector regex: " + pse.getDescription());
            }
        }

        int stablePolls = Math.max(2, Math.min(10, json.optInt("stablePolls", 2)));
        int timeoutMs = Math.max(0, Math.min(60_000, json.optInt("timeoutMs", 5_000)));
        boolean includeSystemUi = json.optBoolean("includeSystemUi", false);
        boolean returnHierarchy = json.optBoolean("returnHierarchy", false);
        int maxDepth = json.optInt("maxDepth", HierarchySnapshot.DEFAULT_MAX_DEPTH);

        long started = SystemClock.uptimeMillis();
        long deadline = started + timeoutMs;
        int polls = 0;
        int backoffIdx = 0;
        String lastHash = "";
        int consecutiveSameHash = 0;
        HierarchySnapshot.CaptureResult lastCapture = null;

        while (true) {
            if (polls > 0 && SystemClock.uptimeMillis() >= deadline) {
                break;
            }
            polls++;
            try {
                HierarchySnapshot.CaptureResult capture = HierarchySnapshot.capture(
                        context,
                        uiAutomation,
                        appId,
                        includeSystemUi,
                        maxDepth
                );
                lastCapture = capture;

                if (capture.truncated) {
                    lastHash = "";
                    consecutiveSameHash = 0;
                } else if (!capture.hierarchyHash.isEmpty() && capture.hierarchyHash.equals(lastHash)) {
                    consecutiveSameHash++;
                } else {
                    lastHash = capture.hierarchyHash;
                    consecutiveSameHash = 1;
                }

                if (timeoutMs > 0 && SystemClock.uptimeMillis() > deadline) {
                    break;
                }

                HierarchySnapshot.CapturedNode matchedNode = findMatchingNode(
                        capture.nodes,
                        id,
                        idPattern,
                        text,
                        textPattern,
                        textMatch
                );
                boolean conditionMet;
                if (id == null && text == null) {
                    conditionMet = !notVisible;
                } else if (notVisible) {
                    conditionMet = (matchedNode == null);
                } else {
                    conditionMet = (matchedNode != null);
                }

                boolean stableMet = !waitStable || (!capture.truncated && consecutiveSameHash >= stablePolls);
                if (conditionMet && stableMet) {
                    JSONObject body = new JSONObject();
                    body.put("matched", true);
                    body.put("elapsedMs", Math.max(0L, SystemClock.uptimeMillis() - started));
                    body.put("polls", polls);
                    body.put("hierarchyHash", capture.hierarchyHash);
                    body.put("visibleSample", new JSONArray());
                    if (matchedNode != null) {
                        body.put("node", matchedNode.toJson());
                    }
                    if (returnHierarchy) {
                        body.put("hierarchy", capture.toJson(null));
                    }
                    return HttpResponse.json(200, body);
                }
            } catch (Throwable ignored) {
                lastHash = "";
                consecutiveSameHash = 0;
            }

            long remaining = deadline - SystemClock.uptimeMillis();
            if (remaining <= 0) {
                break;
            }
            int stepMs = WAIT_BACKOFF_MS[Math.min(backoffIdx, WAIT_BACKOFF_MS.length - 1)];
            backoffIdx = Math.min(backoffIdx + 1, WAIT_BACKOFF_MS.length - 1);
            SystemClock.sleep(Math.min(remaining, stepMs));
        }

        JSONObject timeoutBody = new JSONObject();
        timeoutBody.put("matched", false);
        timeoutBody.put("elapsedMs", Math.max(0L, SystemClock.uptimeMillis() - started));
        timeoutBody.put("polls", polls);
        timeoutBody.put("hierarchyHash", lastCapture != null ? lastCapture.hierarchyHash : "");
        timeoutBody.put(
                "visibleSample",
                lastCapture != null
                        ? HierarchySnapshot.extractRawVisibleSampleJson(lastCapture.nodes)
                        : new JSONArray()
        );
        if (returnHierarchy && lastCapture != null) {
            timeoutBody.put("hierarchy", lastCapture.toJson(null));
        }
        return HttpResponse.json(200, timeoutBody);
    }

    private HttpResponse handleAppState(byte[] rawBody) throws JSONException {
        JSONObject json = parseJsonBody(rawBody, false);
        if (json == null) {
            return HttpResponse.error(400, "invalid JSON body");
        }
        String appId = json.optString("appId", "").trim();
        if (appId.isEmpty()) {
            return HttpResponse.error(400, "missing appId");
        }

        String foregroundPkg = HierarchySnapshot.detectForegroundPackage(uiAutomation);
        boolean foreground = appId.equals(foregroundPkg);
        boolean runningApp = foreground;

        if (!runningApp && context != null) {
            ActivityManager am = (ActivityManager) context.getSystemService(Context.ACTIVITY_SERVICE);
            if (am != null) {
                List<ActivityManager.RunningAppProcessInfo> procs = am.getRunningAppProcesses();
                if (procs != null) {
                    for (ActivityManager.RunningAppProcessInfo p : procs) {
                        if (appId.equals(p.processName)) {
                            runningApp = true;
                            break;
                        }
                        if (p.pkgList != null) {
                            for (String pkg : p.pkgList) {
                                if (appId.equals(pkg)) {
                                    runningApp = true;
                                    break;
                                }
                            }
                        }
                        if (runningApp) {
                            break;
                        }
                    }
                }
            }
        }

        JSONObject body = new JSONObject();
        body.put("appId", appId);
        body.put("foreground", foreground);
        body.put("foregroundPackage", foregroundPkg != null ? foregroundPkg : "");
        body.put("running", runningApp);
        return HttpResponse.json(200, body);
    }

    private static HierarchySnapshot.CapturedNode findMatchingNode(
            List<HierarchySnapshot.CapturedNode> nodes,
            String id,
            Pattern idPattern,
            String text,
            Pattern textPattern,
            String textMatch
    ) {
        HierarchySnapshot.CapturedNode fallback = null;
        for (HierarchySnapshot.CapturedNode node : nodes) {
            if (!node.isOnScreen) {
                continue;
            }
            if (id != null && !matchesValue(node.identifier, id, idPattern, "exact")) {
                continue;
            }
            if (text != null) {
                boolean textHit = matchesValue(node.label, text, textPattern, textMatch)
                        || matchesValue(node.title, text, textPattern, textMatch)
                        || matchesValue(node.value, text, textPattern, textMatch)
                        || matchesValue(node.placeholderValue, text, textPattern, textMatch);
                if (!textHit) {
                    continue;
                }
            }
            if (node.hittable) {
                return node;
            }
            if (fallback == null) {
                fallback = node;
            }
        }
        return fallback;
    }

    private static boolean matchesValue(String candidate, String patternRaw, Pattern regex, String mode) {
        if (candidate == null || candidate.isEmpty()) {
            return false;
        }
        if (candidate.equals(patternRaw)) {
            return true;
        }
        if (regex != null) {
            return regex.matcher(candidate).matches();
        }
        if ("contains".equals(mode)) {
            return candidate.contains(patternRaw);
        }
        return false;
    }

    private static double requireFiniteNonNegative(JSONObject json, String key) {
        if (!json.has(key) || json.isNull(key)) {
            throw new IllegalArgumentException("missing coordinate `" + key + "`");
        }
        double val = json.optDouble(key, Double.NaN);
        if (!Double.isFinite(val) || val < 0.0) {
            throw new IllegalArgumentException("coordinate `" + key + "` must be a finite non-negative number");
        }
        return val;
    }

    private static String optNullableString(JSONObject json, String key) {
        if (!json.has(key) || json.isNull(key)) {
            return null;
        }
        String val = json.optString(key, "");
        return val.isEmpty() ? null : val;
    }

    private static JSONObject parseJsonBody(byte[] rawBody, boolean allowEmpty) {
        if (rawBody == null || rawBody.length == 0) {
            return allowEmpty ? new JSONObject() : null;
        }
        String text = new String(rawBody, StandardCharsets.UTF_8).trim();
        if (text.isEmpty()) {
            return allowEmpty ? new JSONObject() : null;
        }
        try {
            return new JSONObject(text);
        } catch (JSONException e) {
            return null;
        }
    }

    private static boolean isValidHex16(String value) {
        if (value == null || value.length() != 16) {
            return false;
        }
        for (int i = 0; i < 16; i++) {
            char c = value.charAt(i);
            boolean hex = (c >= '0' && c <= '9')
                    || (c >= 'a' && c <= 'f')
                    || (c >= 'A' && c <= 'F');
            if (!hex) {
                return false;
            }
        }
        return true;
    }

    private static String sha256Prefix16(byte[] input) {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            byte[] digest = md.digest(input);
            StringBuilder sb = new StringBuilder(16);
            for (int i = 0; i < 8; i++) {
                sb.append(String.format(Locale.US, "%02x", digest[i] & 0xFF));
            }
            return sb.toString();
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException("SHA-256 unavailable", e);
        }
    }

    private static final class HttpRequest {
        String method;
        String path;
        Map<String, String> headers = new HashMap<>();
        byte[] body = new byte[0];
        int parseErrorStatus = 0;
        String parseErrorMessage = null;

        static HttpRequest error(int status, String message) {
            HttpRequest r = new HttpRequest();
            r.parseErrorStatus = status;
            r.parseErrorMessage = message;
            return r;
        }
    }

    private static final class HttpResponse {
        final int status;
        final byte[] body;

        HttpResponse(int status, byte[] body) {
            this.status = status;
            this.body = body;
        }

        static HttpResponse json(int status, JSONObject obj) {
            return new HttpResponse(status, obj.toString().getBytes(StandardCharsets.UTF_8));
        }

        static HttpResponse error(int status, String message) {
            JSONObject obj = new JSONObject();
            try {
                obj.put("error", message != null ? message : "error");
            } catch (JSONException ignored) {
                // Unreachable for plain string key/value.
            }
            return json(status, obj);
        }
    }

    private static HttpRequest readRequest(InputStream in) throws IOException {
        ByteArrayOutputStream headerBuf = new ByteArrayOutputStream(512);
        int state = 0;
        while (true) {
            int b = in.read();
            if (b < 0) {
                return headerBuf.size() == 0 ? null : HttpRequest.error(400, "unexpected EOF in request headers");
            }
            headerBuf.write(b);
            if (headerBuf.size() > MAX_HEADER_BYTES) {
                return HttpRequest.error(431, "request headers exceed " + MAX_HEADER_BYTES + " bytes");
            }
            if (state == 0 && b == '\r') {
                state = 1;
            } else if (state == 1 && b == '\n') {
                state = 2;
            } else if (state == 2 && b == '\r') {
                state = 3;
            } else if (state == 3 && b == '\n') {
                break;
            } else {
                state = (b == '\r') ? 1 : 0;
            }
        }

        String headerText = new String(headerBuf.toByteArray(), StandardCharsets.UTF_8);
        String[] lines = headerText.split("\r\n");
        if (lines.length == 0 || lines[0].isEmpty()) {
            return HttpRequest.error(400, "missing HTTP request line");
        }
        String[] parts = lines[0].split(" ");
        if (parts.length != 3) {
            return HttpRequest.error(400, "malformed HTTP request line");
        }
        if (!"HTTP/1.1".equals(parts[2]) && !"HTTP/1.0".equals(parts[2])) {
            return HttpRequest.error(400, "unsupported HTTP version");
        }

        HttpRequest req = new HttpRequest();
        req.method = parts[0];
        String target = parts[1];
        int qIdx = target.indexOf('?');
        req.path = qIdx >= 0 ? target.substring(0, qIdx) : target;

        for (int i = 1; i < lines.length; i++) {
            String line = lines[i];
            if (line.isEmpty()) {
                continue;
            }
            int colon = line.indexOf(':');
            if (colon <= 0) {
                return HttpRequest.error(400, "malformed HTTP header");
            }
            String key = line.substring(0, colon).trim().toLowerCase(Locale.US);
            String val = line.substring(colon + 1).trim();
            req.headers.put(key, val);
        }

        if (req.headers.containsKey("transfer-encoding")) {
            return HttpRequest.error(501, "Transfer-Encoding is not supported");
        }

        int contentLength = 0;
        String rawLen = req.headers.get("content-length");
        if (rawLen != null) {
            try {
                contentLength = Integer.parseInt(rawLen);
            } catch (NumberFormatException e) {
                return HttpRequest.error(400, "invalid Content-Length");
            }
            if (contentLength < 0) {
                return HttpRequest.error(400, "negative Content-Length");
            }
            if (contentLength > MAX_BODY_BYTES) {
                return HttpRequest.error(413, "request body exceeds " + MAX_BODY_BYTES + " bytes");
            }
        }

        if (contentLength > 0) {
            byte[] body = new byte[contentLength];
            int offset = 0;
            while (offset < contentLength) {
                int n = in.read(body, offset, contentLength - offset);
                if (n < 0) {
                    return HttpRequest.error(400, "truncated request body");
                }
                offset += n;
            }
            req.body = body;
        }
        return req;
    }

    private static void writeJsonError(OutputStream out, int status, String message) throws IOException {
        writeResponse(out, HttpResponse.error(status, message));
    }

    private static void writeResponse(OutputStream out, HttpResponse resp) throws IOException {
        String header = "HTTP/1.1 " + resp.status + " " + reasonPhrase(resp.status) + "\r\n"
                + "Content-Type: application/json; charset=utf-8\r\n"
                + "Content-Length: " + resp.body.length + "\r\n"
                + "Connection: close\r\n"
                + "\r\n";
        out.write(header.getBytes(StandardCharsets.US_ASCII));
        out.write(resp.body);
        out.flush();
    }

    private static String reasonPhrase(int status) {
        switch (status) {
            case 200:
                return "OK";
            case 400:
                return "Bad Request";
            case 401:
                return "Unauthorized";
            case 404:
                return "Not Found";
            case 409:
                return "Conflict";
            case 413:
                return "Payload Too Large";
            case 431:
                return "Request Header Fields Too Large";
            case 500:
                return "Internal Server Error";
            case 501:
                return "Not Implemented";
            default:
                return "Error";
        }
    }

    private static void safeClose(Socket socket) {
        if (socket != null) {
            try {
                socket.close();
            } catch (IOException ignored) {
                // Best-effort close.
            }
        }
    }
}
