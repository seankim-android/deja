package dev.deja.agent;

import android.accessibilityservice.AccessibilityService;
import android.app.UiAutomation;
import android.graphics.Rect;
import android.os.Build;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.InputDevice;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import java.util.List;
import java.util.Locale;

/**
 * Low-level UI input injection for Déjà Android agent.
 *
 * Implements hold-before-lift zero-fling swipe (DEJA-85): after moving in 12 interpolated
 * steps from start to end, sleeps {@code holdDurationMs} and emits a final zero-delta MOVE
 * at (endX, endY) so Android's VelocityTracker decays to zero before ACTION_UP.
 */
public final class UiActions {
    private static final int SWIPE_STEPS = 12;
    private static final int MAX_KEY_COUNT = 100;
    private static final int MAX_ERASE_COUNT = 200;
    private static final int HIDE_KEYBOARD_SETTLE_MS = 500;

    private final UiAutomation uiAutomation;

    public UiActions(UiAutomation uiAutomation) {
        this.uiAutomation = uiAutomation;
    }

    public void tap(float x, float y) {
        long downTime = SystemClock.uptimeMillis();
        injectTouch(downTime, downTime, MotionEvent.ACTION_DOWN, x, y);
        SystemClock.sleep(20);
        injectTouch(downTime, SystemClock.uptimeMillis(), MotionEvent.ACTION_UP, x, y);
    }

    public void doubleTap(float x, float y) {
        tap(x, y);
        SystemClock.sleep(50);
        tap(x, y);
    }

    public void longPress(float x, float y, int durationMs) {
        int clamped = Math.max(1, Math.min(30_000, durationMs > 0 ? durationMs : 1_000));
        long downTime = SystemClock.uptimeMillis();
        injectTouch(downTime, downTime, MotionEvent.ACTION_DOWN, x, y);
        SystemClock.sleep(clamped);
        injectTouch(downTime, SystemClock.uptimeMillis(), MotionEvent.ACTION_UP, x, y);
    }

    public void swipe(
            float startX,
            float startY,
            float endX,
            float endY,
            int durationMs,
            int holdDurationMs
    ) {
        int clampedDuration = Math.max(50, Math.min(10_000, durationMs > 0 ? durationMs : 300));
        int clampedHold = Math.max(0, Math.min(5_000, holdDurationMs));

        long downTime = SystemClock.uptimeMillis();
        injectTouch(downTime, downTime, MotionEvent.ACTION_DOWN, startX, startY);

        for (int step = 1; step <= SWIPE_STEPS; step++) {
            long targetTime = downTime + ((long) clampedDuration * step) / SWIPE_STEPS;
            long now = SystemClock.uptimeMillis();
            if (targetTime > now) {
                SystemClock.sleep(targetTime - now);
            }
            float fraction = (float) step / (float) SWIPE_STEPS;
            float curX = startX + (endX - startX) * fraction;
            float curY = startY + (endY - startY) * fraction;
            injectTouch(downTime, SystemClock.uptimeMillis(), MotionEvent.ACTION_MOVE, curX, curY);
        }

        if (clampedHold > 0) {
            SystemClock.sleep(clampedHold);
            // Final zero-delta MOVE at (endX, endY) decays Android's VelocityTracker to 0
            // before ACTION_UP so scrolls land predictably without inertial fling overshoot.
            injectTouch(downTime, SystemClock.uptimeMillis(), MotionEvent.ACTION_MOVE, endX, endY);
        }

        injectTouch(downTime, SystemClock.uptimeMillis(), MotionEvent.ACTION_UP, endX, endY);
    }

    public void typeText(String text) {
        if (text == null || text.isEmpty()) {
            return;
        }
        if (isPureAsciiPrintableOrBasicWhitespace(text)) {
            KeyCharacterMap kcm = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD);
            KeyEvent[] events = kcm.getEvents(text.toCharArray());
            if (events != null && events.length > 0) {
                for (KeyEvent event : events) {
                    if (!uiAutomation.injectInputEvent(event, true)) {
                        throw new IllegalStateException(
                                "injectInputEvent rejected KeyEvent during typeText (keyCode="
                                        + event.getKeyCode() + ")"
                        );
                    }
                }
                return;
            }
        }

