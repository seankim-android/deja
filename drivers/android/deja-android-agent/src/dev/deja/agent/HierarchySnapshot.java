package dev.deja.agent;

import android.app.UiAutomation;
import android.content.Context;
import android.graphics.Rect;
import android.os.Build;
import android.util.DisplayMetrics;
import android.view.WindowManager;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;

/**
 * Accessibility hierarchy traversal and bit-for-bit Rust-compatible FNV-1a 64-bit hashing.
 *
 * Hashing matches {@code deja_flow::backoff::hash_hierarchy_nodes} byte-for-byte:
 * volatile SystemUI chrome nodes and all of their descendants are pruned before hashing,
 * retained nodes receive sequential ordinals so parent indices remain invariant across
 * unrelated SystemUI changes, and fields are fed in identical little-endian / 0x1f / 0x1e order.
 */
public final class HierarchySnapshot {
    public static final int DEFAULT_MAX_DEPTH = 64;
    public static final int MAX_NODES = 2048;
    public static final int MAX_VISIBLE_SAMPLE_ITEMS = 30;

    private static final long FNV_OFFSET_BASIS = 0xcbf29ce484222325L;
    private static final long FNV_PRIME = 0x0100000001b3L;

    public static final class DisplaySize {
        public final int width;
        public final int height;

        public DisplaySize(int width, int height) {
            this.width = width;
            this.height = height;
        }
    }

    public static final class CapturedNode {
        public final int index;
        public final Integer parentIndex;
        public final int depth;
        public final String source;
        public final String elementType;
        public final String identifier;
        public final String label;
        public final String title;
        public final String value;
        public final String placeholderValue;
        public final boolean enabled;
        public final boolean checked;
        public final boolean selected;
        public final boolean hasFocus;
        public final boolean hittable;
        public final boolean isOnScreen;
        public final int x;
        public final int y;
        public final int width;
        public final int height;
        public final Rect visibleFrame;

        public CapturedNode(
                int index,
                Integer parentIndex,
                int depth,
                String source,
                String elementType,
                String identifier,
                String label,
                String title,
                String value,
                String placeholderValue,
                boolean enabled,
                boolean checked,
                boolean selected,
                boolean hasFocus,
                boolean hittable,
                boolean isOnScreen,
                int x,
                int y,
                int width,
                int height,
                Rect visibleFrame
        ) {
            this.index = index;
            this.parentIndex = parentIndex;
            this.depth = depth;
            this.source = source;
            this.elementType = elementType;
            this.identifier = identifier;
            this.label = label;
            this.title = title;
            this.value = value;
            this.placeholderValue = placeholderValue;
            this.enabled = enabled;
            this.checked = checked;
            this.selected = selected;
            this.hasFocus = hasFocus;
            this.hittable = hittable;
            this.isOnScreen = isOnScreen;
            this.x = x;
            this.y = y;
            this.width = width;
            this.height = height;
            this.visibleFrame = visibleFrame;
        }

        public JSONObject toJson() throws JSONException {
            JSONObject obj = new JSONObject();
            obj.put("index", index);
            if (parentIndex != null) {
                obj.put("parentIndex", parentIndex.intValue());
            }
            obj.put("depth", depth);
            obj.put("source", source);
            obj.put("elementType", elementType);
            obj.put("identifier", identifier);
            obj.put("label", label);
            obj.put("title", title);
            if (value != null) {
                obj.put("value", value);
            }
            if (placeholderValue != null) {
                obj.put("placeholderValue", placeholderValue);
            }
            obj.put("enabled", enabled);
            obj.put("checked", checked);
            obj.put("selected", selected);
            obj.put("hasFocus", hasFocus);
            obj.put("hittable", hittable);
            obj.put("isOnScreen", isOnScreen);

            JSONObject frameObj = new JSONObject();
            frameObj.put("x", x);
            frameObj.put("y", y);
            frameObj.put("width", width);
            frameObj.put("height", height);
            obj.put("frame", frameObj);

            if (visibleFrame != null) {
                JSONObject vf = new JSONObject();
                vf.put("x", visibleFrame.left);
                vf.put("y", visibleFrame.top);
                vf.put("width", Math.max(0, visibleFrame.width()));
                vf.put("height", Math.max(0, visibleFrame.height()));
                obj.put("visibleFrame", vf);
            }
            return obj;
        }
    }

