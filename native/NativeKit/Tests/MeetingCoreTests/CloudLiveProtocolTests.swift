import Foundation
import Testing
@testable import MeetingCore

@Test func pcm16EncoderClampsAndEncodesLittleEndian() {
    let b64 = PCM16Encoder.encodeBase64([0.0, 2.0, -2.0])
    let data = Data(base64Encoded: b64)!
    #expect(data.count == 6)
    let s0 = Int16(littleEndian: data.withUnsafeBytes { $0.load(fromByteOffset: 0, as: Int16.self) })
    let s1 = Int16(littleEndian: data.withUnsafeBytes { $0.load(fromByteOffset: 2, as: Int16.self) })
    let s2 = Int16(littleEndian: data.withUnsafeBytes { $0.load(fromByteOffset: 4, as: Int16.self) })
    #expect(s0 == 0)
    #expect(s1 == Int16.max)
    #expect(s2 == Int16.min)
}

@Test func openAIRealtimeCodecEncodesRequestsAndParsesEvents() throws {
    #expect(OpenAIRealtimeCodec.endpointURL().absoluteString == "wss://api.openai.com/v1/realtime?intent=transcription")
    #expect(OpenAIRealtimeCodec.headers(apiKey: " sk-1 ")["Authorization"] == "Bearer sk-1")

    let setup = OpenAIRealtimeCodec.sessionUpdateJSON(model: "gpt-live-transcribe", language: "zh-Hans")
    let setupObj = try JSONSerialization.jsonObject(with: Data(setup.utf8)) as! [String: Any]
    #expect(setupObj["type"] as? String == "session.update")
    let session = setupObj["session"] as! [String: Any]
    #expect(session["type"] as? String == "transcription")
    let audio = session["audio"] as! [String: Any]
    let input = audio["input"] as! [String: Any]
    #expect(input["turn_detection"] is NSNull)
    let format = input["format"] as! [String: Any]
    #expect(format["rate"] as? Int == 24_000)
    let tx = input["transcription"] as! [String: Any]
    #expect(tx["model"] as? String == "gpt-live-transcribe")
    #expect(tx["language"] as? String == "zh")

    let append = OpenAIRealtimeCodec.audioAppendJSON(base64PCM24k: "AAAA")
    #expect(append.contains("input_audio_buffer.append") && append.contains("AAAA"))
    #expect(OpenAIRealtimeCodec.audioCommitJSON().contains("input_audio_buffer.commit"))

    #expect(OpenAIRealtimeCodec.parseServerEvent(#"{"type":"session.created"}"#) == nil)
    #expect(OpenAIRealtimeCodec.parseServerEvent(#"{"type":"session.updated"}"#) == .ready)
    #expect(OpenAIRealtimeCodec.parseServerEvent(#"{"type":"conversation.item.input_audio_transcription.delta","delta":"你好"}"#) == .partialDelta("你好"))
    #expect(OpenAIRealtimeCodec.parseServerEvent(
        #"{"type":"conversation.item.input_audio_transcription.completed","transcript":" 你好世界 ","usage":{"input_tokens":150,"output_tokens":12}}"#
    ) == .finalTranscript(text: "你好世界", inputTokens: 150, outputTokens: 12))
    #expect(OpenAIRealtimeCodec.parseServerEvent(#"{"type":"error","error":{"message":"bad key"}}"#) == .error("bad key"))
}

@Test func geminiLiveCodecEncodesRequestsAndParsesEvents() throws {
    let url = GeminiLiveCodec.endpointURL(apiKey: "AIza-key")!
    #expect(url.absoluteString.contains("BidiGenerateContent?key=AIza-key"))

    let setup = GeminiLiveCodec.setupJSON(model: "gemini-3.5-transcribe-live")
    let obj = try JSONSerialization.jsonObject(with: Data(setup.utf8)) as! [String: Any]
    let inner = obj["setup"] as! [String: Any]
    #expect(inner["model"] as? String == "models/gemini-3.5-transcribe-live")

    #expect(GeminiLiveCodec.activityStartJSON().contains("activityStart"))
    #expect(GeminiLiveCodec.audioChunkJSON(base64PCM16k: "BBBB").contains("audio/pcm;rate=16000"))
    #expect(GeminiLiveCodec.activityEndJSON().contains("activityEnd"))

    #expect(GeminiLiveCodec.parseServerEvent(#"{"setupComplete":{}}"#) == .ready)
    #expect(GeminiLiveCodec.parseServerEvent(
        #"{"serverContent":{"interimInputTranscription":{"text":" 今天 "}}}"#
    ) == .partialReplace("今天"))
    #expect(GeminiLiveCodec.parseServerEvent(
        #"{"serverContent":{"inputTranscription":{"text":"今天开会"}},"usageMetadata":{"promptTokenCount":240,"candidatesTokenCount":18}}"#
    ) == .finalTranscript(text: "今天开会", inputTokens: 240, outputTokens: 18))
    #expect(GeminiLiveCodec.parseServerEvent(
        #"{"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":4}}"#
    ) == .usage(inputTokens: 50, outputTokens: 4))
    #expect(GeminiLiveCodec.parseServerEvent(#"{"error":{"message":"quota exceeded"}}"#) == .error("quota exceeded"))
}
