import Foundation
import Network

struct HttpRequest {
    let method: String
    let path: String
    let queryItems: [String: String]
    let headers: [String: String]
    let body: Data
}

struct HttpResponse {
    let status: Int
    let body: Data

    static func json(_ status: Int, object: Any) -> HttpResponse {
        let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]))
            ?? Data("{\"error\":\"json serialization failed\"}".utf8)
        return HttpResponse(status: status, body: data)
    }

    static func jsonError(status: Int, message: String) -> HttpResponse {
        json(status, object: ["error": message])
    }

    var reasonPhrase: String {
        switch status {
        case 200: return "OK"
        case 400: return "Bad Request"
        case 401: return "Unauthorized"
        case 404: return "Not Found"
        case 405: return "Method Not Allowed"
        case 413: return "Payload Too Large"
        case 431: return "Request Header Fields Too Large"
        case 500: return "Internal Server Error"
        case 501: return "Not Implemented"
        case 503: return "Service Unavailable"
        default: return "Error"
        }
    }
}

final class HttpServer {
    static let maxHeaderBytes = 16_384
    static let maxBodyBytes = 1_048_576

    typealias RequestHandler = (HttpRequest, @escaping (HttpResponse) -> Void) -> Void

    private let requestedPort: UInt16
    private let authToken: String
    private let handler: RequestHandler
    private let queue = DispatchQueue(label: "dev.deja.ios.http", qos: .userInteractive)
    private var listener: NWListener?
    private var connections: [ObjectIdentifier: ConnectionHandler] = [:]

    init(port: UInt16, authToken: String, handler: @escaping RequestHandler) {
        self.requestedPort = port
        self.authToken = authToken
        self.handler = handler
    }

    func start(timeoutSeconds: TimeInterval) throws -> UInt16 {
        let params = NWParameters.tcp
        params.allowLocalEndpointReuse = true
        if let ipOptions = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ipOptions.version = .v4
        }
        let nwPort: NWEndpoint.Port = requestedPort == 0
            ? .any
            : (NWEndpoint.Port(rawValue: requestedPort) ?? .any)
        params.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: nwPort)

        let listener = try NWListener(using: params)
        self.listener = listener

        let readySemaphore = DispatchSemaphore(value: 0)
        var startError: Error?
        var boundPort: UInt16 = 0
        var resolved = false

        listener.stateUpdateHandler = { [weak listener] state in
            switch state {
            case .ready:
                if !resolved {
                    resolved = true
                    boundPort = listener?.port?.rawValue ?? 0
                    readySemaphore.signal()
                }
            case .failed(let error):
                if !resolved {
                    resolved = true
                    startError = error
                    readySemaphore.signal()
                }
            case .cancelled:
                if !resolved {
                    resolved = true
                    startError = NSError(
                        domain: "DejaHttpServer",
                        code: 1,
                        userInfo: [NSLocalizedDescriptionKey: "listener cancelled before ready"]
                    )
                    readySemaphore.signal()
                }
            default:
                break
            }
        }

        listener.newConnectionHandler = { [weak self] connection in
            self?.accept(connection)
        }

        listener.start(queue: queue)

        let waitResult = readySemaphore.wait(timeout: .now() + timeoutSeconds)
        if waitResult == .timedOut {
            listener.cancel()
            throw NSError(
                domain: "DejaHttpServer",
                code: 2,
                userInfo: [NSLocalizedDescriptionKey: "timed out waiting for HTTP listener to bind"]
            )
        }
        if let startError {
            listener.cancel()
            throw startError
        }
        if boundPort == 0 {
            listener.cancel()
            throw NSError(
                domain: "DejaHttpServer",
                code: 3,
                userInfo: [NSLocalizedDescriptionKey: "HTTP listener bound to port 0"]
            )
        }
        return boundPort
    }

    func stop() {
        queue.sync {
            listener?.cancel()
            listener = nil
            for conn in connections.values {
                conn.close()
            }
            connections.removeAll()
        }
    }

    private func accept(_ connection: NWConnection) {
        if case .hostPort(let host, _) = connection.endpoint {
            let isLoopback: Bool
            switch host {
            case .ipv4(let addr):
                isLoopback = addr == .loopback
            case .ipv6(let addr):
                isLoopback = addr == .loopback
            default:
                isLoopback = false
            }
            if !isLoopback {
                connection.cancel()
                return
            }
        }
        let connHandler = ConnectionHandler(
            connection: connection,
            queue: queue,
            authToken: authToken,
            handler: handler
        ) { [weak self] id in
            self?.connections.removeValue(forKey: id)
        }
        connections[ObjectIdentifier(connHandler)] = connHandler
        connHandler.start()
    }
}

