import Foundation

/// Errors from ``StdoutGuard``.
public enum StdoutGuardError: Error, CustomStringConvertible {
    case dup(fd: Int32, errno: Int32)
    case redirect(from: Int32, to: Int32, errno: Int32)

    /// Human-readable cause including the file descriptors involved.
    public var description: String {
        switch self {
        case .dup(let fd, let e): "dup(\(fd)) failed: \(String(cString: strerror(e)))"
        case .redirect(let from, let to, let e): "dup2(\(from), \(to)) failed: \(String(cString: strerror(e)))"
        }
    }
}

/// Keeps stdout clean for JSON-RPC even when linked libraries `print` to it.
///
/// Model libraries (speech-swift, for instance) print loading progress on stdout, which would
/// corrupt the newline-delimited protocol. The guard duplicates the real stdout into a private
/// descriptor used only for protocol lines, then points descriptor 1 at stderr so stray output
/// ends up in the log instead.
public enum StdoutGuard {
    /// Reserves the current stdout for protocol traffic and redirects stray writes to stderr.
    /// - Parameters:
    ///   - stdoutFD: Descriptor to protect (1 in production; a pipe in tests).
    ///   - stderrFD: Descriptor stray writes should go to (2 in production).
    /// - Returns: A handle writing to the original stdout.
    /// - Throws: ``StdoutGuardError`` when the descriptors cannot be duplicated.
    public static func reserve(stdoutFD: Int32 = STDOUT_FILENO, stderrFD: Int32 = STDERR_FILENO) throws -> FileHandle {
        if stdoutFD == STDOUT_FILENO { fflush(stdout) }
        let saved = dup(stdoutFD)
        guard saved >= 0 else { throw StdoutGuardError.dup(fd: stdoutFD, errno: errno) }
        guard dup2(stderrFD, stdoutFD) >= 0 else {
            let e = errno
            close(saved)
            throw StdoutGuardError.redirect(from: stderrFD, to: stdoutFD, errno: e)
        }
        return FileHandle(fileDescriptor: saved, closeOnDealloc: true)
    }

    /// Builds an ``RPCServer`` sink that writes each line to `handle`.
    /// - Parameter handle: Usually the result of ``reserve(stdoutFD:stderrFD:)``.
    /// - Returns: A sink appending the newline delimiter.
    public static func sink(_ handle: FileHandle) -> @Sendable (Data) -> Void {
        // FileHandle is thread-safe for writes; the server serializes calls anyway.
        nonisolated(unsafe) let handle = handle
        return { data in handle.write(data + Data([0x0A])) }
    }
}
