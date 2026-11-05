/// Method and notification names. Keep in sync with `crates/mm-engine/src/methods.rs`
/// and `docs/engine-protocol.md`.
public enum Methods {
    public static let initialize = "initialize"
    public static let shutdown = "shutdown"

    public enum Meeting {
        public static let captureListSources = "capture/listSources"
        public static let captureStart = "capture/start"
        public static let captureStop = "capture/stop"
        public static let audioProbeInputs = "audio/probeInputs"
        public static let voiceprintEnroll = "voiceprint/enroll"
        public static let voiceprintFromClip = "voiceprint/fromClip"
        public static let transcribeFile = "transcribe/file"
        public static let calendarEvents = "calendar/events"

        public static let nCaptionPartial = "caption/partial"
        public static let nCaptionFinal = "caption/final"
        public static let nSpeakerRelabel = "speaker/relabel"
        public static let nSpatialCue = "spatial/cue"
        public static let nAudioLevels = "audio/levels"
        public static let nCaptureState = "capture/state"
    }

    public enum Embed {
        public static let batch = "embed/batch"
        public static let modelStatus = "model/status"
        public static let modelLoad = "model/load"
        public static let modelUnload = "model/unload"
    }
}