private final class ConnectionHandler {
    private static let headerDelimiter = Data("\r\n\r\n".utf8)

    private let connection: NWConnection
    private let queue: DispatchQueue
    private let authToken: String
    private let handler: HttpServer.RequestHandler
    private let onClose: (ObjectIdentifier) -> Void

    private var buffer = Data()
    private var inFlight = false
    private var isClosed = false

    init(
        connection: NWConnection,
        queue: DispatchQueue,
        authToken: String,
        handler: @escaping HttpServer.RequestHandler,
        onClose: @escaping (ObjectIdentifier) -> Void
    ) {
        self.connection = connection
        self.queue = queue
        self.authToken = authToken
        self.handler = handler
        self.onClose = onClose
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                self?.receiveNext()
            case .failed, .cancelled:
                self?.finishClose()
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    func close() {
        guard !isClosed else { return }
        isClosed = true
        connection.cancel()
        onClose(ObjectIdentifier(self))
    }

    private func finishClose() {
        guard !isClosed else { return }
        isClosed = true
        onClose(ObjectIdentifier(self))
    }

    private func receiveNext() {
        guard !isClosed, !inFlight else { return }
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { [weak self] data, _, isComplete, error in
            guard let self, !self.isClosed else { return }
            if let data, !data.isEmpty {
                self.buffer.append(data)
            }
            if error != nil {
                self.close()
                return
            }
            self.processBuffer(eof: isComplete)
        }
    }

    private func processBuffer(eof: Bool) {
        guard !isClosed, !inFlight else { return }

        guard let delimiterRange = buffer.range(of: Self.headerDelimiter) else {
            if buffer.count > HttpServer.maxHeaderBytes {
                sendAndClose(HttpResponse.jsonError(status: 431, message: "request headers exceed \(HttpServer.maxHeaderBytes) bytes"))
                return
            }
            if eof {
                close()
            } else {
                receiveNext()
            }
            return
        }

        let startIdx = buffer.startIndex
        let headerByteCount = buffer.distance(from: startIdx, to: delimiterRange.lowerBound)
        if headerByteCount > HttpServer.maxHeaderBytes {
            sendAndClose(HttpResponse.jsonError(status: 431, message: "request headers exceed \(HttpServer.maxHeaderBytes) bytes"))
            return
        }

        let headerData = Data(buffer[startIdx..<delimiterRange.lowerBound])
        guard let headerString = String(data: headerData, encoding: .utf8) else {
            sendAndClose(HttpResponse.jsonError(status: 400, message: "malformed UTF-8 request headers"))
            return
        }

        let lines = headerString.components(separatedBy: "\r\n")
        guard let requestLine = lines.first, !requestLine.isEmpty else {
            sendAndClose(HttpResponse.jsonError(status: 400, message: "missing request line"))
            return
        }

        let parts = requestLine.split(separator: " ", omittingEmptySubsequences: false)
        guard parts.count == 3 else {
            sendAndClose(HttpResponse.jsonError(status: 400, message: "malformed HTTP request line"))
            return
        }

        let method = String(parts[0])
        let target = String(parts[1])
        let httpVersion = String(parts[2])
        guard httpVersion == "HTTP/1.1" || httpVersion == "HTTP/1.0" else {
            sendAndClose(HttpResponse.jsonError(status: 400, message: "unsupported HTTP version"))
            return
        }

        var headers: [String: String] = [:]
        for line in lines.dropFirst() {
            if line.isEmpty { continue }
            guard let colon = line.firstIndex(of: ":") else {
                sendAndClose(HttpResponse.jsonError(status: 400, message: "malformed header line"))
                return
            }
            let key = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
            let val = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            headers[key] = val
        }

        if headers["transfer-encoding"] != nil {
            sendAndClose(HttpResponse.jsonError(status: 501, message: "Transfer-Encoding is not supported"))
            return
        }

        let contentLength: Int
        if let rawLen = headers["content-length"] {
            guard let parsed = Int(rawLen), parsed >= 0 else {
                sendAndClose(HttpResponse.jsonError(status: 400, message: "invalid Content-Length"))
                return
            }
            contentLength = parsed
        } else {
            contentLength = 0
        }

        if contentLength > HttpServer.maxBodyBytes {
            sendAndClose(HttpResponse.jsonError(status: 413, message: "request body exceeds \(HttpServer.maxBodyBytes) bytes"))
            return
        }

        let bodyStart = delimiterRange.upperBound
        let availableBodyBytes = buffer.distance(from: bodyStart, to: buffer.endIndex)
        if availableBodyBytes < contentLength {
            if eof {
                close()
            } else {
                receiveNext()
            }
            return
        }

        let bodyEnd = buffer.index(bodyStart, offsetBy: contentLength)
        let body = contentLength > 0 ? Data(buffer[bodyStart..<bodyEnd]) : Data()
        buffer = Data(buffer[bodyEnd...])

        let connHeader = headers["connection"]?.lowercased()
        let keepAlive: Bool
        if httpVersion == "HTTP/1.1" {
            keepAlive = connHeader != "close"
        } else {
            keepAlive = connHeader == "keep-alive"
        }

        let providedToken = headers["x-deja-agent-token"] ?? ""
        if !Self.constantTimeEqual(providedToken, authToken) {
            sendResponse(
                HttpResponse.jsonError(status: 401, message: "unauthorized"),
                keepAlive: keepAlive,
                eof: eof
            )
            return
        }

        guard let components = URLComponents(string: "http://127.0.0.1" + target) else {
            sendAndClose(HttpResponse.jsonError(status: 400, message: "malformed request target"))
            return
        }

        var queryItems: [String: String] = [:]
        for item in components.queryItems ?? [] {
            queryItems[item.name] = item.value ?? ""
        }

        let request = HttpRequest(
            method: method,
            path: components.path,
            queryItems: queryItems,
            headers: headers,
            body: body
        )

        inFlight = true
        handler(request) { [weak self] response in
            guard let self else { return }
            self.queue.async {
                guard !self.isClosed else { return }
                self.inFlight = false
                self.sendResponse(response, keepAlive: keepAlive, eof: eof)
            }
        }
    }

    private func sendAndClose(_ response: HttpResponse) {
        sendResponse(response, keepAlive: false, eof: true)
    }

    private func sendResponse(_ response: HttpResponse, keepAlive: Bool, eof: Bool) {
        let shouldClose = !keepAlive || eof
        let connectionValue = shouldClose ? "close" : "keep-alive"
        var raw = "HTTP/1.1 \(response.status) \(response.reasonPhrase)\r\n"
        raw += "Content-Type: application/json; charset=utf-8\r\n"
        raw += "Content-Length: \(response.body.count)\r\n"
        raw += "Connection: \(connectionValue)\r\n"
        raw += "\r\n"

        var payload = Data(raw.utf8)
        payload.append(response.body)

        connection.send(content: payload, completion: .contentProcessed { [weak self] error in
            guard let self, !self.isClosed else { return }
            if error != nil || shouldClose {
                self.close()
                return
            }
            if !self.buffer.isEmpty {
                self.processBuffer(eof: false)
            } else {
                self.receiveNext()
            }
        })
    }

    private static func constantTimeEqual(_ a: String, _ b: String) -> Bool {
        let aBytes = Array(a.utf8)
        let bBytes = Array(b.utf8)
        var diff: UInt8 = aBytes.count == bBytes.count ? 0 : 1
        let maxLen = max(aBytes.count, bBytes.count)
        for i in 0..<maxLen {
            let ca = i < aBytes.count ? aBytes[i] : 0
            let cb = i < bBytes.count ? bBytes[i] : 0
            diff |= ca ^ cb
        }
        return diff == 0
    }
}