    public static final class CaptureResult {
        public final String appId;
        public final String state;
        public final String foregroundPackage;
        public final DisplaySize viewport;
        public final boolean truncated;
        public final boolean rootCaptured;
        public final String hierarchyHash;
        public final List<CapturedNode> nodes;

        public CaptureResult(
                String appId,
                String state,
                String foregroundPackage,
                DisplaySize viewport,
                boolean truncated,
                boolean rootCaptured,
                String hierarchyHash,
                List<CapturedNode> nodes
        ) {
            this.appId = appId;
            this.state = state;
            this.foregroundPackage = foregroundPackage;
            this.viewport = viewport;
            this.truncated = truncated;
            this.rootCaptured = rootCaptured;
            this.hierarchyHash = hierarchyHash;
            this.nodes = nodes;
        }

        public JSONObject toJson(String knownHash) throws JSONException {
            JSONObject obj = new JSONObject();
            obj.put("appId", appId != null ? appId : "");
            obj.put("state", state);
            obj.put("foregroundPackage", foregroundPackage != null ? foregroundPackage : "");
            if (viewport != null && viewport.width > 0 && viewport.height > 0) {
                JSONObject vp = new JSONObject();
                vp.put("width", viewport.width);
                vp.put("height", viewport.height);
                obj.put("viewport", vp);
            }
            obj.put("truncated", truncated);
            obj.put("hierarchyHash", hierarchyHash);

            if (knownHash != null && knownHash.equalsIgnoreCase(hierarchyHash)) {
                obj.put("unchanged", true);
                obj.put("nodeCount", 0);
                obj.put("nodes", new JSONArray());
                obj.put("visibleSample", new JSONArray());
                return obj;
            }

            obj.put("unchanged", false);
            obj.put("nodeCount", nodes.size());
            JSONArray arr = new JSONArray();
            for (CapturedNode node : nodes) {
                arr.put(node.toJson());
            }
            obj.put("nodes", arr);
            obj.put("visibleSample", extractRawVisibleSampleJson(nodes));
            return obj;
        }
    }

    private HierarchySnapshot() {
    }

    public static DisplaySize getDisplaySize(Context context, UiAutomation uiAutomation) {
        if (context != null) {
            WindowManager wm = (WindowManager) context.getSystemService(Context.WINDOW_SERVICE);
            if (wm != null && Build.VERSION.SDK_INT >= 30) {
                Rect bounds = wm.getCurrentWindowMetrics().getBounds();
                if (bounds != null && bounds.width() > 0 && bounds.height() > 0) {
                    return new DisplaySize(bounds.width(), bounds.height());
                }
            }
            if (context.getResources() != null) {
                DisplayMetrics dm = context.getResources().getDisplayMetrics();
                if (dm != null && dm.widthPixels > 0 && dm.heightPixels > 0) {
                    return new DisplaySize(dm.widthPixels, dm.heightPixels);
                }
            }
        }
        if (uiAutomation != null) {
            AccessibilityNodeInfo root = uiAutomation.getRootInActiveWindow();
            if (root != null) {
                try {
                    Rect r = new Rect();
                    root.getBoundsInScreen(r);
                    if (r.width() > 0 && r.height() > 0) {
                        return new DisplaySize(r.width(), r.height());
                    }
                } finally {
                    safeRecycle(root);
                }
            }
        }
        return new DisplaySize(0, 0);
    }

