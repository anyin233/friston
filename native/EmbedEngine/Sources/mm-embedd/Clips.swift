import AVFoundation
import EngineKit
import Foundation

/// Cuts time windows out of long recordings so the indexer can embed a 30 s audio or
/// 20 s video window of a whole meeting without writing clip files ahead of time.
enum Clips {
    /// Where temporary clips go; cleaned up by the caller after embedding.
    static let directory: URL = FileManager.default.temporaryDirectory.appendingPathComponent("mm-embedd-clips", isDirectory: true)

    /// Copies `[startMs, endMs)` of an audio file into a temporary CAF file.
    /// - Parameters:
    ///   - source: Recording (CAF/WAV/M4A, any channel count).
    ///   - startMs: Window start.
    ///   - endMs: Window end; clamped to the file length.
    /// - Returns: The clip URL.
    /// - Throws: `RPCError.invalidParams` naming the file when it cannot be read or the window is empty.
    static func audio(_ source: URL, startMs: Int, endMs: Int) throws -> URL {
        let input: AVAudioFile
        do { input = try AVAudioFile(forReading: source) } catch {
            throw RPCError.invalidParams("cannot open audio \(source.path): \(error.localizedDescription)")
        }
        let format = input.processingFormat
        let rate = format.sampleRate
        let start = AVAudioFramePosition(Double(startMs) / 1000 * rate)
        let end = min(input.length, AVAudioFramePosition(Double(endMs) / 1000 * rate))
        guard start < end else {
            throw RPCError.invalidParams("empty audio window \(startMs)–\(endMs) ms in \(source.path) (length \(input.length) frames)")
        }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let out = directory.appendingPathComponent("\(UUID().uuidString).caf")
        let output = try AVAudioFile(forWriting: out, settings: format.settings, commonFormat: format.commonFormat, interleaved: format.isInterleaved)
        input.framePosition = start
        // 1 s chunks keep memory flat for long windows.
        let chunk = AVAudioFrameCount(rate)
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: chunk) else {
            throw RPCError(code: -32000, message: "cannot allocate audio buffer for \(source.path)")
        }
        var remaining = end - start
        while remaining > 0 {
            let n = AVAudioFrameCount(min(Int64(chunk), remaining))
            try input.read(into: buffer, frameCount: n)
            if buffer.frameLength == 0 { break }
            try output.write(from: buffer)
            remaining -= AVAudioFramePosition(buffer.frameLength)
        }
        return out
    }

    /// Exports `[startMs, endMs)` of a movie into a temporary file without re-encoding.
    /// - Parameters:
    ///   - source: Recording (`screen.mov`).
    ///   - startMs: Window start.
    ///   - endMs: Window end.
    /// - Returns: The clip URL.
    /// - Throws: `RPCError` naming the file when export fails.
    static func video(_ source: URL, startMs: Int, endMs: Int) async throws -> URL {
        let asset = AVURLAsset(url: source)
        guard let export = AVAssetExportSession(asset: asset, presetName: AVAssetExportPresetPassthrough) else {
            throw RPCError.invalidParams("cannot export video from \(source.path)")
        }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let out = directory.appendingPathComponent("\(UUID().uuidString).mov")
        export.timeRange = CMTimeRange(
            start: CMTime(value: CMTimeValue(startMs), timescale: 1000),
            end: CMTime(value: CMTimeValue(endMs), timescale: 1000))
        do {
            try await export.export(to: out, as: .mov)
        } catch {
            throw RPCError(code: -32000, message: "exporting \(startMs)–\(endMs) ms of \(source.path): \(error.localizedDescription)")
        }
        return out
    }

    /// Deletes temporary clips.
    /// - Parameter urls: Files returned by `audio` / `video`.
    static func remove(_ urls: [URL]) {
        for url in urls { try? FileManager.default.removeItem(at: url) }
    }
}
