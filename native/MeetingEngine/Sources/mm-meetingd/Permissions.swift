import AVFoundation
import CoreGraphics
import EngineKit
import EventKit
import Foundation

/// Privacy permissions the engine needs, as seen from this process. TCC attributes the
/// engine to the app that launched it, so this reports the app's grants.
struct PermissionStatus: Encodable {
    /// Screen & System Audio Recording (ScreenCaptureKit window/display and system audio).
    var screenCapture: String
    var microphone: String
    var calendar: String
}

enum Permissions {
    /// Current state of every permission, without prompting.
    /// - Returns: `granted` / `denied` / `notDetermined` / `restricted` per permission.
    static func status() -> PermissionStatus {
        PermissionStatus(
            // CoreGraphics only says yes or no: "not asked yet" also reads as denied.
            screenCapture: CGPreflightScreenCaptureAccess() ? "granted" : "denied",
            microphone: name(AVCaptureDevice.authorizationStatus(for: .audio)),
            calendar: name(EKEventStore.authorizationStatus(for: .event)))
    }

    /// Asks macOS for one permission (shows the system prompt when it has not been asked yet;
    /// for screen recording it also adds the app to the System Settings list).
    /// - Parameter kind: `screenCapture`, `microphone` or `calendar`.
    /// - Returns: The status after the request.
    /// - Throws: `RPCError.invalidParams` naming an unknown kind.
    static func request(_ kind: String) async throws -> PermissionStatus {
        switch kind {
        case "screenCapture":
            _ = CGRequestScreenCaptureAccess()
        case "microphone":
            _ = await AVCaptureDevice.requestAccess(for: .audio)
        case "calendar":
            _ = try? await EKEventStore().requestFullAccessToEvents()
        default:
            throw RPCError.invalidParams("permissions/request: unknown kind \(kind); use screenCapture, microphone or calendar")
        }
        return status()
    }

    private static func name(_ s: AVAuthorizationStatus) -> String {
        switch s {
        case .authorized: "granted"
        case .denied: "denied"
        case .restricted: "restricted"
        case .notDetermined: "notDetermined"
        @unknown default: "denied"
        }
    }

    private static func name(_ s: EKAuthorizationStatus) -> String {
        switch s {
        case .fullAccess, .authorized: "granted"
        case .writeOnly, .denied: "denied"
        case .restricted: "restricted"
        case .notDetermined: "notDetermined"
        @unknown default: "denied"
        }
    }
}