    public static String detectForegroundPackage(UiAutomation uiAutomation) {
        if (uiAutomation == null) {
            return "";
        }
        List<AccessibilityWindowInfo> windows = uiAutomation.getWindows();
        if (windows != null && !windows.isEmpty()) {
            String fallbackPkg = "";
            int bestLayer = Integer.MIN_VALUE;
            String bestAppPkg = "";
            for (AccessibilityWindowInfo win : windows) {
                if (win == null) {
                    continue;
                }
                try {
                    if (win.getType() != AccessibilityWindowInfo.TYPE_APPLICATION) {
                        continue;
                    }
                    AccessibilityNodeInfo root = win.getRoot();
                    if (root == null) {
                        continue;
                    }
                    try {
                        CharSequence pkgCs = root.getPackageName();
                        String pkg = pkgCs != null ? pkgCs.toString() : "";
                        if (!pkg.isEmpty()) {
                            if (win.isActive() || win.isFocused()) {
                                return pkg;
                            }
                            int layer = win.getLayer();
                            if (layer >= bestLayer) {
                                bestLayer = layer;
                                bestAppPkg = pkg;
                            }
                            if (fallbackPkg.isEmpty()) {
                                fallbackPkg = pkg;
                            }
                        }
                    } finally {
                        safeRecycle(root);
                    }
                } finally {
                    safeRecycle(win);
                }
            }
            if (!bestAppPkg.isEmpty()) {
                return bestAppPkg;
            }
            if (!fallbackPkg.isEmpty()) {
                return fallbackPkg;
            }
        }

        AccessibilityNodeInfo activeRoot = uiAutomation.getRootInActiveWindow();
        if (activeRoot != null) {
            try {
                CharSequence pkgCs = activeRoot.getPackageName();
                return pkgCs != null ? pkgCs.toString() : "";
            } finally {
                safeRecycle(activeRoot);
            }
        }
        return "";
    }

