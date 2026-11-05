import Foundation
import Synchronization
import Testing
@testable import EngineKit

/// Collects lines written by the server.
final class LineSink: Sendable {
    private let lines = Mutex<[String]>([])
    var all: [String] { lines.withLock { $0 } }
    var write: @Sendable (Data) -> Void {
        { data in self.lines.withLock { $0.append(String(decoding: data, as: UTF8.self)) } }
    }
}

@Test func echoesResultWithSameId() async throws {
    let sink = LineSink()
    let server = RPCServer(sink: sink.write)
    await server.register("echo") { params in params ?? .null }
    await server.handle(line: #"{"jsonrpc":"2.0","id":7,"method":"echo","params":{"x":1}}"#)
    let reply = try JSONDecoder().decode(JSONValue.self, from: Data(sink.all[0].utf8))
    #expect(reply["id"] == .number(7))
    #expect(reply["result"]?["x"] == .number(1))
}

@Test func unknownMethodAndHandlerErrorsAreReported() async throws {
    let sink = LineSink()
    let server = RPCServer(sink: sink.write)
    await server.register("todo") { _ in throw RPCError.notImplemented("todo") }
    await server.handle(line: #"{"id":1,"method":"nope"}"#)
    await server.handle(line: #"{"id":2,"method":"todo"}"#)
    let replies = try sink.all.map { try JSONDecoder().decode(JSONValue.self, from: Data($0.utf8)) }
    #expect(replies[0]["error"]?["code"] == .number(-32601))
    #expect(replies[1]["error"]?["code"] == .number(-32004))
}

@Test func notificationsHaveNoId() async throws {
    let sink = LineSink()
    let server = RPCServer(sink: sink.write)
    await server.notify("caption/partial", .object(["text": .string("大家好")]))
    let n = try JSONDecoder().decode(JSONValue.self, from: Data(sink.all[0].utf8))
    #expect(n["id"] == nil)
    #expect(n["method"] == .string("caption/partial"))
    #expect(sink.all[0].contains("大家好"), "non-ASCII must not be escaped")
}

@Test func waitUntilIdleCoversSlowRequests() async throws {
    let sink = LineSink()
    let server = RPCServer(sink: sink.write)
    await server.register("slow") { _ in
        try await Task.sleep(for: .milliseconds(80))
        return .bool(true)
    }
    let pending = Task { await server.handle(line: #"{"id":9,"method":"slow"}"#) }
    try await Task.sleep(for: .milliseconds(10))
    await server.waitUntilIdle()
    #expect(sink.all.count == 1, "the slow reply must be written before idle is reported")
    await pending.value
}

@Test func decodeErrorsNameTheField() async throws {
    struct P: Decodable { var items: [String] }
    let sink = LineSink()
    let server = RPCServer(sink: sink.write)
    await server.register("typed") { params in
        _ = try (params ?? .null).decode(P.self)
        return .null
    }
    await server.handle(line: #"{"id":1,"method":"typed","params":{"items":"bad"}}"#)
    #expect(sink.all[0].contains("items"))
    #expect(sink.all[0].contains("-32602"))
}
