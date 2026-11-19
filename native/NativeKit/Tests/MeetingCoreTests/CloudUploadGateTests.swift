import Foundation
import Testing
@testable import MeetingCore

@Test func trailingSilenceHangoverIsNotUploaded() {
    var gate = CloudUploadGate()
    let activeStart = 16_000
    // Feed 1.5 s of voiced speech in 32 ms chunks from 16_000 to 40_000.
    for chunkEnd in stride(from: 16_512, through: 40_000, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.9, rms: 0.05)
    }

    // First streaming step confirms >= 350 ms and uploads pre-roll through lastVoiced + padding.
    let first = gate.nextStreamingStep(
        activeId: 1, activeStart: activeStart, processed: 40_000, audioBase: 0, heldFinalIds: []
    )
    #expect(first == .upload(id: 1, range: 14_400..<40_000, isFirstChunk: true))

    // Now simulate 500 ms (8,000 samples) of trailing VAD hangover silence while activeId stays 1.
    for chunkEnd in stride(from: 40_512, through: 48_000, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.05, rms: 0.0005)
    }
    let duringSilence = gate.nextStreamingStep(
        activeId: 1, activeStart: activeStart, processed: 48_000, audioBase: 0, heldFinalIds: []
    )
    // Trailing hangover silence (beyond trailingHoldPaddingSamples = 1,024 < minStreamChunkSamples = 3,200) is held back.
    #expect(duringSilence == .idle)

    // Utterance finalizes at silenceStart (~40_000), leaving zero trailing silence uploaded.
    let fin = gate.finalize(id: 1, start: activeStart, end: 40_000, audioBase: 0, maxRMS: 0.05)
    #expect(fin == .commit(tailRange: nil, totalUploadedSamples: 40_000 - 14_400, wasOpened: true))
}

@Test func briefMidSentencePauseResumesCleanly() {
    var gate = CloudUploadGate()
    for chunkEnd in stride(from: 512, through: 8_192, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.85, rms: 0.04)
    }
    let s1 = gate.nextStreamingStep(activeId: 1, activeStart: 0, processed: 8_192, audioBase: 0, heldFinalIds: [])
    #expect(s1 == .upload(id: 1, range: 0..<8_192, isFirstChunk: true))

    // 320 ms brief silence (< 500 ms hangover), still below minStreamChunkSamples past padding.
    for chunkEnd in stride(from: 8_704, through: 13_312, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.1, rms: 0.001)
    }
    let paused = gate.nextStreamingStep(activeId: 1, activeStart: 0, processed: 13_312, audioBase: 0, heldFinalIds: [])
    #expect(paused == .idle)

    // Speaker resumes -> contiguous slice including the brief pause uploads without gaps.
    for chunkEnd in stride(from: 13_824, through: 20_000, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.9, rms: 0.04)
    }
    let resumed = gate.nextStreamingStep(activeId: 1, activeStart: 0, processed: 20_000, audioBase: 0, heldFinalIds: [])
    #expect(resumed == .upload(id: 1, range: 8_192..<20_000, isFirstChunk: false))
}

@Test func shortNoiseBlipNeverUploadsAndOrphanedUtteranceAborts() {
    var gate = CloudUploadGate()
    // 250 ms blip (4,000 samples < 5,600 confirm threshold): never opens upload.
    for chunkEnd in stride(from: 512, through: 4_096, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.8, rms: 0.03)
    }
    let blip = gate.nextStreamingStep(activeId: 1, activeStart: 0, processed: 4_096, audioBase: 0, heldFinalIds: [])
    #expect(blip == .idle)

    // Now force-open an utterance #2, then drop activeId without finalize (orphan) vs held Final.
    for chunkEnd in stride(from: 10_512, through: 20_000, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.9, rms: 0.05)
    }
    let opened = gate.nextStreamingStep(activeId: 2, activeStart: 10_000, processed: 20_000, audioBase: 0, heldFinalIds: [])
    #expect(opened == .upload(id: 2, range: 8_400..<20_000, isFirstChunk: true))

    // While #2 waits in Sortformer `heldFinalIds`, it must NOT abort.
    let whileHeld = gate.nextStreamingStep(activeId: 3, activeStart: 20_500, processed: 21_000, audioBase: 0, heldFinalIds: [2])
    #expect(whileHeld == .idle)

    // If #2 vanished without `heldFinalIds`, abort fires so OpenAI input buffer can be cleared.
    let orphan = gate.nextStreamingStep(activeId: nil, activeStart: nil, processed: 21_000, audioBase: 0, heldFinalIds: [])
    #expect(orphan == .abort(id: 2))
}

@Test func subFloorRMSIsRejectedInStreamingAndFinalize() {
    var gate = CloudUploadGate()
    for chunkEnd in stride(from: 512, through: 16_000, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.95, rms: 0.0005)
    }
    let step = gate.nextStreamingStep(activeId: 7, activeStart: 0, processed: 16_000, audioBase: 0, heldFinalIds: [])
    #expect(step == .idle)

    let fin = gate.finalize(id: 7, start: 0, end: 16_000, audioBase: 0, maxRMS: 0.0005)
    #expect(fin == .skip)
}

@Test func microFramesCoalesceTo200MsAndReportExactMeteredSamples() {
    var gate = CloudUploadGate()
    for chunkEnd in stride(from: 512, through: 6_144, by: 512) {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.9, rms: 0.04)
    }
    let first = gate.nextStreamingStep(activeId: 10, activeStart: 0, processed: 6_144, audioBase: 0, heldFinalIds: [])
    #expect(first == .upload(id: 10, range: 0..<6_144, isFirstChunk: true))

    // Feed 50 ms (800 samples) -> held until >= 3,200 samples.
    for chunkEnd in [6_656, 7_168, 7_680] {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.9, rms: 0.04)
    }
    #expect(gate.nextStreamingStep(activeId: 10, activeStart: 0, processed: 7_680, audioBase: 0, heldFinalIds: []) == .idle)

    // Reach 3,200+ new samples -> coalesced upload fires.
    for chunkEnd in [8_192, 8_704, 9_216, 9_728] {
        gate.observeVADChunk(endSample: chunkEnd, speechProb: 0.9, rms: 0.04)
    }
    let second = gate.nextStreamingStep(activeId: 10, activeStart: 0, processed: 9_728, audioBase: 0, heldFinalIds: [])
    #expect(second == .upload(id: 10, range: 6_144..<9_728, isFirstChunk: false))

    // Finalize at 10,500 -> flushes only remaining 772 samples and totals 10,500.
    let fin = gate.finalize(id: 10, start: 0, end: 10_500, audioBase: 0, maxRMS: 0.04)
    #expect(fin == .commit(tailRange: 9_728..<10_500, totalUploadedSamples: 10_500, wasOpened: true))

    // NaN-safe RMS helper sanity check.
    #expect(CloudUploadGate.rms([]) == 0)
    #expect(CloudUploadGate.rms([0.5, -0.5, .nan]) > 0.49)
}

@Test func clearFrameAndAuthErrorDetectionWork() {
    #expect(OpenAIRealtimeCodec.audioClearJSON().contains("input_audio_buffer.clear"))
    #expect(CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: 401, message: "anything"))
    #expect(CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: 403, message: "forbidden"))
    #expect(CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: 429, message: "insufficient_quota"))
    #expect(!CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: 429, message: "rate limit retry"))
    #expect(CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: nil, message: "Incorrect API key provided"))
    #expect(CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: nil, message: "API_KEY_INVALID"))
    #expect(!CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: 500, message: "temporary server error"))
}
