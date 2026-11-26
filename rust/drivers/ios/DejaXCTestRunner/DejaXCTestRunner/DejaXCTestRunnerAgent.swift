import Foundation
import XCTest

final class DejaXCTestRunnerAgent: XCTestCase {
    private let issueLock = NSLock()
    private var capturedIssues: [String]?

    override func setUp() {
        super.setUp()
        continueAfterFailure = true
    }

    override func record(_ issue: XCTIssue) {
        issueLock.lock()
        let isCapturing = capturedIssues != nil
        if isCapturing {
            capturedIssues?.append(issue.compactDescription)
        }
        issueLock.unlock()

        if !isCapturing {
            super.record(issue)
        }
    }

    private func runCapturingIssues<T>(_ body: () throws -> T) -> (Result<T, Error>, [String]) {
        issueLock.lock()
        capturedIssues = []
        issueLock.unlock()

        let result: Result<T, Error>
        do {
            result = .success(try body())
        } catch {
            result = .failure(error)
        }

        issueLock.lock()
        let issues = capturedIssues ?? []
        capturedIssues = nil
        issueLock.unlock()

        return (result, issues)
    }

    func testRunAgent() {
        let env = ProcessInfo.processInfo.environment
        guard let token = env["DEJA_IOS_AGENT_TOKEN"], !token.isEmpty else {
            XCTFail("DEJA_IOS_AGENT_TOKEN environment variable is required")
            return
        }

        let requestedPort = UInt16(env["DEJA_IOS_AGENT_PORT"] ?? "") ?? 0
        let readyFilePath = env["DEJA_IOS_AGENT_READY_FILE"]

        let actions = XcuiActions()
        let shutdownExpectation = expectation(description: "deja-agent-shutdown")

        let server = HttpServer(port: requestedPort, authToken: token) { [weak self] request, respond in
            if request.method == "GET" && request.path == "/health" {
                respond(actions.handleHealth())
                return
            }

            DispatchQueue.main.async {
                guard let self else {
                    respond(HttpResponse.jsonError(status: 503, message: "agent shutting down"))
                    return
                }
                let (result, issues) = self.runCapturingIssues {
                    actions.handle(request)
                }
                switch result {
                case .success(let response):
                    if !issues.isEmpty && response.status < 400 {
                        respond(
                            HttpResponse.jsonError(
                                status: 500,
                                message: "XCTest recorded issue(s): \(issues.joined(separator: "; "))"
                            )
                        )
                    } else {
                        respond(response)
                    }
                case .failure(let error):
                    let detail = issues.isEmpty
                        ? error.localizedDescription
                        : "\(error.localizedDescription) (issues: \(issues.joined(separator: "; ")))"
                    respond(HttpResponse.jsonError(status: 500, message: detail))
                }
            }
        }

        let boundPort: UInt16
        do {
            boundPort = try server.start(timeoutSeconds: 10.0)
        } catch {
            XCTFail("Failed to start DejaXCTestRunner HTTP server: \(error.localizedDescription)")
            return
        }

        let pid = Int(getpid())
        if let readyFilePath, !readyFilePath.isEmpty {
            let readyPayload: [String: Any] = ["port": Int(boundPort), "pid": pid]
            if let data = try? JSONSerialization.data(withJSONObject: readyPayload, options: [.sortedKeys]) {
                let url = URL(fileURLWithPath: readyFilePath)
                try? data.write(to: url, options: .atomic)
            }
        }

        let sentinel = "DEJA_AGENT_READY {\"port\":\(boundPort)}\n"
        fputs(sentinel, stderr)
        fflush(stderr)

        wait(for: [shutdownExpectation], timeout: 86_400 * 7)
        server.stop()
    }
}