    public static CaptureResult capture(
            Context context,
            UiAutomation uiAutomation,
            String targetAppId,
            boolean includeSystemUi,
            int maxDepth
    ) {
        int effectiveMaxDepth = maxDepth > 0 ? Math.min(maxDepth, 128) : DEFAULT_MAX_DEPTH;
        DisplaySize display = getDisplaySize(context, uiAutomation);
        Rect displayRect = (display.width > 0 && display.height > 0)
                ? new Rect(0, 0, display.width, display.height)
                : null;

        List<CapturedNode> nodes = new ArrayList<>();
        boolean[] truncated = new boolean[]{false};
        boolean rootCaptured = false;
        String foregroundPkg = "";
        int bestAppLayer = Integer.MIN_VALUE;

        List<AccessibilityWindowInfo> rawWindows = uiAutomation.getWindows();
        if (rawWindows != null && !rawWindows.isEmpty()) {
            List<AccessibilityWindowInfo> ordered = new ArrayList<>(rawWindows);
            Collections.sort(ordered, new Comparator<AccessibilityWindowInfo>() {
                @Override
                public int compare(AccessibilityWindowInfo a, AccessibilityWindowInfo b) {
                    int typeRankA = windowTypeRank(a.getType());
                    int typeRankB = windowTypeRank(b.getType());
                    if (typeRankA != typeRankB) {
                        return Integer.compare(typeRankA, typeRankB);
                    }
                    // Higher layer first within same window type so active dialogs appear ahead.
                    return Integer.compare(b.getLayer(), a.getLayer());
                }
            });

            for (AccessibilityWindowInfo win : ordered) {
                if (win == null) {
                    continue;
                }
                try {
                    int type = win.getType();
                    if (type == AccessibilityWindowInfo.TYPE_ACCESSIBILITY_OVERLAY
                            || type == AccessibilityWindowInfo.TYPE_SPLIT_SCREEN_DIVIDER) {
                        continue;
                    }
                    AccessibilityNodeInfo root = win.getRoot();
                    if (root == null) {
                        continue;
                    }
                    try {
                        String winPkg = charSeqToString(root.getPackageName());
                        if (type == AccessibilityWindowInfo.TYPE_APPLICATION && !winPkg.isEmpty()) {
                            if (win.isActive() || win.isFocused()) {
                                foregroundPkg = winPkg;
                            } else if (foregroundPkg.isEmpty() || win.getLayer() > bestAppLayer) {
                                bestAppLayer = win.getLayer();
                                foregroundPkg = winPkg;
                            }
                        }

                        boolean isSystemUiWin = "com.android.systemui".equalsIgnoreCase(winPkg);
                        boolean isImeWin = (type == AccessibilityWindowInfo.TYPE_INPUT_METHOD);
                        if (!includeSystemUi && (isSystemUiWin || isImeWin)) {
                            continue;
                        }

                        rootCaptured = true;
                        Rect winBounds = new Rect();
                        win.getBoundsInScreen(winBounds);
                        Rect clipBounds = computeClipRect(winBounds, displayRect);

                        String windowSource = resolveWindowSource(winPkg, targetAppId, type);
                        traverseNode(
                                root,
                                null,
                                0,
                                effectiveMaxDepth,
                                windowSource,
                                targetAppId,
                                clipBounds,
                                nodes,
                                truncated
                        );
                    } finally {
                        safeRecycle(root);
                    }
                } finally {
                    safeRecycle(win);
                }
            }
        }

        if (nodes.isEmpty()) {
            AccessibilityNodeInfo activeRoot = uiAutomation.getRootInActiveWindow();
            if (activeRoot != null) {
                try {
                    rootCaptured = true;
                    String pkg = charSeqToString(activeRoot.getPackageName());
                    if (foregroundPkg.isEmpty()) {
                        foregroundPkg = pkg;
                    }
                    String source = resolveWindowSource(pkg, targetAppId, AccessibilityWindowInfo.TYPE_APPLICATION);
                    traverseNode(
                            activeRoot,
                            null,
                            0,
                            effectiveMaxDepth,
                            source,
                            targetAppId,
                            displayRect,
                            nodes,
                            truncated
                    );
                } finally {
                    safeRecycle(activeRoot);
                }
            }
        }

        if (display.width <= 0 || display.height <= 0) {
            for (CapturedNode n : nodes) {
                if (n.depth == 0 && n.width > 0 && n.height > 0) {
                    display = new DisplaySize(n.width, n.height);
                    break;
                }
            }
        }

        String state;
        if (targetAppId != null && !targetAppId.isEmpty()) {
            if (targetAppId.equals(foregroundPkg)) {
                state = "running_foreground";
            } else if (!foregroundPkg.isEmpty()) {
                state = "running_background";
            } else {
                state = "unknown";
            }
        } else {
            state = nodes.isEmpty() ? "unknown" : "running_foreground";
        }

        String hash = rootCaptured && !nodes.isEmpty() ? computeHierarchyHashHex(nodes) : "";
        return new CaptureResult(
                targetAppId != null ? targetAppId : "",
                state,
                foregroundPkg,
                display,
                truncated[0],
                rootCaptured && !nodes.isEmpty(),
                hash,
                nodes
        );
    }

    private static int windowTypeRank(int type) {
        switch (type) {
            case AccessibilityWindowInfo.TYPE_APPLICATION:
                return 0;
            case AccessibilityWindowInfo.TYPE_SYSTEM:
                return 1;
            case AccessibilityWindowInfo.TYPE_INPUT_METHOD:
                return 2;
            default:
                return 3;
        }
    }

