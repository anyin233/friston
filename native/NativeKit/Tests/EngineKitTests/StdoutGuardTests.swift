import Foundation
import Testing
@testable import EngineKit

/// Reads everything currently buffered in a pipe without blocking on EOF.
private func drain(_ pipe: Pipe) -> String {
    let fd = pipe.fileHandleForReading.fileDescriptor
    let flags = fcntl(fd, F_GETFL)
    _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK)
    var bytes = [UInt8](repeating: 0, count: 4096)
    let n = read(fd, &bytes, bytes.count)
    return n > 0 ? String(decoding: bytes[0..<n], as: UTF8.self) : ""
}

@Test func strayWritesGoToTheLogWhileProtocolKeepsTheOriginalOutput() throws {
    let output = Pipe(), logs = Pipe()
    // Stand-ins for fd 1 (protocol) and fd 2 (logs), so the test runner's own stdout is untouched.
    let fakeStdout = dup(output.fileHandleForWriting.fileDescriptor)
    let fakeStderr = logs.fileHandleForWriting.fileDescriptor
    defer { close(fakeStdout) }

    let protocolHandle = try StdoutGuard.reserve(stdoutFD: fakeStdout, stderrFD: fakeStderr)
    // A library `print` writes to "fd 1", which now lands in the log pipe.
    _ = "stray\n".withCString { write(fakeStdout, $0, 6) }
    protocolHandle.write(Data("{\"jsonrpc\":\"2.0\"}\n".utf8))

    #expect(drain(logs) == "stray\n")
    #expect(drain(output) == "{\"jsonrpc\":\"2.0\"}\n")
}
