import Foundation
import XCTest

final class XcuiActions {
    private static let maxHierarchyNodes = 2_048
    private static let maxHierarchyDepth = 64
    private static let springboardBundleId = "com.apple.springboard"

    func handleHealth() -> HttpResponse {
        HttpResponse.json(200, object: [
            "status": "ok",
            "agent": "DejaXCTestRunner",
            "version": "1",
            "pid": Int(getpid()),
        ])
    }

    func handle(_ request: HttpRequest) -> HttpResponse {
        switch (request.method, request.path) {
        case ("GET", "/health"):
            return handleHealth()
        case ("GET", "/appState"):
            return handleAppState(request)
        case ("GET", "/hierarchy"):
            return handleHierarchy(request)
        case ("POST", "/tap"):
            return handleTap(request)
        case ("POST", "/swipe"):
            return handleSwipe(request)
        case ("POST", "/type"):
            return handleType(request)
        case ("POST", "/key"):
            return handleKey(request)
        default:
            return HttpResponse.jsonError(status: 404, message: "unknown route \(request.method) \(request.path)")
        }
    }

    private func handleAppState(_ request: HttpRequest) -> HttpResponse {
        guard let appId = request.queryItems["appId"], !appId.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing required query parameter: appId")
        }
        let app = XCUIApplication(bundleIdentifier: appId)
        let (stateName, stateCode) = describeAppState(app.state)
        return HttpResponse.json(200, object: [
            "appId": appId,
            "state": stateName,
            "stateCode": stateCode,
        ])
    }

    private func handleHierarchy(_ request: HttpRequest) -> HttpResponse {
        guard let appId = request.queryItems["appId"], !appId.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing required query parameter: appId")
        }
        let includeSystemUi = parseBoolParam(request.queryItems["includeSystemUi"])
        let app = XCUIApplication(bundleIdentifier: appId)
        let state = app.state
        let (stateName, stateCode) = describeAppState(state)

        var nodes: [[String: Any]] = []
        var truncated = false

        if isRunningState(state) {
            do {
                let snapshot = try app.snapshot()
                let rootBounds = sanitizeRect(snapshot.frame)
                appendSnapshotTree(
                    snapshot,
                    parentIndex: nil,
                    depth: 0,
                    source: "app",
                    rootBounds: rootBounds,
                    nodes: &nodes,
                    truncated: &truncated
                )
            } catch {
                return HttpResponse.jsonError(
                    status: 500,
                    message: "failed to capture app hierarchy snapshot: \(error.localizedDescription)"
                )
            }
        }

        if (state != .runningForeground || includeSystemUi) && !truncated {
            let springboard = XCUIApplication(bundleIdentifier: Self.springboardBundleId)
            if let sbSnapshot = try? springboard.snapshot() {
                let sbRootBounds = sanitizeRect(sbSnapshot.frame)
                appendSnapshotTree(
                    sbSnapshot,
                    parentIndex: nil,
                    depth: 0,
                    source: "springboard",
                    rootBounds: sbRootBounds,
                    nodes: &nodes,
                    truncated: &truncated
                )
            }
        }

        return HttpResponse.json(200, object: [
            "appId": appId,
            "state": stateName,
            "stateCode": stateCode,
            "truncated": truncated,
            "nodeCount": nodes.count,
            "nodes": nodes,
        ])
    }

    private func handleTap(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = json["appId"] as? String, !appId.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing appId")
        }
        guard let x = numberValue(json["x"]), let y = numberValue(json["y"]),
              x.isFinite, y.isFinite, x >= 0, y >= 0 else {
            return HttpResponse.jsonError(status: 400, message: "x and y must be finite non-negative numbers")
        }

        let count = max(1, min(10, intValue(json["count"]) ?? 1))
        let durationMs = max(0, min(30_000, intValue(json["durationMs"]) ?? 0))

        let app = XCUIApplication(bundleIdentifier: appId)
        let origin = app.coordinate(withNormalizedOffset: CGVector(dx: 0, dy: 0))
        let target = origin.withOffset(CGVector(dx: x, dy: y))

        if durationMs > 0 {
            target.press(forDuration: Double(durationMs) / 1000.0)
        } else if count == 2 {
            target.doubleTap()
        } else {
            for _ in 0..<count {
                target.tap()
            }
        }

        return HttpResponse.json(200, object: ["ok": true])
    }

    private func handleSwipe(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = json["appId"] as? String, !appId.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing appId")
        }
        guard let startX = numberValue(json["startX"]),
              let startY = numberValue(json["startY"]),
              let endX = numberValue(json["endX"]),
              let endY = numberValue(json["endY"]),
              startX.isFinite, startY.isFinite, endX.isFinite, endY.isFinite,
              startX >= 0, startY >= 0, endX >= 0, endY >= 0 else {
            return HttpResponse.jsonError(status: 400, message: "swipe coordinates must be finite non-negative numbers")
        }

        let durationMs = max(50, min(10_000, intValue(json["durationMs"]) ?? 300))
        let distance = hypot(endX - startX, endY - startY)
        let rawVelocity = distance / (Double(durationMs) / 1000.0)
        let clampedVelocity = max(50.0, min(5_000.0, rawVelocity.isFinite ? rawVelocity : 500.0))

        let app = XCUIApplication(bundleIdentifier: appId)
        let origin = app.coordinate(withNormalizedOffset: CGVector(dx: 0, dy: 0))
        let start = origin.withOffset(CGVector(dx: startX, dy: startY))
        let end = origin.withOffset(CGVector(dx: endX, dy: endY))

        start.press(
            forDuration: 0.01,
            thenDragTo: end,
            withVelocity: XCUIGestureVelocity(rawValue: CGFloat(clampedVelocity)),
            thenHoldForDuration: 0.0
        )

        return HttpResponse.json(200, object: ["ok": true])
    }

    private func handleType(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = json["appId"] as? String, !appId.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing appId")
        }
        guard let text = json["text"] as? String else {
            return HttpResponse.jsonError(status: 400, message: "missing text")
        }
        let settleKeyboard = (json["settleKeyboard"] as? Bool) ?? true

        let app = XCUIApplication(bundleIdentifier: appId)

        // Pre-type focus settle: wait up to 1.5s for software keyboard or any focused snapshot element.
        var hadSoftwareKeyboard = false
        let focusDeadline = Date().addingTimeInterval(1.5)
        while Date() < focusDeadline {
            if app.keyboards.count > 0 {
                hadSoftwareKeyboard = true
                break
            }
            if let snap = try? app.snapshot(), snapshotHasFocus(snap) {
                break
            }
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }

        if !hadSoftwareKeyboard && app.keyboards.count > 0 {
            hadSoftwareKeyboard = true
        }

        app.typeText(text)

        // DEJA-58 post-type settle: wait until keyboard is visible with identical positive height
        // across two consecutive polls (~120ms apart), without assuming a Return/Done button exists.
        if settleKeyboard && hadSoftwareKeyboard {
            let settleDeadline = Date().addingTimeInterval(2.5)
            var previousHeight: Int?
            while Date() < settleDeadline {
                let kb = app.keyboards.firstMatch
                let exists = kb.exists
                let rawHeight = exists ? kb.frame.height : 0
                let height = (exists && rawHeight.isFinite && rawHeight > 0)
                    ? Int(rawHeight.rounded())
                    : 0
                if exists && height > 0 {
                    if previousHeight == height {
                        break
                    }
                    previousHeight = height
                } else {
                    previousHeight = nil
                }
                RunLoop.current.run(until: Date().addingTimeInterval(0.12))
            }
        }

        return HttpResponse.json(200, object: [
            "ok": true,
            "hadSoftwareKeyboard": hadSoftwareKeyboard,
        ])
    }

    private func handleKey(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = json["appId"] as? String, !appId.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing appId")
        }
        guard let key = json["key"] as? String, !key.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing key")
        }
        let modifiers = (json["modifiers"] as? [String]) ?? []
        if !modifiers.isEmpty {
            return HttpResponse.jsonError(status: 400, message: "keyboard modifiers are not supported on iOS Simulator")
        }
        let count = max(1, min(100, intValue(json["count"]) ?? 1))

        if key.lowercased() == "home" {
            for _ in 0..<count {
                XCUIDevice.shared.press(.home)
            }
            return HttpResponse.json(200, object: ["ok": true])
        }

        guard let mapped = mapKeyboardKey(key) else {
            return HttpResponse.jsonError(status: 400, message: "unsupported key: \(key)")
        }

        let app = XCUIApplication(bundleIdentifier: appId)
        for _ in 0..<count {
            app.typeText(mapped)
        }
        return HttpResponse.json(200, object: ["ok": true])
    }

    private func mapKeyboardKey(_ key: String) -> String? {
        switch key.lowercased() {
        case "return", "enter":
            return XCUIKeyboardKey.return.rawValue
        case "delete", "backspace":
            return XCUIKeyboardKey.delete.rawValue
        case "forwarddelete", "forward_delete":
            return XCUIKeyboardKey.forwardDelete.rawValue
        case "tab":
            return XCUIKeyboardKey.tab.rawValue
        case "space":
            return XCUIKeyboardKey.space.rawValue
        case "escape", "esc":
            return XCUIKeyboardKey.escape.rawValue
        case "up", "uparrow":
            return XCUIKeyboardKey.upArrow.rawValue
        case "down", "downarrow":
            return XCUIKeyboardKey.downArrow.rawValue
        case "left", "leftarrow":
            return XCUIKeyboardKey.leftArrow.rawValue
        case "right", "rightarrow":
            return XCUIKeyboardKey.rightArrow.rawValue
        case "home_key":
            return XCUIKeyboardKey.home.rawValue
        case "end":
            return XCUIKeyboardKey.end.rawValue
        case "pageup", "page_up":
            return XCUIKeyboardKey.pageUp.rawValue
        case "pagedown", "page_down":
            return XCUIKeyboardKey.pageDown.rawValue
        default:
            if key.count == 1 {
                return key
            }
            return nil
        }
    }

    private func appendSnapshotTree(
        _ snapshot: XCUIElementSnapshot,
        parentIndex: Int?,
        depth: Int,
        source: String,
        rootBounds: CGRect,
        nodes: inout [[String: Any]],
        truncated: inout Bool
    ) {
        if nodes.count >= Self.maxHierarchyNodes {
            truncated = true
            return
        }

        let currentIndex = nodes.count
        let rect = sanitizeRect(snapshot.frame)
        let hittable = snapshot.isEnabled
            && rect.width > 0
            && rect.height > 0
            && rect.intersects(rootBounds)

        var node: [String: Any] = [
            "index": currentIndex,
            "depth": depth,
            "source": source,
            "elementType": describeElementType(snapshot.elementType),
            "identifier": snapshot.identifier,
            "label": snapshot.label,
            "title": snapshot.title,
            "enabled": snapshot.isEnabled,
            "selected": snapshot.isSelected,
            "hasFocus": snapshot.hasFocus,
            "hittable": hittable,
            "frame": [
                "x": Double(rect.origin.x),
                "y": Double(rect.origin.y),
                "width": Double(rect.size.width),
                "height": Double(rect.size.height),
            ],
        ]
        if let parentIndex {
            node["parentIndex"] = parentIndex
        }
        if let valueStr = stringifyValue(snapshot.value) {
            node["value"] = valueStr
        }
        if let placeholder = snapshot.placeholderValue, !placeholder.isEmpty {
            node["placeholderValue"] = placeholder
        }
        nodes.append(node)

        if depth >= Self.maxHierarchyDepth {
            if !snapshot.children.isEmpty {
                truncated = true
            }
            return
        }

        for child in snapshot.children {
            if nodes.count >= Self.maxHierarchyNodes {
                truncated = true
                return
            }
            appendSnapshotTree(
                child,
                parentIndex: currentIndex,
                depth: depth + 1,
                source: source,
                rootBounds: rootBounds,
                nodes: &nodes,
                truncated: &truncated
            )
        }
    }

    private func snapshotHasFocus(_ snapshot: XCUIElementSnapshot, depth: Int = 0) -> Bool {
        if snapshot.hasFocus {
            return true
        }
        if depth >= Self.maxHierarchyDepth {
            return false
        }
        for child in snapshot.children {
            if snapshotHasFocus(child, depth: depth + 1) {
                return true
            }
        }
        return false
    }

    private func sanitizeRect(_ rect: CGRect) -> CGRect {
        let x = rect.origin.x.isFinite ? rect.origin.x : 0
        let y = rect.origin.y.isFinite ? rect.origin.y : 0
        let w = rect.size.width.isFinite && rect.size.width > 0 ? rect.size.width : 0
        let h = rect.size.height.isFinite && rect.size.height > 0 ? rect.size.height : 0
        return CGRect(x: x, y: y, width: w, height: h)
    }

    private func stringifyValue(_ value: Any?) -> String? {
        guard let value else { return nil }
        if let s = value as? String {
            return s.isEmpty ? nil : s
        }
        if let n = value as? NSNumber {
            return n.stringValue
        }
        let desc = String(describing: value)
        return desc.isEmpty ? nil : desc
    }

    private func isRunningState(_ state: XCUIApplication.State) -> Bool {
        switch state {
        case .runningForeground, .runningBackground, .runningBackgroundSuspended:
            return true
        default:
            return false
        }
    }

    private func describeAppState(_ state: XCUIApplication.State) -> (String, Int) {
        switch state {
        case .unknown:
            return ("unknown", 0)
        case .notRunning:
            return ("not_running", 1)
        case .runningBackgroundSuspended:
            return ("running_background_suspended", 2)
        case .runningBackground:
            return ("running_background", 3)
        case .runningForeground:
            return ("running_foreground", 4)
        @unknown default:
            return ("unknown", Int(state.rawValue))
        }
    }

    private func describeElementType(_ elementType: XCUIElement.ElementType) -> String {
        switch elementType {
        case .any: return "Any"
        case .other: return "Other"
        case .application: return "Application"
        case .group: return "Group"
        case .window: return "Window"
        case .sheet: return "Sheet"
        case .alert: return "Alert"
        case .dialog: return "Dialog"
        case .button: return "Button"
        case .radioButton: return "RadioButton"
        case .radioGroup: return "RadioGroup"
        case .checkBox: return "CheckBox"
        case .disclosureTriangle: return "DisclosureTriangle"
        case .popUpButton: return "PopUpButton"
        case .comboBox: return "ComboBox"
        case .menuButton: return "MenuButton"
        case .toolbarButton: return "ToolbarButton"
        case .popover: return "Popover"
        case .keyboard: return "Keyboard"
        case .key: return "Key"
        case .navigationBar: return "NavigationBar"
        case .tabBar: return "TabBar"
        case .tabGroup: return "TabGroup"
        case .toolbar: return "Toolbar"
        case .statusBar: return "StatusBar"
        case .table: return "Table"
        case .tableRow: return "TableRow"
        case .tableColumn: return "TableColumn"
        case .outline: return "Outline"
        case .outlineRow: return "OutlineRow"
        case .browser: return "Browser"
        case .collectionView: return "CollectionView"
        case .slider: return "Slider"
        case .pageIndicator: return "PageIndicator"
        case .progressIndicator: return "ProgressIndicator"
        case .activityIndicator: return "ActivityIndicator"
        case .segmentedControl: return "SegmentedControl"
        case .picker: return "Picker"
        case .pickerWheel: return "PickerWheel"
        case .switch: return "Switch"
        case .toggle: return "Toggle"
        case .link: return "Link"
        case .image: return "Image"
        case .icon: return "Icon"
        case .searchField: return "SearchField"
        case .scrollView: return "ScrollView"
        case .scrollBar: return "ScrollBar"
        case .staticText: return "StaticText"
        case .textField: return "TextField"
        case .secureTextField: return "SecureTextField"
        case .datePicker: return "DatePicker"
        case .textView: return "TextView"
        case .menu: return "Menu"
        case .menuItem: return "MenuItem"
        case .menuBar: return "MenuBar"
        case .menuBarItem: return "MenuBarItem"
        case .map: return "Map"
        case .webView: return "WebView"
        case .cell: return "Cell"
        case .stepper: return "Stepper"
        case .tab: return "Tab"
        default: return "ElementType(\(elementType.rawValue))"
        }
    }

    private func parseJsonBody(_ data: Data) -> [String: Any]? {
        guard !data.isEmpty,
              let obj = try? JSONSerialization.jsonObject(with: data, options: []),
              let dict = obj as? [String: Any] else {
            return nil
        }
        return dict
    }

    private func parseBoolParam(_ raw: String?) -> Bool {
        guard let raw else { return false }
        let lowered = raw.lowercased()
        return lowered == "true" || lowered == "1" || lowered == "yes"
    }

    private func numberValue(_ raw: Any?) -> Double? {
        if let d = raw as? Double { return d }
        if let i = raw as? Int { return Double(i) }
        if let n = raw as? NSNumber { return n.doubleValue }
        return nil
    }

    private func intValue(_ raw: Any?) -> Int? {
        if let i = raw as? Int { return i }
        if let n = raw as? NSNumber { return n.intValue }
        return nil
    }
}
