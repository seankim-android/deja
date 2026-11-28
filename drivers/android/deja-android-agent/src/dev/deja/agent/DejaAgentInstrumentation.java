package dev.deja.agent;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Activity;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.os.Build;
import android.os.Bundle;
import android.os.Process;
import android.view.accessibility.AccessibilityEvent;

import org.json.JSONObject;

/**
 * Self-instrumenting UiAutomator2 HTTP agent for Déjà.
 *
 * Launch via:
 *   adb -s <serial> shell am instrument -w -r \
 *     -e dejaToken <token> [-e dejaPort 7007] \
 *     dev.deja.agent/.DejaAgentInstrumentation
 */
public final class DejaAgentInstrumentation extends Instrumentation {
    public static final int DEFAULT_PORT = 7007;
    public static final String READY_PREFIX = "DEJA_ANDROID_AGENT_READY ";

    private String authToken;
    private int requestedPort = DEFAULT_PORT;
    private volatile HttpServer server;

    @Override
    public void onCreate(Bundle arguments) {
        super.onCreate(arguments);

        if (arguments == null) {
            failFast("missing instrumentation arguments: -e dejaToken <token> is required");
            return;
        }

        String token = arguments.getString("dejaToken");
        if (token == null || token.trim().isEmpty()) {
            failFast("missing required instrumentation argument: -e dejaToken <token>");
            return;
        }
        this.authToken = token.trim();

        String rawPort = arguments.getString("dejaPort");
        if (rawPort != null && !rawPort.trim().isEmpty()) {
            try {
                int parsed = Integer.parseInt(rawPort.trim());
                if (parsed < 1 || parsed > 65535) {
                    failFast("invalid dejaPort (must be 1..65535): " + rawPort);
                    return;
                }
                this.requestedPort = parsed;
            } catch (NumberFormatException e) {
                failFast("invalid dejaPort integer: " + rawPort);
                return;
            }
        }

        start();
    }

    @Override
    public void onStart() {
        super.onStart();
        HttpServer httpServer = null;
        try {
            UiAutomation uiAutomation = getUiAutomation(
                    UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES
            );
            if (uiAutomation == null) {
                failFast("getUiAutomation returned null");
                return;
            }

            AccessibilityServiceInfo info = uiAutomation.getServiceInfo();
            if (info == null) {
                info = new AccessibilityServiceInfo();
            }
            info.eventTypes = AccessibilityEvent.TYPES_ALL_MASK;
            info.feedbackType = AccessibilityServiceInfo.FEEDBACK_GENERIC;
            info.flags |= AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
                    | AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS
                    | AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS;
            uiAutomation.setServiceInfo(info);

            httpServer = new HttpServer(getContext(), uiAutomation, authToken, requestedPort);
            this.server = httpServer;
            int boundPort = httpServer.start();

            JSONObject ready = new JSONObject();
            ready.put("port", boundPort);
            ready.put("pid", Process.myPid());
            ready.put("sdkInt", Build.VERSION.SDK_INT);
            ready.put("tokenDigest", httpServer.getTokenDigest());

            Bundle status = new Bundle();
            status.putString("stream", READY_PREFIX + ready.toString() + "\n");
            status.putInt("port", boundPort);
            status.putInt("pid", Process.myPid());
            status.putString("tokenDigest", httpServer.getTokenDigest());
            sendStatus(Activity.RESULT_OK, status);

            httpServer.awaitShutdown();

            Bundle done = new Bundle();
            done.putString("stream", "deja-android-agent stopped cleanly\n");
            finish(Activity.RESULT_OK, done);
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
            Bundle done = new Bundle();
            done.putString("stream", "deja-android-agent interrupted\n");
            finish(Activity.RESULT_OK, done);
        } catch (Throwable t) {
            failFast("deja-android-agent fatal error: " + t.getClass().getSimpleName() + ": " + t.getMessage());
        } finally {
            if (httpServer != null) {
                httpServer.stop();
            }
            this.server = null;
        }
    }

    @Override
    public void onDestroy() {
        HttpServer current = this.server;
        if (current != null) {
            current.stop();
        }
        super.onDestroy();
    }

    private void failFast(String message) {
        Bundle err = new Bundle();
        err.putString("stream", "DEJA_ANDROID_AGENT_ERROR " + message + "\n");
        err.putString("error", message);
        sendStatus(Activity.RESULT_CANCELED, err);
        finish(Activity.RESULT_CANCELED, err);
    }
}
