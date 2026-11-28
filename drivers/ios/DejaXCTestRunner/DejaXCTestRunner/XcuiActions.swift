import Foundation
import XCTest

final class XcuiActions {
    private static let maxHierarchyNodes = 2_048
    private static let maxHierarchyDepth = 64
    private static let maxAppIdLength = 255
    private static let maxKeyLength = 64
    private static let maxTextLength = 16_384
    private static let maxSelectorLength = 4_096
    private static let maxVisibleSampleItems = 30
    private static let springboardBundleId = "com.apple.springboard"

    private static let fnvOffsetBasis: UInt64 = 0xcbf2_9ce4_8422_2325
    private static let fnvPrime: UInt64 = 0x0100_0000_01b3

    private struct CapturedNode {
        let index: Int
        let parentIndex: Int?
        let depth: Int
        let source: String
        let elementType: String
        let identifier: String
        let label: String
        let title: String
        let value: String?
        let placeholderValue: String?
        let enabled: Bool
        let selected: Bool
        let hasFocus: Bool
        let hittable: Bool
        let isOnScreen: Bool
        let frame: CGRect
        let inVolatileChrome: Bool

        func toJsonDict() -> [String: Any] {
            var dict: [String: Any] = [
                "index": index,
                "depth": depth,
                "source": source,
                "elementType": elementType,
                "identifier": identifier,
                "label": label,
                "title": title,
                "enabled": enabled,
                "selected": selected,
                "hasFocus": hasFocus,
                "hittable": hittable,
                "isOnScreen": isOnScreen,
                "frame": [
                    "x": Double(frame.origin.x),
                    "y": Double(frame.origin.y),
                    "width": Double(frame.size.width),
                    "height": Double(frame.size.height),
                ],
            ]
            if let parentIndex {
                dict["parentIndex"] = parentIndex
            }
            if let value {
                dict["value"] = value
            }
            if let placeholderValue {
                dict["placeholderValue"] = placeholderValue
            }
            return dict
        }
    }

    private struct HierarchyCapture {
        let appId: String
        let stateName: String
        let stateCode: Int
        let truncated: Bool
        let hierarchyHash: String
        let nodes: [CapturedNode]

        func toJsonObject(knownHash: String?) -> [String: Any] {
            if let knownHash, knownHash == hierarchyHash {
                return [
                    "appId": appId,
                    "state": stateName,
                    "stateCode": stateCode,
                    "truncated": truncated,
                    "hierarchyHash": hierarchyHash,
                    "unchanged": true,
                    "nodeCount": 0,
                    "nodes": [] as [[String: Any]],
                ]
            }
            return [
                "appId": appId,
                "state": stateName,
                "stateCode": stateCode,
                "truncated": truncated,
                "hierarchyHash": hierarchyHash,
                "unchanged": false,
                "nodeCount": nodes.count,
                "nodes": nodes.map { $0.toJsonDict() },
            ]
        }
    }

    private struct FnvHasher {
        var state: UInt64 = XcuiActions.fnvOffsetBasis
        var included: UInt64 = 0

        mutating func writeByte(_ b: UInt8) {
            state ^= UInt64(b)
            state = state &* XcuiActions.fnvPrime
        }

        mutating func writeBytes(_ bytes: [UInt8]) {
            for b in bytes {
                writeByte(b)
            }
        }

        mutating func writeU64LE(_ value: UInt64) {
            var le = value.littleEndian
            withUnsafeBytes(of: &le) { buf in
                for b in buf {
                    writeByte(b)
                }
            }
        }

        mutating func writeI32LE(_ value: Int32) {
            var le = value.littleEndian
            withUnsafeBytes(of: &le) { buf in
                for b in buf {
                    writeByte(b)
                }
            }
        }

        mutating func writeField(_ text: String) {
            for b in text.utf8 {
                writeByte(b)
            }
            writeByte(0x1f)
        }

        mutating func recordNode(_ node: CapturedNode, retainedParentOrdinal: UInt64?) {
            included &+= 1
            writeU64LE(retainedParentOrdinal ?? UInt64.max)
            writeU64LE(UInt64(node.depth))
            writeField(node.source)
            writeField(node.elementType)
            writeField(node.identifier)
            writeField(node.label)
            writeField(node.title)
            writeField(node.value ?? "")
            writeField(node.placeholderValue ?? "")
            let flags: UInt8 =
                (node.enabled ? 1 : 0)
                | (node.selected ? 2 : 0)
                | (node.hasFocus ? 4 : 0)
                | (node.hittable ? 8 : 0)
            writeByte(flags)
            writeI32LE(Self.roundToI32(node.frame.origin.x))
            writeI32LE(Self.roundToI32(node.frame.origin.y))
            writeI32LE(Self.roundToI32(node.frame.size.width))
            writeI32LE(Self.roundToI32(node.frame.size.height))
            writeByte(0x1e)
        }

