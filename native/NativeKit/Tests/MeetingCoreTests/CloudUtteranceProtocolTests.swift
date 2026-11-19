import Foundation
import Testing
@testable import MeetingCore

@Test func wavEncoderWritesValid16kMonoPCM16HeaderAndSamples() {
    let data = WAVEncoder.encode16kMonoPCM16([0.0, 2.0, -2.0, .nan])
    #expect(data.count == 44 + 8)
    #expect(String(data: data.subdata(in: 0..<4), encoding: .ascii) == "RIFF")
    #expect(String(data: data.subdata(in: 8..<12), encoding: .ascii) == "WAVE")
    #expect(String(data: data.subdata(in: 12..<16), encoding: .ascii) == "fmt ")
    #expect(String(data: data.subdata(in: 36..<40), encoding: .ascii) == "data")

    func u32LE(_ offset: Int) -> UInt32 {
        UInt32(littleEndian: data.withUnsafeBytes { $0.loadUnaligned(fromByteOffset: offset, as: UInt32.self) })
    }
    func i16LE(_ offset: Int) -> Int16 {
        Int16(littleEndian: data.withUnsafeBytes { $0.loadUnaligned(fromByteOffset: offset, as: Int16.self) })
    }
    #expect(u32LE(4) == 44) // 36 + 8 bytes PCM
    #expect(u32LE(24) == 16_000) // sample rate
    #expect(u32LE(40) == 8) // data chunk size
    #expect(i16LE(44) == 0)
    #expect(i16LE(46) == Int16.max)
    #expect(i16LE(48) == Int16.min)
    #expect(i16LE(50) == 0) // NaN -> 0
}

@Test func openAIUtteranceCodecBuildsMultipartAndParsesFormats() throws {
    let wav = WAVEncoder.encode16kMonoPCM16([0.1, -0.1])
    let endpoint = OpenAIUtteranceCodec.endpointURL(baseURL: "http://127.0.0.1:8080/")
    #expect(endpoint.absoluteString == "http://127.0.0.1:8080/v1/audio/transcriptions")

    let reqStandard = OpenAIUtteranceCodec.buildRequest(
        endpoint: endpoint, apiKey: " sk-test ", model: "gpt-transcribe",
        language: "zh-Hans-CN", wavData: wav, boundary: "B123"
    )
    #expect(reqStandard.value(forHTTPHeaderField: "Authorization") == "Bearer sk-test")
    #expect(reqStandard.value(forHTTPHeaderField: "Content-Type") == "multipart/form-data; boundary=B123")
    let bodyStr = String(decoding: reqStandard.httpBody ?? Data(), as: UTF8.self)
    #expect(bodyStr.contains("name=\"model\"\r\n\r\ngpt-transcribe"))
    #expect(bodyStr.contains("name=\"language\"\r\n\r\nzh"))
    #expect(bodyStr.contains("name=\"response_format\"\r\n\r\njson"))

    let reqWhisper = OpenAIUtteranceCodec.buildRequest(
        endpoint: endpoint, apiKey: "sk-test", model: "whisper-1",
        language: "auto", wavData: wav, boundary: "B456"
    )
    let whisperStr = String(decoding: reqWhisper.httpBody ?? Data(), as: UTF8.self)
    #expect(!whisperStr.contains("name=\"language\""))
    #expect(whisperStr.contains("name=\"response_format\"\r\n\r\nverbose_json"))

    let reqDiarize = OpenAIUtteranceCodec.buildRequest(
        endpoint: endpoint, apiKey: "sk-test", model: "gpt-4o-transcribe-diarize",
        language: nil, wavData: wav, boundary: "B789"
    )
    let diarizeStr = String(decoding: reqDiarize.httpBody ?? Data(), as: UTF8.self)
    #expect(diarizeStr.contains("name=\"response_format\"\r\n\r\ndiarized_json"))
    #expect(diarizeStr.contains("name=\"chunking_strategy\"\r\n\r\nauto"))

    let jsonResp = Data(#"{"text":" 今天讨论预算 ","usage":{"input_tokens":20,"output_tokens":7,"seconds":2.5}}"#.utf8)
    let parsedJson = try OpenAIUtteranceCodec.parseResponse(jsonResp)
    #expect(parsedJson == CloudUtteranceTranscript(text: "今天讨论预算", inputTokens: 20, outputTokens: 7, billedAudioMs: 2500))

    let diarizeResp = Data(#"{"segments":[{"text":"第一句"},{"text":"第二句"}],"duration":3.2}"#.utf8)
    let parsedSegs = try OpenAIUtteranceCodec.parseResponse(diarizeResp)
    #expect(parsedSegs == CloudUtteranceTranscript(text: "第一句 第二句", inputTokens: 0, outputTokens: 0, billedAudioMs: 3200))
}

@Test func geminiUtteranceCodecEncodesAndParsesGenerateContentAndInteractions() throws {
    #expect(GeminiUtteranceCodec.isDedicatedTranscribeModel("gemini-3.5-transcribe"))
    #expect(!GeminiUtteranceCodec.isDedicatedTranscribeModel("gemini-3.8-flash"))

    let wav = WAVEncoder.encode16kMonoPCM16([0.2, -0.2])
    let gcBody = GeminiUtteranceCodec.generateContentBody(wavData: wav, language: "zh")
    let gcObj = try JSONSerialization.jsonObject(with: gcBody) as! [String: Any]
    let contents = gcObj["contents"] as! [[String: Any]]
    let parts = contents[0]["parts"] as! [[String: Any]]
    #expect((parts[0]["inline_data"] as! [String: Any])["mime_type"] as? String == "audio/wav")

    let gcResp = Data(#"""
    {
      "candidates":[{"content":{"parts":[{"text":"```json\n{\"text\":\"下午三点评审\"}\n```"}]}}],
      "usageMetadata":{"promptTokenCount":90,"candidatesTokenCount":12}
    }
    """#.utf8)
    let gcParsed = try GeminiUtteranceCodec.parseGenerateContentResponse(gcResp)
    #expect(gcParsed == CloudUtteranceTranscript(text: "下午三点评审", inputTokens: 90, outputTokens: 12))

    let intBody = GeminiUtteranceCodec.interactionsBody(
        model: "gemini-3.5-transcribe", fileUri: "https://files/f1", mimeType: "audio/wav", language: "zh"
    )
    let intObj = try JSONSerialization.jsonObject(with: intBody) as! [String: Any]
    #expect(intObj["model"] as? String == "gemini-3.5-transcribe")
    let genCfg = intObj["generation_config"] as! [String: Any]
    let txCfg = genCfg["transcription_config"] as! [String: Any]
    #expect((txCfg["language_codes"] as? [String]) == ["zh-CN"])

    let intResp = Data(#"""
    {
      "steps":[{"content":[{"annotations":[{"type":"word_info","word":"你"},{"type":"word_info","word":"好"},{"type":"word_info","word":"。"}]}]}],
      "usage":{"total_input_tokens":44,"total_output_tokens":9}
    }
    """#.utf8)
    let intParsed = try GeminiUtteranceCodec.parseInteractionsResponse(intResp)
    #expect(intParsed == CloudUtteranceTranscript(text: "你好。", inputTokens: 44, outputTokens: 9))
}