    private static String resolveWindowSource(String pkg, String targetAppId, int windowType) {
        if ("com.android.systemui".equalsIgnoreCase(pkg)) {
            return "com.android.systemui";
        }
        if (windowType == AccessibilityWindowInfo.TYPE_INPUT_METHOD) {
            return pkg.isEmpty() ? "ime" : pkg;
        }
        if (targetAppId != null && !targetAppId.isEmpty() && targetAppId.equals(pkg)) {
            return "app";
        }
        if (windowType == AccessibilityWindowInfo.TYPE_APPLICATION && (targetAppId == null || targetAppId.isEmpty())) {
            return "app";
        }
        return pkg.isEmpty() ? "system" : pkg;
    }

    private static Rect computeClipRect(Rect winBounds, Rect displayRect) {
        boolean winValid = winBounds != null && winBounds.width() > 0 && winBounds.height() > 0;
        if (winValid && displayRect != null) {
            Rect inter = new Rect(winBounds);
            if (inter.intersect(displayRect)) {
                return inter;
            }
            return new Rect(0, 0, 0, 0);
        }
        if (winValid) {
            return new Rect(winBounds);
        }
        return displayRect != null ? new Rect(displayRect) : null;
    }

    private static void traverseNode(
            AccessibilityNodeInfo node,
            Integer parentIndex,
            int depth,
            int maxDepth,
            String windowSource,
            String targetAppId,
            Rect clipBounds,
            List<CapturedNode> out,
            boolean[] truncated
    ) {
        if (node == null) {
            return;
        }
        if (out.size() >= MAX_NODES) {
            truncated[0] = true;
            return;
        }

        String nodePkg = charSeqToString(node.getPackageName());
        String source = windowSource;
        if ("com.android.systemui".equalsIgnoreCase(nodePkg)) {
            source = "com.android.systemui";
        } else if (targetAppId != null && !targetAppId.isEmpty() && targetAppId.equals(nodePkg)) {
            source = "app";
        }

        String elementType = simplifyClassName(charSeqToString(node.getClassName()));
        String identifier = charSeqToString(node.getViewIdResourceName());
        String text = charSeqToString(node.getText());
        String contentDesc = charSeqToString(node.getContentDescription());

        String label;
        String title;
        if (!text.isEmpty()) {
            label = text;
            title = (!contentDesc.isEmpty() && !contentDesc.equals(text)) ? contentDesc : "";
        } else if (!contentDesc.isEmpty()) {
            label = contentDesc;
            title = "";
        } else {
            label = "";
            title = "";
        }

        String value = null;
        if (Build.VERSION.SDK_INT >= 30) {
            CharSequence stateDesc = node.getStateDescription();
            if (stateDesc != null && stateDesc.length() > 0) {
                value = stateDesc.toString();
            }
        }

        String placeholderValue = null;
        if (Build.VERSION.SDK_INT >= 26) {
            CharSequence hint = node.getHintText();
            if (hint != null && hint.length() > 0) {
                placeholderValue = hint.toString();
            }
            if (node.isShowingHintText() && placeholderValue == null && !text.isEmpty()) {
                placeholderValue = text;
            }
        }

        Rect bounds = new Rect();
        node.getBoundsInScreen(bounds);
        int x = bounds.left;
        int y = bounds.top;
        int w = Math.max(0, bounds.width());
        int h = Math.max(0, bounds.height());

        Rect visibleFrame = null;
        boolean hasPositiveClippedArea = (w > 0 && h > 0);
        if (w > 0 && h > 0 && clipBounds != null) {
            Rect clipped = new Rect(bounds);
            if (clipped.intersect(clipBounds) && clipped.width() > 0 && clipped.height() > 0) {
                if (!clipped.equals(bounds)) {
                    visibleFrame = clipped;
                }
            } else {
                hasPositiveClippedArea = false;
                visibleFrame = new Rect(0, 0, 0, 0);
            }
        }

        boolean visibleToUser = node.isVisibleToUser();
        boolean isOnScreen = visibleToUser && hasPositiveClippedArea;
        boolean enabled = node.isEnabled();
        boolean checked = node.isChecked();
        boolean selected = node.isSelected();
        boolean hasFocus = node.isFocused();
        boolean hittable = isOnScreen && (node.isClickable() || node.isFocusable() || visibleToUser);

        int myIndex = out.size();
        out.add(new CapturedNode(
                myIndex,
                parentIndex,
                depth,
                source,
                elementType,
                identifier,
                label,
                title,
                value,
                placeholderValue,
                enabled,
                checked,
                selected,
                hasFocus,
                hittable,
                isOnScreen,
                x,
                y,
                w,
                h,
                visibleFrame
        ));

        int childCount = node.getChildCount();
        if (childCount <= 0) {
            return;
        }
        if (depth >= maxDepth) {
            truncated[0] = true;
            return;
        }

        for (int i = 0; i < childCount; i++) {
            if (out.size() >= MAX_NODES) {
                truncated[0] = true;
                return;
            }
            AccessibilityNodeInfo child = node.getChild(i);
            if (child == null) {
                continue;
            }
            try {
                traverseNode(
                        child,
                        myIndex,
                        depth + 1,
                        maxDepth,
                        windowSource,
                        targetAppId,
                        clipBounds,
                        out,
                        truncated
                );
            } finally {
                safeRecycle(child);
            }
        }
    }