        // Non-ASCII / UTF-8 fallback via AccessibilityNodeInfo.ACTION_SET_TEXT (preserving append semantics).
        AccessibilityNodeInfo target = findEditableFocusTarget();
        if (target == null) {
            throw new IllegalStateException(
                    "cannot input non-ASCII text: no focused or editable input field found"
            );
        }
        try {
            String existing = "";
            boolean showingHint = Build.VERSION.SDK_INT >= 26 && target.isShowingHintText();
            if (!showingHint && target.getText() != null) {
                existing = target.getText().toString();
            }
            Bundle args = new Bundle();
            args.putCharSequence(
                    AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                    existing + text
            );
            boolean ok = target.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args);
            if (!ok) {
                throw new IllegalStateException("ACTION_SET_TEXT returned false for focused input");
            }
        } finally {
            safeRecycle(target);
        }
    }

    public void erase(int count) {
        int clamped = Math.max(0, Math.min(MAX_ERASE_COUNT, count));
        for (int i = 0; i < clamped; i++) {
            injectKeyCode(KeyEvent.KEYCODE_DEL, 0);
        }
    }

    public void pressKey(String keyName, List<String> modifiers, int count) {
        if (keyName == null || keyName.trim().isEmpty()) {
            throw new IllegalArgumentException("missing or empty key name");
        }
        int clampedCount = Math.max(1, Math.min(MAX_KEY_COUNT, count > 0 ? count : 1));
        int metaState = resolveMetaState(modifiers);
        String lower = keyName.trim().toLowerCase(Locale.US);

        if ("back".equals(lower) && metaState == 0) {
            for (int i = 0; i < clampedCount; i++) {
                back();
            }
            return;
        }
        if ("home".equals(lower) && metaState == 0) {
            for (int i = 0; i < clampedCount; i++) {
                if (!uiAutomation.performGlobalAction(AccessibilityService.GLOBAL_ACTION_HOME)) {
                    injectKeyCode(KeyEvent.KEYCODE_HOME, 0);
                }
            }
            return;
        }

        int keyCode = mapKeyCode(lower);
        if (keyCode == KeyEvent.KEYCODE_UNKNOWN) {
            throw new IllegalArgumentException("unsupported Android key: " + keyName);
        }
        for (int i = 0; i < clampedCount; i++) {
            injectKeyCode(keyCode, metaState);
        }
    }

    public void back() {
        if (!uiAutomation.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK)) {
            injectKeyCode(KeyEvent.KEYCODE_BACK, 0);
        }
    }

    /**
     * Dismisses the soft keyboard ONLY when an input-method window ({@code TYPE_INPUT_METHOD})
     * is actually visible, preventing {@code GLOBAL_ACTION_BACK} from navigating away from the
     * current screen when no keyboard is open.
     *
     * @return {@code true} if an IME window was present and dismissed, {@code false} if no IME was visible.
     */
    public boolean hideKeyboard() {
        if (!hasVisibleInputMethodWindow()) {
            return false;
        }
        uiAutomation.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK);
        long deadline = SystemClock.uptimeMillis() + HIDE_KEYBOARD_SETTLE_MS;
        while (SystemClock.uptimeMillis() < deadline) {
            SystemClock.sleep(50);
            if (!hasVisibleInputMethodWindow()) {
                return true;
            }
        }
        return true;
    }

    public boolean hasVisibleInputMethodWindow() {
        List<AccessibilityWindowInfo> windows = uiAutomation.getWindows();
        if (windows == null) {
            return false;
        }
        boolean found = false;
        for (AccessibilityWindowInfo win : windows) {
            if (win == null) {
                continue;
            }
            try {
                if (win.getType() == AccessibilityWindowInfo.TYPE_INPUT_METHOD) {
                    Rect bounds = new Rect();
                    win.getBoundsInScreen(bounds);
                    if (bounds.width() > 0 && bounds.height() > 0) {
                        found = true;
                    }
                }
            } finally {
                safeRecycle(win);
            }
        }
        return found;
    }

    private void injectTouch(long downTime, long eventTime, int action, float x, float y) {
        MotionEvent event = MotionEvent.obtain(downTime, eventTime, action, x, y, 0);
        try {
            event.setSource(InputDevice.SOURCE_TOUCHSCREEN);
            if (!uiAutomation.injectInputEvent(event, true)) {
                throw new IllegalStateException(
                        "injectInputEvent rejected MotionEvent action=" + action
                                + " at (" + x + "," + y + ")"
                );
            }
        } finally {
            event.recycle();
        }
    }

    private void injectKeyCode(int keyCode, int metaState) {
        long now = SystemClock.uptimeMillis();
        KeyEvent down = new KeyEvent(
                now,
                now,
                KeyEvent.ACTION_DOWN,
                keyCode,
                0,
                metaState,
                KeyCharacterMap.VIRTUAL_KEYBOARD,
                0,
                0,
                InputDevice.SOURCE_KEYBOARD
        );
        KeyEvent up = new KeyEvent(
                now,
                SystemClock.uptimeMillis(),
                KeyEvent.ACTION_UP,
                keyCode,
                0,
                metaState,
                KeyCharacterMap.VIRTUAL_KEYBOARD,
                0,
                0,
                InputDevice.SOURCE_KEYBOARD
        );
        if (!uiAutomation.injectInputEvent(down, true)) {
            throw new IllegalStateException("injectInputEvent rejected KeyEvent DOWN keyCode=" + keyCode);
        }
        if (!uiAutomation.injectInputEvent(up, true)) {
            throw new IllegalStateException("injectInputEvent rejected KeyEvent UP keyCode=" + keyCode);
        }
    }

    private AccessibilityNodeInfo findEditableFocusTarget() {
        AccessibilityNodeInfo focused = uiAutomation.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        if (focused != null) {
            if (focused.isEditable()) {
                return focused;
            }
            safeRecycle(focused);
        }
        AccessibilityNodeInfo root = uiAutomation.getRootInActiveWindow();
        if (root == null) {
            return null;
        }
        try {
            return findEditableRecursive(root, 0);
        } finally {
            safeRecycle(root);
        }
    }

    private AccessibilityNodeInfo findEditableRecursive(AccessibilityNodeInfo node, int depth) {
        if (node == null || depth > 48) {
            return null;
        }
        if (node.isEditable() && node.isFocused()) {
            return AccessibilityNodeInfo.obtain(node);
        }
        AccessibilityNodeInfo fallback = null;
        int childCount = node.getChildCount();
        for (int i = 0; i < childCount; i++) {
            AccessibilityNodeInfo child = node.getChild(i);
            if (child == null) {
                continue;
            }
            try {
                if (child.isEditable() && child.isFocused()) {
                    if (fallback != null) {
                        safeRecycle(fallback);
                    }
                    return AccessibilityNodeInfo.obtain(child);
                }
                AccessibilityNodeInfo nested = findEditableRecursive(child, depth + 1);
                if (nested != null) {
                    if (nested.isFocused()) {
                        if (fallback != null) {
                            safeRecycle(fallback);
                        }
                        return nested;
                    }
                    if (fallback == null) {
                        fallback = nested;
                    } else {
                        safeRecycle(nested);
                    }
                }
            } finally {
                safeRecycle(child);
            }
        }
        if (fallback == null && node.isEditable()) {
            return AccessibilityNodeInfo.obtain(node);
        }
        return fallback;
    }

    private static boolean isPureAsciiPrintableOrBasicWhitespace(String text) {
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            boolean ok = (c >= 0x20 && c <= 0x7E) || c == '\n' || c == '\t';
            if (!ok) {
                return false;
            }
        }
        return true;
    }

    private static int resolveMetaState(List<String> modifiers) {
        if (modifiers == null || modifiers.isEmpty()) {
            return 0;
        }
        int meta = 0;
        for (String raw : modifiers) {
            if (raw == null) {
                continue;
            }
            String m = raw.trim().toLowerCase(Locale.US);
            switch (m) {
                case "ctrl":
                case "control":
                    meta |= KeyEvent.META_CTRL_ON | KeyEvent.META_CTRL_LEFT_ON;
                    break;
                case "shift":
                    meta |= KeyEvent.META_SHIFT_ON | KeyEvent.META_SHIFT_LEFT_ON;
                    break;
                case "alt":
                case "option":
                    meta |= KeyEvent.META_ALT_ON | KeyEvent.META_ALT_LEFT_ON;
                    break;
                case "meta":
                case "cmd":
                case "command":
                case "super":
                    meta |= KeyEvent.META_META_ON | KeyEvent.META_META_LEFT_ON;
                    break;
                default:
                    throw new IllegalArgumentException("unsupported Android key modifier: " + raw);
            }
        }
        return meta;
    }

    private static int mapKeyCode(String lower) {
        switch (lower) {
            case "enter":
            case "return":
                return KeyEvent.KEYCODE_ENTER;
            case "delete":
            case "backspace":
                return KeyEvent.KEYCODE_DEL;
            case "forward_delete":
            case "forward delete":
                return KeyEvent.KEYCODE_FORWARD_DEL;
            case "tab":
                return KeyEvent.KEYCODE_TAB;
            case "escape":
            case "esc":
                return KeyEvent.KEYCODE_ESCAPE;
            case "space":
                return KeyEvent.KEYCODE_SPACE;
            case "back":
                return KeyEvent.KEYCODE_BACK;
            case "home":
                return KeyEvent.KEYCODE_HOME;
            case "lock":
            case "power":
                return KeyEvent.KEYCODE_POWER;
            case "volume up":
            case "volume_up":
                return KeyEvent.KEYCODE_VOLUME_UP;
            case "volume down":
            case "volume_down":
                return KeyEvent.KEYCODE_VOLUME_DOWN;
            case "up":
            case "uparrow":
            case "up arrow":
                return KeyEvent.KEYCODE_DPAD_UP;
            case "down":
            case "downarrow":
            case "down arrow":
                return KeyEvent.KEYCODE_DPAD_DOWN;
            case "left":
            case "leftarrow":
            case "left arrow":
                return KeyEvent.KEYCODE_DPAD_LEFT;
            case "right":
            case "rightarrow":
            case "right arrow":
                return KeyEvent.KEYCODE_DPAD_RIGHT;
            case "page_up":
            case "page up":
                return KeyEvent.KEYCODE_PAGE_UP;
            case "page_down":
            case "page down":
                return KeyEvent.KEYCODE_PAGE_DOWN;
            case "menu":
                return KeyEvent.KEYCODE_MENU;
            case "search":
                return KeyEvent.KEYCODE_SEARCH;
            case "media play pause":
            case "play_pause":
                return KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE;
            case "media next":
                return KeyEvent.KEYCODE_MEDIA_NEXT;
            case "media previous":
                return KeyEvent.KEYCODE_MEDIA_PREVIOUS;
            default:
                if (lower.length() == 1) {
                    char ch = lower.charAt(0);
                    if (ch >= 'a' && ch <= 'z') {
                        return KeyEvent.KEYCODE_A + (ch - 'a');
                    }
                    if (ch >= '0' && ch <= '9') {
                        return KeyEvent.KEYCODE_0 + (ch - '0');
                    }
                }
                return KeyEvent.KEYCODE_UNKNOWN;
        }
    }

    @SuppressWarnings("deprecation")
    private static void safeRecycle(AccessibilityNodeInfo node) {
        if (node != null && Build.VERSION.SDK_INT < 33) {
            try {
                node.recycle();
            } catch (Throwable ignored) {
                // Best-effort cleanup on older API levels.
            }
        }
    }

    @SuppressWarnings("deprecation")
    private static void safeRecycle(AccessibilityWindowInfo win) {
        if (win != null && Build.VERSION.SDK_INT < 33) {
            try {
                win.recycle();
            } catch (Throwable ignored) {
                // Best-effort cleanup on older API levels.
            }
        }
    }
}
