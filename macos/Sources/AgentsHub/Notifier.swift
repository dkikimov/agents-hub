import AgentsHubCore
import UserNotifications

/// Banners for sessions you are not looking at.
///
/// The delegate is not optional: macOS withholds a banner while the posting app is
/// frontmost, and the case this whole feature exists for includes "app in front, but the
/// agent that rang is on another tab". `willPresent` is what overrides that.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    /// `UNUserNotificationCenter.current()` raises rather than returning nil when the
    /// process has no bundle identity, so a bare `.build/release/AgentsHub` would crash on
    /// launch instead of just going quiet. The Makefile only ever ships the .app bundle.
    private let center: UNUserNotificationCenter? =
        Bundle.main.bundleIdentifier == nil ? nil : .current()

    /// `onFailure` goes to the status line: a bell that reaches a denied notification
    /// centre is indistinguishable from one that never rang, and this feature's whole
    /// point is that you are not watching.
    func start(onFailure: @escaping @MainActor (String) -> Void) {
        guard let center else {
            onFailure("notifications need the .app bundle")
            return
        }
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { granted, error in
            Task { @MainActor in
                if let error {
                    onFailure("notifications: \(error.localizedDescription)")
                } else if !granted {
                    onFailure("notifications not allowed — System Settings ▸ Notifications")
                }
            }
        }
    }

    /// Keyed by session, so an agent that rings twice replaces its own banner instead of
    /// stacking two that say the same thing.
    func post(_ key: SessionKey, title: String, body: String) {
        guard let center else { return }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        center.add(UNNotificationRequest(
            identifier: "\(key.vm)/\(key.id)",
            content: content,
            trigger: nil
        ))
    }

    nonisolated func userNotificationCenter(
        _: UNUserNotificationCenter,
        willPresent _: UNNotification,
        withCompletionHandler completion: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completion([.banner, .sound])
    }
}
