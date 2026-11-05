import EngineKit
import Foundation
import ScreenCaptureKit

/// A window the user can choose for window-level recording.
struct CaptureWindow: Encodable {
    var windowID: UInt32
    var title: String
    var appName: String
    var bundleID: String
    var width: Double
    var height: Double
}

/// Shareable windows and displays, as reported by ScreenCaptureKit.
struct CaptureSources: Encodable {
    var windows: [CaptureWindow]
    var displays: [UInt32]
}

/// Lists on-screen windows (filtered to titled, reasonably sized ones) and displays.
/// - Returns: Sources for the capture picker.
/// - Throws: `RPCError` with code -32010 when Screen Recording permission is missing.
func listCaptureSources() async throws -> CaptureSources {
    do {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        let windows = content.windows
            .filter { ($0.title?.isEmpty == false) && $0.frame.width >= 200 && $0.frame.height >= 150 }
            .map {
                CaptureWindow(
                    windowID: $0.windowID, title: $0.title ?? "",
                    appName: $0.owningApplication?.applicationName ?? "",
                    bundleID: $0.owningApplication?.bundleIdentifier ?? "",
                    width: $0.frame.width, height: $0.frame.height)
            }
        return CaptureSources(windows: windows, displays: content.displays.map(\.displayID))
    } catch {
        throw RPCError(code: -32010, message: "ScreenCaptureKit unavailable (Screen Recording permission?): \(error.localizedDescription)")
    }
}
