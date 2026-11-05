import EngineKit
import Foundation

// mm-embedd: newline-delimited JSON-RPC 2.0 over stdio. Logs go to stderr.

let server = RPCServer()
let service = EmbeddingService()

await server.register(Methods.initialize) { _ in
    .object([
        "name": .string("mm-embedd"),
        "version": .string("0.1.0"),
        "model": .string("EmbeddingGemma 2 (MLX)"),
        "dimensions": .array([768, 512, 256, 128].map { .number(Double($0)) }),
    ])
}

await server.register(Methods.Embed.modelLoad) { params in
    guard let path = params?["directory"]?.stringValue else { throw RPCError.invalidParams("model/load needs `directory`") }
    try await service.load(directory: URL(fileURLWithPath: path))
    return await service.status()
}

await server.register(Methods.Embed.modelUnload) { _ in
    await service.unload()
    return .null
}

await server.register(Methods.Embed.modelStatus) { _ in await service.status() }

await server.register(Methods.Embed.batch) { params in
    guard let params else { throw RPCError.invalidParams("embed/batch needs params") }
    let request = try params.decode(EmbedRequest.self)
    let vectors = try await service.embed(request)
    return .object(["vectors": .array(vectors.map { .array($0.map { .number(Double($0)) }) })])
}

await server.register(Methods.shutdown) { _ in
    // Exit only after every in-flight request (including this one) has replied.
    Task {
        try? await Task.sleep(for: .milliseconds(5))
        await server.waitUntilIdle()
        exit(0)
    }
    return .null
}

log("mm-embedd ready")
await server.runStdio()