        mutating func finalizeHex() -> String {
            writeU64LE(included)
            return String(format: "%016llx", state)
        }

        private static func roundToI32(_ v: CGFloat) -> Int32 {
            let d = Double(v)
            guard d.isFinite else { return 0 }
            let rounded = d.rounded()
            if rounded >= Double(Int32.max) { return Int32.max }
            if rounded <= Double(Int32.min) { return Int32.min }
            return Int32(rounded)
        }
    }

    private enum ValidatedAction {
        case tap(x: Double, y: Double, count: Int, durationMs: Int)
        case swipe(startX: Double, startY: Double, endX: Double, endY: Double, durationMs: Int, holdDurationMs: Int)
        case type(text: String, settleKeyboard: Bool)
        case key(key: String, mapped: String?, count: Int)
    }

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
        case ("POST", "/act"):
            return handleAct(request)
        case ("POST", "/waitForSelector"):
            return handleWaitForSelector(request)
        case ("POST", "/warmup"):
            return handleWarmup(request)
        default:
            return HttpResponse.jsonError(status: 404, message: "unknown route \(request.method) \(request.path)")
        }
    }

    private func handleAppState(_ request: HttpRequest) -> HttpResponse {
        guard let appId = validateAppId(request.queryItems["appId"]) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid query parameter: appId")
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
        guard let appId = validateAppId(request.queryItems["appId"]) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid query parameter: appId")
        }
        let includeSystemUi = parseBoolParam(request.queryItems["includeSystemUi"])
        let knownHash: String?
        switch parseOptionalKnownHash(request.queryItems["knownHash"]) {
        case .success(let normalized):
            knownHash = normalized
        case .failure(let msg):
            return HttpResponse.jsonError(status: 400, message: msg)
        }

        switch captureHierarchy(appId: appId, includeSystemUi: includeSystemUi) {
        case .success(let capture):
            return HttpResponse.json(200, object: capture.toJsonObject(knownHash: knownHash))
        case .failure(let msg):
            return HttpResponse.jsonError(status: 500, message: msg)
        }
    }

    private func handleTap(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = validateAppId(json["appId"] as? String) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid appId")
        }
        switch parseTapAction(json) {
        case .failure(let err):
            return HttpResponse.jsonError(status: 400, message: err.message)
        case .success(let action):
            _ = executeAction(appId: appId, action: action)
            return HttpResponse.json(200, object: ["ok": true])
        }
    }

    private func handleSwipe(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = validateAppId(json["appId"] as? String) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid appId")
        }
        switch parseSwipeAction(json) {
        case .failure(let err):
            return HttpResponse.jsonError(status: 400, message: err.message)
        case .success(let action):
            _ = executeAction(appId: appId, action: action)
            return HttpResponse.json(200, object: ["ok": true])
        }
    }

    private func handleType(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = validateAppId(json["appId"] as? String) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid appId")
        }
        switch parseTypeAction(json) {
        case .failure(let err):
            return HttpResponse.jsonError(status: 400, message: err.message)
        case .success(let action):
            let hadKeyboard = executeAction(appId: appId, action: action)
            return HttpResponse.json(200, object: [
                "ok": true,
                "hadSoftwareKeyboard": hadKeyboard ?? false,
            ])
        }
    }

    private func handleKey(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = validateAppId(json["appId"] as? String) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid appId")
        }
        switch parseKeyAction(json) {
        case .failure(let err):
            return HttpResponse.jsonError(status: 400, message: err.message)
        case .success(let action):
            _ = executeAction(appId: appId, action: action)
            return HttpResponse.json(200, object: ["ok": true])
        }
    }

    /// Compound action + optional post-action delay + optional hierarchy observation in one round-trip.
    private func handleAct(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = validateAppId(json["appId"] as? String) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid appId")
        }
        guard let actionName = (json["action"] as? String)?.lowercased(), !actionName.isEmpty else {
            return HttpResponse.jsonError(status: 400, message: "missing action")
        }
        let knownHash: String?
        if let rawKnown = json["knownHash"], !(rawKnown is NSNull) {
            guard let knownStr = rawKnown as? String else {
                return HttpResponse.jsonError(status: 400, message: "knownHash must be a 16-character hexadecimal string")
            }
            switch parseOptionalKnownHash(knownStr) {
            case .success(let normalized):
                knownHash = normalized
            case .failure(let msg):
                return HttpResponse.jsonError(status: 400, message: msg)
            }
        } else {
            knownHash = nil
        }

        let actionResult: Result<ValidatedAction, ValidationError>
        switch actionName {
        case "tap":
            actionResult = parseTapAction(json)
        case "swipe":
            actionResult = parseSwipeAction(json)
        case "type":
            actionResult = parseTypeAction(json)
        case "key":
            actionResult = parseKeyAction(json)
        default:
            return HttpResponse.jsonError(status: 400, message: "unsupported action: \(actionName)")
        }

        let validatedAction: ValidatedAction
        switch actionResult {
        case .success(let a):
            validatedAction = a
        case .failure(let err):
            return HttpResponse.jsonError(status: 400, message: err.message)
        }

        let postActionDelayMs = max(0, min(5_000, intValue(json["postActionDelayMs"]) ?? 0))
        let returnHierarchy = boolValue(json["returnHierarchy"]) ?? false
        let includeSystemUi = boolValue(json["includeSystemUi"]) ?? false

        let started = Date()
        let hadSoftwareKeyboard = executeAction(appId: appId, action: validatedAction)

        if postActionDelayMs > 0 {
            RunLoop.current.run(until: Date().addingTimeInterval(Double(postActionDelayMs) / 1000.0))
        }

        var response: [String: Any] = ["ok": true]
        if let hadSoftwareKeyboard {
            response["hadSoftwareKeyboard"] = hadSoftwareKeyboard
        }

        if returnHierarchy {
            switch captureHierarchy(appId: appId, includeSystemUi: includeSystemUi) {
            case .success(let capture):
                response["hierarchy"] = capture.toJsonObject(knownHash: knownHash)
            case .failure(let msg):
                // Preserve `ok: true` because the action already ran; expose the observation
                // error separately so callers can retry only the snapshot without re-running
                // a non-idempotent tap/type/swipe.
                response["hierarchyError"] = msg
            }
        }

        response["elapsedMs"] = elapsedMs(since: started)
        return HttpResponse.json(200, object: response)
    }

    /// Server-side adaptive selector / stability wait loop (`15..150 ms` doubling backoff).
    /// Returns HTTP 200 with `matched: false` and pruned `visibleSample` on timeout so callers
    /// never need a second round-trip to inspect what was on screen.
    private func handleWaitForSelector(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = validateAppId(json["appId"] as? String) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid appId")
        }

        let identifier: String?
        switch parseOptionalSelectorField(json["identifier"], name: "identifier", maxLen: Self.maxSelectorLength) {
        case .success(let value):
            identifier = value
        case .failure(let msg):
            return HttpResponse.jsonError(status: 400, message: msg)
        }

        let text: String?
        switch parseOptionalSelectorField(json["text"], name: "text", maxLen: Self.maxSelectorLength) {
        case .success(let value):
            text = value
        case .failure(let msg):
            return HttpResponse.jsonError(status: 400, message: msg)
        }

        let textMatchRaw: String
        if let rawTextMatch = json["textMatch"], !(rawTextMatch is NSNull) {
            guard let str = rawTextMatch as? String else {
                return HttpResponse.jsonError(status: 400, message: "textMatch must be 'exact' or 'contains'")
            }
            textMatchRaw = str.lowercased()
        } else {
            textMatchRaw = "contains"
        }
        guard textMatchRaw == "exact" || textMatchRaw == "contains" else {
            return HttpResponse.jsonError(status: 400, message: "textMatch must be 'exact' or 'contains'")
        }
        let exactText = textMatchRaw == "exact"

        let notVisible = boolValue(json["notVisible"]) ?? false
        let waitStable = boolValue(json["waitStable"]) ?? false
        if identifier == nil && text == nil && !(waitStable && !notVisible) {
            return HttpResponse.jsonError(
                status: 400,
                message: "waitForSelector requires identifier or text (or waitStable=true without notVisible)"
            )
        }

        let stablePolls = max(2, min(10, intValue(json["stablePolls"]) ?? 2))
        let timeoutMs = max(0, min(60_000, intValue(json["timeoutMs"]) ?? 5_000))
        let includeSystemUi = boolValue(json["includeSystemUi"]) ?? false
        let returnHierarchy = boolValue(json["returnHierarchy"]) ?? false

        let started = Date()
        let deadline = started.addingTimeInterval(Double(timeoutMs) / 1000.0)
        var polls = 0
        var delayMs = 15
        var lastHash = ""
        var consecutiveSameHash = 0
        var lastCapture: HierarchyCapture?

        waitLoop: while true {
            if polls > 0 && Date() >= deadline {
                break
            }
            polls += 1
            switch captureHierarchy(appId: appId, includeSystemUi: includeSystemUi) {
            case .success(let capture):
                lastCapture = capture
                if capture.truncated {
                    // Truncated captures omit subtrees that may still be changing, so never
                    // count them toward `waitStable` stability streaks.
                    lastHash = ""
                    consecutiveSameHash = 0
                } else if capture.hierarchyHash == lastHash && !lastHash.isEmpty {
                    consecutiveSameHash += 1
                } else {
                    lastHash = capture.hierarchyHash
                    consecutiveSameHash = 1
                }

                // Reject matches completed after a positive timeout budget expired, while
                // preserving the single probe semantics for `timeoutMs == 0`.
                if timeoutMs > 0 && Date() > deadline {
                    break waitLoop
                }

                let (conditionMet, matchedNode) = evaluateWaitPredicate(
                    capture: capture,
                    identifier: identifier,
                    text: text,
                    exactText: exactText,
                    notVisible: notVisible
                )

                let stableMet = !waitStable || (!capture.truncated && consecutiveSameHash >= stablePolls)
                if conditionMet && stableMet {
                    var body: [String: Any] = [
                        "matched": true,
                        "elapsedMs": elapsedMs(since: started),
                        "polls": polls,
                        "hierarchyHash": capture.hierarchyHash,
                        "visibleSample": [] as [String],
                    ]
                    if let matchedNode {
                        body["node"] = matchedNode.toJsonDict()
                    }
                    if returnHierarchy {
                        body["hierarchy"] = capture.toJsonObject(knownHash: nil)
                    }
                    return HttpResponse.json(200, object: body)
                }
            case .failure:
                lastHash = ""
                consecutiveSameHash = 0
            }

            let remaining = deadline.timeIntervalSinceNow
            if remaining <= 0 {
                break
            }
            let sleepSec = min(Double(delayMs) / 1000.0, remaining)
            RunLoop.current.run(until: Date().addingTimeInterval(sleepSec))
            if Date() >= deadline {
                break
            }
            delayMs = min(150, delayMs * 2)
        }

        let visibleSample = lastCapture.map { extractVisibleSample($0.nodes) } ?? []
        var timeoutBody: [String: Any] = [
            "matched": false,
            "elapsedMs": elapsedMs(since: started),
            "polls": polls,
            "hierarchyHash": lastCapture?.hierarchyHash ?? "",
            "visibleSample": visibleSample,
        ]
        if returnHierarchy, let lastCapture {
            timeoutBody["hierarchy"] = lastCapture.toJsonObject(knownHash: nil)
        }
        return HttpResponse.json(200, object: timeoutBody)
    }

    /// Post-launch AX warmup helper: polls until the target app is running and returns a valid
    /// accessibility snapshot, absorbing the initial post-launch AX daemon startup latency.
    private func handleWarmup(_ request: HttpRequest) -> HttpResponse {
        guard let json = parseJsonBody(request.body) else {
            return HttpResponse.jsonError(status: 400, message: "invalid JSON body")
        }
        guard let appId = validateAppId(json["appId"] as? String) else {
            return HttpResponse.jsonError(status: 400, message: "missing or invalid appId")
        }
        let timeoutMs = max(0, min(60_000, intValue(json["timeoutMs"]) ?? 10_000))
        let includeSystemUi = boolValue(json["includeSystemUi"]) ?? false
        let returnHierarchy = boolValue(json["returnHierarchy"]) ?? false

        let started = Date()
        let deadline = started.addingTimeInterval(Double(timeoutMs) / 1000.0)
        var attempts = 0
        var delayMs = 15
        var lastError = "app did not reach running_foreground state with readable hierarchy"

        warmupLoop: while true {
            if attempts > 0 && Date() >= deadline {
                break
            }
            attempts += 1
            let app = XCUIApplication(bundleIdentifier: appId)
            if app.state == .runningForeground {
                switch captureHierarchy(appId: appId, includeSystemUi: includeSystemUi) {
                case .success(let capture):
                    if timeoutMs > 0 && Date() > deadline {
                        lastError = "warmup snapshot finished after timeoutMs=\(timeoutMs)"
                        break warmupLoop
                    }
                    if capture.stateCode == 4 && capture.nodes.contains(where: { $0.source == "app" }) {
                        var body: [String: Any] = [
                            "ok": true,
                            "appId": appId,
                            "state": capture.stateName,
                            "stateCode": capture.stateCode,
                            "elapsedMs": elapsedMs(since: started),
                            "nodeCount": capture.nodes.count,
                            "hierarchyHash": capture.hierarchyHash,
                        ]
                        if returnHierarchy {
                            body["hierarchy"] = capture.toJsonObject(knownHash: nil)
                        }
                        return HttpResponse.json(200, object: body)
                    } else {
                        lastError = "app state was \(capture.stateName) during warmup snapshot"
                    }
                case .failure(let msg):
                    lastError = msg
                }
            } else {
                let (stateName, _) = describeAppState(app.state)
                lastError = "app state is \(stateName)"
            }

            let remaining = deadline.timeIntervalSinceNow
            if remaining <= 0 {
                break
            }
            let sleepSec = min(Double(delayMs) / 1000.0, remaining)
            RunLoop.current.run(until: Date().addingTimeInterval(sleepSec))
            if Date() >= deadline {
                break
            }
            delayMs = min(150, delayMs * 2)
        }

        return HttpResponse.jsonError(status: 500, message: "warmup timed out: \(lastError)")
    }

    private struct ValidationError: Error {
        let message: String
    }

    private func parseTapAction(_ json: [String: Any]) -> Result<ValidatedAction, ValidationError> {
        guard let x = numberValue(json["x"]), let y = numberValue(json["y"]),
              x.isFinite, y.isFinite, x >= 0, y >= 0 else {
            return .failure(ValidationError(message: "x and y must be finite non-negative numbers"))
        }
        let count = max(1, min(10, intValue(json["count"]) ?? 1))
        let durationMs = max(0, min(30_000, intValue(json["durationMs"]) ?? 0))
        return .success(.tap(x: x, y: y, count: count, durationMs: durationMs))
    }

    private func parseSwipeAction(_ json: [String: Any]) -> Result<ValidatedAction, ValidationError> {
        guard let startX = numberValue(json["startX"]),
              let startY = numberValue(json["startY"]),
              let endX = numberValue(json["endX"]),
              let endY = numberValue(json["endY"]),
              startX.isFinite, startY.isFinite, endX.isFinite, endY.isFinite,
              startX >= 0, startY >= 0, endX >= 0, endY >= 0 else {
            return .failure(ValidationError(message: "swipe coordinates must be finite non-negative numbers"))
        }
        let durationMs = max(50, min(10_000, intValue(json["durationMs"]) ?? 300))
        let holdDurationMs = max(0, min(5_000, intValue(json["holdDurationMs"]) ?? 0))
        return .success(.swipe(
            startX: startX,
            startY: startY,
            endX: endX,
            endY: endY,
            durationMs: durationMs,
            holdDurationMs: holdDurationMs
        ))
    }

    private func parseTypeAction(_ json: [String: Any]) -> Result<ValidatedAction, ValidationError> {
        guard let text = json["text"] as? String else {
            return .failure(ValidationError(message: "missing text"))
        }
        if text.utf8.count > Self.maxTextLength {
            return .failure(ValidationError(message: "text exceeds maximum length"))
        }
        let settleKeyboard = boolValue(json["settleKeyboard"]) ?? true
        return .success(.type(text: text, settleKeyboard: settleKeyboard))
    }

    private func parseKeyAction(_ json: [String: Any]) -> Result<ValidatedAction, ValidationError> {
        guard let key = json["key"] as? String, !key.isEmpty, key.utf8.count <= Self.maxKeyLength else {
            return .failure(ValidationError(message: "missing or invalid key"))
        }
        let modifiers = (json["modifiers"] as? [String]) ?? []
        if !modifiers.isEmpty {
            return .failure(ValidationError(message: "keyboard modifiers are not supported on iOS Simulator"))
        }
        let count = max(1, min(100, intValue(json["count"]) ?? 1))
        if key.lowercased() == "home" {
            return .success(.key(key: key, mapped: nil, count: count))
        }
        guard let mapped = mapKeyboardKey(key) else {
            return .failure(ValidationError(message: "unsupported key: \(key)"))
        }
        return .success(.key(key: key, mapped: mapped, count: count))
    }

    private func executeAction(appId: String, action: ValidatedAction) -> Bool? {
        switch action {
        case .tap(let x, let y, let count, let durationMs):
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
            return nil

        case .swipe(let startX, let startY, let endX, let endY, let durationMs, let holdDurationMs):
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
                thenHoldForDuration: Double(holdDurationMs) / 1000.0
            )
            return nil

        case .type(let text, let settleKeyboard):
            return performType(appId: appId, text: text, settleKeyboard: settleKeyboard)

        case .key(let key, let mapped, let count):
            if key.lowercased() == "home" {
                for _ in 0..<count {
                    XCUIDevice.shared.press(.home)
                }
            } else if let mapped {
                let app = XCUIApplication(bundleIdentifier: appId)
                for _ in 0..<count {
                    app.typeText(mapped)
                }
            }
            return nil
        }
    }

    private func performType(appId: String, text: String, settleKeyboard: Bool) -> Bool {
        let app = XCUIApplication(bundleIdentifier: appId)

        // Pre-type focus settle: single-pass snapshot inspection avoids separate app.keyboards IPCs
        // on every poll while retaining an out-of-process keyboard fallback check.
        var hadSoftwareKeyboard = false
        var keyboardVisibleInSnapshot = false
        var checkedOutOfProcessKeyboard = false
        let focusDeadline = Date().addingTimeInterval(1.5)
        while Date() < focusDeadline {
            if let snap = try? app.snapshot() {
                let info = inspectSnapshotFocusAndKeyboard(snap)
                if info.keyboardHeight > 0 {
                    hadSoftwareKeyboard = true
                    keyboardVisibleInSnapshot = true
                    break
                }
                if info.hasFocus {
                    // Check once for out-of-process software keyboard (e.g. InputUI on simulator).
                    checkedOutOfProcessKeyboard = true
                    if app.keyboards.firstMatch.exists {
                        hadSoftwareKeyboard = true
                    }
                    break
                }
            }
            RunLoop.current.run(until: Date().addingTimeInterval(0.03))
        }

        if !hadSoftwareKeyboard && !checkedOutOfProcessKeyboard && app.keyboards.firstMatch.exists {
            hadSoftwareKeyboard = true
        }

        app.typeText(text)

        // DEJA-58 post-type settle: wait until keyboard is visible with identical positive height
        // across two consecutive polls (~120ms apart), without assuming a Return/Done button exists.
        if settleKeyboard && hadSoftwareKeyboard {
            let settleDeadline = Date().addingTimeInterval(2.5)
            var previousHeight: Int?
            while Date() < settleDeadline {
                let height: Int
                if keyboardVisibleInSnapshot, let snap = try? app.snapshot() {
                    height = inspectSnapshotFocusAndKeyboard(snap).keyboardHeight
                } else {
                    let kb = app.keyboards.firstMatch
                    let exists = kb.exists
                    let rawHeight = exists ? kb.frame.height : 0
                    height = (exists && rawHeight.isFinite && rawHeight > 0)
                        ? Int(rawHeight.rounded())
                        : 0
                }
                if height > 0 {
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

        return hadSoftwareKeyboard
    }

    private enum CaptureResult {
        case success(HierarchyCapture)
        case failure(String)
    }

    private func captureHierarchy(appId: String, includeSystemUi: Bool) -> CaptureResult {
        let app = XCUIApplication(bundleIdentifier: appId)
        let state = app.state
        let (stateName, stateCode) = describeAppState(state)

        var nodes: [CapturedNode] = []
        var truncated = false
        var hasher = FnvHasher()
        var nextRetainedOrdinal: UInt64 = 0

        // Only snapshot the target app while it is running in the foreground; snapshotting a
        // backgrounded or kernel-suspended app can stall XCTest's AX IPC for >30s, whereas
        // SpringBoard owns the visible screen whenever state != .runningForeground.
        if state == .runningForeground {
            do {
                let snapshot = try app.snapshot()
                let rootBounds = sanitizeRect(snapshot.frame)
                appendSnapshotTree(
                    snapshot,
                    parentIndex: nil,
                    retainedParentOrdinal: nil,
                    depth: 0,
                    source: "app",
                    clipBounds: rootBounds,
                    parentInVolatileChrome: false,
                    nodes: &nodes,
                    truncated: &truncated,
                    hasher: &hasher,
                    nextRetainedOrdinal: &nextRetainedOrdinal
                )
            } catch {
                return .failure("failed to capture app hierarchy snapshot: \(error.localizedDescription)")
            }
        }

        if (state != .runningForeground || includeSystemUi) && !truncated {
            let springboard = XCUIApplication(bundleIdentifier: Self.springboardBundleId)
            do {
                let sbSnapshot = try springboard.snapshot()
                let sbRootBounds = sanitizeRect(sbSnapshot.frame)
                appendSnapshotTree(
                    sbSnapshot,
                    parentIndex: nil,
                    retainedParentOrdinal: nil,
                    depth: 0,
                    source: "springboard",
                    clipBounds: sbRootBounds,
                    parentInVolatileChrome: false,
                    nodes: &nodes,
                    truncated: &truncated,
                    hasher: &hasher,
                    nextRetainedOrdinal: &nextRetainedOrdinal
                )
            } catch {
                if state != .runningForeground {
                    return .failure(
                        "failed to capture SpringBoard hierarchy snapshot: \(error.localizedDescription)"
                    )
                }
                // System UI overlay was requested alongside a foreground app, but could not be
                // observed; mark truncated so `notVisible` waits do not treat omission as absence.
                truncated = true
            }
        }

        return .success(HierarchyCapture(
            appId: appId,
            stateName: stateName,
            stateCode: stateCode,
            truncated: truncated,
            hierarchyHash: hasher.finalizeHex(),
            nodes: nodes
        ))
    }

    private func evaluateWaitPredicate(
        capture: HierarchyCapture,
        identifier: String?,
        text: String?,
        exactText: Bool,
        notVisible: Bool
    ) -> (Bool, CapturedNode?) {
        if identifier == nil && text == nil {
            // Pure waitStable mode requires a foreground capture containing target-app nodes.
            let hasForegroundAppNodes = capture.stateCode == 4
                && capture.nodes.contains(where: { $0.source == "app" })
            return (hasForegroundAppNodes, nil)
        }

        let matchedNode = findMatchingNode(
            in: capture.nodes,
            identifier: identifier,
            text: text,
            exactText: exactText
        )

        if notVisible {
            // Never treat a truncated hierarchy as proof of absence: the selector may live in
            // the omitted portion of the tree.
            if matchedNode == nil && capture.truncated {
                return (false, nil)
            }
            return (matchedNode == nil, nil)
        }
        return (matchedNode != nil, matchedNode)
    }

    private func findMatchingNode(
        in nodes: [CapturedNode],
        identifier: String?,
        text: String?,
        exactText: Bool
    ) -> CapturedNode? {
        func matches(_ node: CapturedNode) -> Bool {
            guard node.isOnScreen else { return false }
            if let identifier, node.identifier != identifier {
                return false
            }
            if let text {
                let candidates = [
                    node.label,
                    node.title,
                    node.value ?? "",
                    node.placeholderValue ?? "",
                ]
                let textHit = candidates.contains { candidate in
                    guard !candidate.isEmpty else { return false }
                    return exactText ? (candidate == text) : candidate.contains(text)
                }
                if !textHit {
                    return false
                }
            }
            return true
        }

        if let hittableMatch = nodes.first(where: { $0.hittable && matches($0) }) {
            return hittableMatch
        }
        return nodes.first(where: matches)
    }

    private func extractVisibleSample(_ nodes: [CapturedNode]) -> [String] {
        var result: [String] = []
        var seen = Set<String>()
        for node in nodes {
            if node.inVolatileChrome || !node.isOnScreen { continue }
            let rawCandidates: [String] = [
                node.label,
                node.title,
                node.identifier,
                node.value ?? "",
                node.placeholderValue ?? "",
            ]
            for raw in rawCandidates {
                let normalized = raw
                    .components(separatedBy: .whitespacesAndNewlines)
                    .filter { !$0.isEmpty }
                    .joined(separator: " ")
                if normalized.isEmpty { continue }
                if seen.insert(normalized).inserted {
                    result.append(normalized)
                    if result.count >= Self.maxVisibleSampleItems {
                        return result
                    }
                }
            }
        }
        return result
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

    private func equalsAsciiIgnoreCase(_ lhs: String, _ rhs: String) -> Bool {
        let a = lhs.utf8
        let b = rhs.utf8
        guard a.count == b.count else { return false }
        for (ba, bb) in zip(a, b) {
            let fa = (ba >= 65 && ba <= 90) ? (ba + 32) : ba
            let fb = (bb >= 65 && bb <= 90) ? (bb + 32) : bb
            if fa != fb { return false }
        }
        return true
    }

    private func isVolatileSystemChrome(source: String, elementType: String, identifier: String) -> Bool {
        if equalsAsciiIgnoreCase(elementType, "StatusBar") {
            return true
        }
        if equalsAsciiIgnoreCase(source, "systemui")
            || equalsAsciiIgnoreCase(source, "com.android.systemui") {
            return true
        }
        if identifier.hasPrefix("com.android.systemui:id/")
            || equalsAsciiIgnoreCase(identifier, "statusBarBackground")
            || equalsAsciiIgnoreCase(identifier, "navigationBarBackground") {
            return true
        }
        return false
    }

    private func isClippingScrollContainer(_ elementType: XCUIElement.ElementType) -> Bool {
        switch elementType {
        case .scrollView, .table, .collectionView, .webView:
            return true
        default:
            return false
        }
    }

    private func appendSnapshotTree(
        _ snapshot: XCUIElementSnapshot,
        parentIndex: Int?,
        retainedParentOrdinal: UInt64?,
        depth: Int,
        source: String,
        clipBounds: CGRect,
        parentInVolatileChrome: Bool,
        nodes: inout [CapturedNode],
        truncated: inout Bool,
        hasher: inout FnvHasher,
        nextRetainedOrdinal: inout UInt64
    ) {
        if nodes.count >= Self.maxHierarchyNodes {
            truncated = true
            return
        }

        let currentIndex = nodes.count
        let elementType = describeElementType(snapshot.elementType)
        let identifier = snapshot.identifier
        let rect = sanitizeRect(snapshot.frame)
        let visibleIntersection = rect.intersection(clipBounds)
        let isOnScreen = rect.width > 0
            && rect.height > 0
            && !visibleIntersection.isNull
            && visibleIntersection.width > 0
            && visibleIntersection.height > 0
        let hittable = snapshot.isEnabled && isOnScreen

        let childClipBounds: CGRect
        if isClippingScrollContainer(snapshot.elementType) && rect.width > 0 && rect.height > 0 {
            childClipBounds = visibleIntersection.isNull ? .zero : visibleIntersection
        } else {
            childClipBounds = clipBounds
        }

        let inVolatileChrome = parentInVolatileChrome
            || isVolatileSystemChrome(source: source, elementType: elementType, identifier: identifier)

        let placeholder: String?
        if let rawPlaceholder = snapshot.placeholderValue, !rawPlaceholder.isEmpty {
            placeholder = rawPlaceholder
        } else {
            placeholder = nil
        }

        let captured = CapturedNode(
            index: currentIndex,
            parentIndex: parentIndex,
            depth: depth,
            source: source,
            elementType: elementType,
            identifier: identifier,
            label: snapshot.label,
            title: snapshot.title,
            value: stringifyValue(snapshot.value),
            placeholderValue: placeholder,
            enabled: snapshot.isEnabled,
            selected: snapshot.isSelected,
            hasFocus: snapshot.hasFocus,
            hittable: hittable,
            isOnScreen: isOnScreen,
            frame: rect,
            inVolatileChrome: inVolatileChrome
        )
        nodes.append(captured)

        let myRetainedOrdinal: UInt64?
        if !inVolatileChrome {
            let ordinal = nextRetainedOrdinal
            nextRetainedOrdinal &+= 1
            hasher.recordNode(captured, retainedParentOrdinal: retainedParentOrdinal)
            myRetainedOrdinal = ordinal
        } else {
            myRetainedOrdinal = nil
        }

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
                retainedParentOrdinal: myRetainedOrdinal,
                depth: depth + 1,
                source: source,
                clipBounds: childClipBounds,
                parentInVolatileChrome: inVolatileChrome,
                nodes: &nodes,
                truncated: &truncated,
                hasher: &hasher,
                nextRetainedOrdinal: &nextRetainedOrdinal
            )
        }
    }

    private func inspectSnapshotFocusAndKeyboard(
        _ snapshot: XCUIElementSnapshot,
        depth: Int = 0
    ) -> (hasFocus: Bool, keyboardHeight: Int) {
        var hasFocus = snapshot.hasFocus
        var keyboardHeight = 0
        if snapshot.elementType == .keyboard {
            let h = snapshot.frame.height
            if h.isFinite && h > 0 {
                keyboardHeight = Int(h.rounded())
            }
        }
        if depth >= Self.maxHierarchyDepth {
            return (hasFocus, keyboardHeight)
        }
        for child in snapshot.children {
            let sub = inspectSnapshotFocusAndKeyboard(child, depth: depth + 1)
            if sub.hasFocus { hasFocus = true }
            if sub.keyboardHeight > keyboardHeight { keyboardHeight = sub.keyboardHeight }
        }
        return (hasFocus, keyboardHeight)
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

    private func validateAppId(_ raw: String?) -> String? {
        guard let raw, !raw.isEmpty, raw.utf8.count <= Self.maxAppIdLength else {
            return nil
        }
        return raw
    }

    private enum OptionalStringParseResult {
        case success(String?)
        case failure(String)
    }

    private func parseOptionalSelectorField(_ raw: Any?, name: String, maxLen: Int) -> OptionalStringParseResult {
        guard let raw, !(raw is NSNull) else { return .success(nil) }
        guard let str = raw as? String else {
            return .failure("\(name) must be a string")
        }
        if str.isEmpty {
            return .success(nil)
        }
        if str.utf8.count > maxLen {
            return .failure("\(name) exceeds maximum length")
        }
        return .success(str)
    }

    private enum KnownHashParseResult {
        case success(String?)
        case failure(String)
    }

    private func parseOptionalKnownHash(_ raw: String?) -> KnownHashParseResult {
        guard let raw, !raw.isEmpty else { return .success(nil) }
        guard raw.count == 16 else {
            return .failure("knownHash must be a 16-character hexadecimal string")
        }
        for scalar in raw.unicodeScalars {
            let v = scalar.value
            let isHex = (v >= 48 && v <= 57) || (v >= 65 && v <= 70) || (v >= 97 && v <= 102)
            if !isHex {
                return .failure("knownHash must be a 16-character hexadecimal string")
            }
        }
        return .success(raw.lowercased())
    }

    private func elapsedMs(since started: Date) -> Int {
        max(0, Int((Date().timeIntervalSince(started) * 1000.0).rounded()))
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

    private func boolValue(_ raw: Any?) -> Bool? {
        guard let n = raw as? NSNumber else { return nil }
        guard CFGetTypeID(n) == CFBooleanGetTypeID() else { return nil }
        return n.boolValue
    }

    private func numberValue(_ raw: Any?) -> Double? {
        if let n = raw as? NSNumber {
            if CFGetTypeID(n) == CFBooleanGetTypeID() { return nil }
            return n.doubleValue
        }
        if let d = raw as? Double { return d }
        if let i = raw as? Int { return Double(i) }
        return nil
    }

    private func intValue(_ raw: Any?) -> Int? {
        if let n = raw as? NSNumber {
            if CFGetTypeID(n) == CFBooleanGetTypeID() { return nil }
            let d = n.doubleValue
            guard d.isFinite, d >= -9_007_199_254_740_992.0, d <= 9_007_199_254_740_992.0 else {
                return nil
            }
            return Int(d)
        }
        if let i = raw as? Int { return i }
        return nil
    }
}
