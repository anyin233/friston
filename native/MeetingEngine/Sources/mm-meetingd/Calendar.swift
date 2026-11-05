import EngineKit
import EventKit
import Foundation

/// A calendar event surfaced in the schedule.
struct CalendarEvent: Encodable {
    var id: String
    var title: String
    var start: Date
    var end: Date
    var calendar: String
    var location: String?
    /// First meeting link found in URL / location / notes (Zoom, Teams, Meet, Tencent Meeting, Feishu …).
    var joinURL: String?
    var attendees: [String]
}

/// Reads events from the user's calendars via EventKit.
actor CalendarService {
    private let store = EKEventStore()

    /// Returns events between `from` and `to`, requesting full calendar access on first use.
    /// - Parameters:
    ///   - from: Range start.
    ///   - to: Range end.
    /// - Returns: Events sorted by start time.
    /// - Throws: `RPCError` -32011 when access is denied.
    func events(from: Date, to: Date) async throws -> [CalendarEvent] {
        let granted = try await store.requestFullAccessToEvents()
        guard granted else { throw RPCError(code: -32011, message: "calendar access denied; enable it in System Settings › Privacy & Security › Calendars") }
        let predicate = store.predicateForEvents(withStart: from, end: to, calendars: nil)
        return store.events(matching: predicate).map { e in
            CalendarEvent(
                id: e.calendarItemIdentifier, title: e.title ?? "(untitled)", start: e.startDate, end: e.endDate,
                calendar: e.calendar?.title ?? "", location: e.location,
                joinURL: Self.findJoinURL([e.url?.absoluteString, e.location, e.notes]),
                attendees: (e.attendees ?? []).compactMap { $0.name })
        }
    }

    /// Extracts the first known conferencing URL from free text.
    static func findJoinURL(_ fields: [String?]) -> String? {
        let hosts = ["zoom.us", "teams.microsoft.com", "meet.google.com", "meeting.tencent.com", "vc.feishu.cn", "webex.com"]
        let detector = try? NSDataDetector(types: NSTextCheckingResult.CheckingType.link.rawValue)
        for text in fields.compactMap({ $0 }) {
            let range = NSRange(text.startIndex..., in: text)
            for match in detector?.matches(in: text, range: range) ?? [] {
                if let url = match.url, let host = url.host, hosts.contains(where: { host.hasSuffix($0) }) {
                    return url.absoluteString
                }
            }
        }
        return nil
    }
}