    public static boolean isVolatileSystemChrome(String source, String elementType, String identifier) {
        if ("StatusBar".equalsIgnoreCase(elementType)) {
            return true;
        }
        if ("systemui".equalsIgnoreCase(source) || "com.android.systemui".equalsIgnoreCase(source)) {
            return true;
        }
        if (identifier != null) {
            if (identifier.startsWith("com.android.systemui:id/")
                    || "statusBarBackground".equalsIgnoreCase(identifier)
                    || "navigationBarBackground".equalsIgnoreCase(identifier)) {
                return true;
            }
        }
        return false;
    }

    /**
     * Computes the exact 64-bit FNV-1a hierarchy hash matching
     * {@code deja_flow::backoff::hash_hierarchy_nodes} bit-for-bit.
     */
    public static long computeHierarchyHash(List<CapturedNode> nodes) {
        Map<Integer, Integer> parentByIndex = new HashMap<>(nodes.size() * 2 + 1);
        Map<Integer, Boolean> retainedByIndex = new HashMap<>(nodes.size() * 2 + 1);

        for (CapturedNode node : nodes) {
            parentByIndex.put(node.index, node.parentIndex);
            if (isVolatileSystemChrome(node.source, node.elementType, node.identifier)) {
                retainedByIndex.put(node.index, Boolean.FALSE);
            }
        }

        List<Integer> chain = new ArrayList<>();
        Set<Integer> visiting = new HashSet<>();
        for (CapturedNode node : nodes) {
            if (retainedByIndex.containsKey(node.index)) {
                continue;
            }
            chain.clear();
            visiting.clear();
            Integer cursor = node.index;
            boolean resolved = true;
            while (cursor != null) {
                Boolean known = retainedByIndex.get(cursor);
                if (known != null) {
                    resolved = known.booleanValue();
                    break;
                }
                if (!visiting.add(cursor)) {
                    resolved = true;
                    break;
                }
                chain.add(cursor);
                cursor = parentByIndex.get(cursor);
            }
            for (Integer idx : chain) {
                retainedByIndex.put(idx, resolved);
            }
        }

        Map<Integer, Long> retainedOrdinal = new HashMap<>(nodes.size() * 2 + 1);
        long nextOrdinal = 0L;
        for (CapturedNode node : nodes) {
            if (Boolean.TRUE.equals(retainedByIndex.get(node.index))) {
                retainedOrdinal.put(node.index, nextOrdinal);
                nextOrdinal += 1L;
            }
        }

        long hash = FNV_OFFSET_BASIS;
        long included = 0L;

        for (CapturedNode node : nodes) {
            if (!Boolean.TRUE.equals(retainedByIndex.get(node.index))) {
                continue;
            }
            included += 1L;

            long normalizedParent = -1L; // u64::MAX
            if (node.parentIndex != null) {
                Long ord = retainedOrdinal.get(node.parentIndex);
                if (ord != null) {
                    normalizedParent = ord.longValue();
                }
            }
            hash = writeU64LE(hash, normalizedParent);
            hash = writeU64LE(hash, (long) node.depth);
            hash = writeField(hash, node.source);
            hash = writeField(hash, node.elementType);
            hash = writeField(hash, node.identifier);
            hash = writeField(hash, node.label);
            hash = writeField(hash, node.title);
            hash = writeField(hash, node.value != null ? node.value : "");
            hash = writeField(hash, node.placeholderValue != null ? node.placeholderValue : "");
            int flags = (node.enabled ? 1 : 0)
                    | (node.selected ? 2 : 0)
                    | (node.hasFocus ? 4 : 0)
                    | (node.hittable ? 8 : 0);
            hash = writeByte(hash, flags);
            hash = writeI32LE(hash, node.x);
            hash = writeI32LE(hash, node.y);
            hash = writeI32LE(hash, node.width);
            hash = writeI32LE(hash, node.height);
            hash = writeByte(hash, 0x1e);
        }

        hash = writeU64LE(hash, included);
        return hash;
    }

