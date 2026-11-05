import Foundation

/// A JSON-RPC error returned to the client.
public struct RPCError: Error, Codable, Sendable, Equatable {
    public var code: Int
    public var message: String

    /// Creates an error.
    /// - Parameters:
    ///   - code: JSON-RPC error code (-32601 method not found, -32602 invalid params, -32004 not implemented, …).
    ///   - message: Human-readable explanation with context.
    public init(code: Int, message: String) {
        self.code = code
        self.message = message
    }

    public static func methodNotFound(_ method: String) -> RPCError {
        RPCError(code: -32601, message: "method not found: \(method)")
    }

    public static func invalidParams(_ detail: String) -> RPCError {
        RPCError(code: -32602, message: "invalid params: \(detail)")
    }

    public static func notImplemented(_ what: String) -> RPCError {
        RPCError(code: -32004, message: "not implemented yet: \(what)")
    }
}

private struct IncomingRequest: Decodable {
    var id: Int64?
    var method: String
    var params: JSONValue?
}

private struct OutgoingResponse: Encodable {
    var jsonrpc = "2.0"
    var id: Int64
    var result: JSONValue?
    var error: RPCError?
}

private struct OutgoingNotification: Encodable {
    var jsonrpc = "2.0"
    var method: String
    var params: JSONValue
}

/// Newline-delimited JSON-RPC 2.0 server.
///
/// Requests are handled concurrently (a slow `transcribe/file` must not block
/// `audio/levels`), while all writes are serialized by the actor so lines never interleave.
public actor RPCServer {
    public typealias Handler = @Sendable (JSONValue?) async throws -> JSONValue

    private var handlers: [String: Handler] = [:]
    /// Requests currently being handled; `shutdown` waits for this to reach zero.
    private var inFlight = 0
    private let sink: @Sendable (Data) -> Void
    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.withoutEscapingSlashes, .sortedKeys]
        return e
    }()

    /// Creates a server.
    /// - Parameter sink: Receives each encoded line (without the trailing newline). Defaults to stdout.
    public init(sink: @escaping @Sendable (Data) -> Void = RPCServer.stdout) {
        self.sink = sink
    }

    /// Writes one line to stdout; stdout is reserved for protocol traffic, logs go to stderr.
    public static let stdout: @Sendable (Data) -> Void = { data in
        FileHandle.standardOutput.write(data + Data([0x0A]))
    }

    /// Registers a method handler, replacing any existing one.
    /// - Parameters:
    ///   - method: JSON-RPC method name.
    ///   - handler: Async handler returning the result value.
    public func register(_ method: String, _ handler: @escaping Handler) {
        handlers[method] = handler
    }

    /// Sends a notification (no reply expected).
    /// - Parameters:
    ///   - method: Notification name, e.g. `caption/partial`.
    ///   - params: Payload.
    public func notify(_ method: String, _ params: JSONValue) {
        emit(OutgoingNotification(method: method, params: params))
    }

    /// Parses and answers one request line.
    /// - Parameter line: A single JSON object.
    public func handle(line: String) async {
        inFlight += 1
        defer { inFlight -= 1 }
        await process(line: line)
    }

    /// Decodes, dispatches and replies; callers account for `inFlight`.
    private func process(line: String) async {
        let request: IncomingRequest
        do {
            request = try JSONDecoder().decode(IncomingRequest.self, from: Data(line.utf8))
        } catch {
            log("dropping malformed request \(line.prefix(200)): \(error)")
            return
        }
        guard let handler = handlers[request.method] else {
            if let id = request.id { emit(OutgoingResponse(id: id, error: .methodNotFound(request.method))) }
            return
        }
        do {
            let result = try await handler(request.params)
            if let id = request.id { emit(OutgoingResponse(id: id, result: result)) }
        } catch let error as RPCError {
            if let id = request.id { emit(OutgoingResponse(id: id, error: error)) }
        } catch let error as DecodingError {
            // DecodingError's localizedDescription hides the failing field; the debug form names it.
            if let id = request.id { emit(OutgoingResponse(id: id, error: .invalidParams("\(request.method): \(error)"))) }
        } catch {
            let wrapped = RPCError(code: -32000, message: "\(request.method) failed: \(error.localizedDescription)")
            if let id = request.id { emit(OutgoingResponse(id: id, error: wrapped)) }
        }
    }

    private func finishRequest() {
        inFlight -= 1
    }

    /// Suspends until no request is being handled (used before exiting on `shutdown`).
    public func waitUntilIdle() async {
        while inFlight > 0 {
            try? await Task.sleep(for: .milliseconds(5))
        }
    }

    /// Serves requests read from stdin until EOF.
    public func runStdio() async {
        do {
            for try await line in FileHandle.standardInput.bytes.lines where !line.isEmpty {
                // Count the request before its task starts, so `waitUntilIdle` cannot miss it.
                inFlight += 1
                Task {
                    await self.process(line: line)
                    await self.finishRequest()
                }
            }
        } catch {
            log("stdin closed with error: \(error)")
        }
    }

    private func emit<T: Encodable>(_ message: T) {
        do {
            sink(try encoder.encode(message))
        } catch {
            log("failed to encode outgoing message: \(error)")
        }
    }
}

/// Writes a diagnostic line to stderr (never stdout, which carries protocol traffic).
/// - Parameter message: The text to log.
public func log(_ message: String) {
    FileHandle.standardError.write(Data("[engine] \(message)\n".utf8))
}