    public static String computeHierarchyHashHex(List<CapturedNode> nodes) {
        return String.format(Locale.US, "%016x", computeHierarchyHash(nodes));
    }

    /**
     * Collects raw, uncollapsed visible descriptors so the Rust driver can apply exact secret
     * redaction BEFORE whitespace normalization.
     */
    public static List<String> extractRawVisibleSample(List<CapturedNode> nodes) {
        List<String> items = new ArrayList<>();
        Set<String> seen = new HashSet<>();
        for (CapturedNode node : nodes) {
            if (!node.isOnScreen) {
                continue;
            }
            if (isVolatileSystemChrome(node.source, node.elementType, node.identifier)) {
                continue;
            }
            String[] candidates = new String[]{
                    node.label,
                    node.title,
                    node.value,
                    node.placeholderValue,
                    node.identifier
            };
            for (String raw : candidates) {
                if (raw == null || raw.trim().isEmpty()) {
                    continue;
                }
                if (seen.add(raw)) {
                    items.add(raw);
                    if (items.size() >= MAX_VISIBLE_SAMPLE_ITEMS) {
                        return items;
                    }
                }
            }
        }
        return items;
    }

    public static JSONArray extractRawVisibleSampleJson(List<CapturedNode> nodes) {
        JSONArray arr = new JSONArray();
        for (String item : extractRawVisibleSample(nodes)) {
            arr.put(item);
        }
        return arr;
    }

    private static long writeByte(long hash, int b) {
        hash ^= (b & 0xFFL);
        return hash * FNV_PRIME;
    }

    private static long writeU64LE(long hash, long value) {
        for (int i = 0; i < 8; i++) {
            hash = writeByte(hash, (int) ((value >>> (i * 8)) & 0xFFL));
        }
        return hash;
    }

    private static long writeI32LE(long hash, int value) {
        for (int i = 0; i < 4; i++) {
            hash = writeByte(hash, (value >>> (i * 8)) & 0xFF);
        }
        return hash;
    }

    private static long writeField(long hash, String text) {
        if (text != null && !text.isEmpty()) {
            byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
            for (byte b : bytes) {
                hash = writeByte(hash, b & 0xFF);
            }
        }
        return writeByte(hash, 0x1f);
    }

    private static String simplifyClassName(String raw) {
        if (raw == null || raw.isEmpty()) {
            return "View";
        }
        int lastDot = raw.lastIndexOf('.');
        if (lastDot >= 0 && lastDot + 1 < raw.length()) {
            return raw.substring(lastDot + 1);
        }
        return raw;
    }

    private static String charSeqToString(CharSequence cs) {
        return cs != null ? cs.toString() : "";
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
