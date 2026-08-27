// APNs registration and daemon synchronization. The APNs token is capability
// material: it is held only in memory, sent in an authenticated JSON body, and
// never logged or placed in a URL.
//
// Every registration carries a TAG — this install's local account id. The
// daemon stores it against the device row and stamps it onto the pushes it
// sends there, which is the only thing that tells the Notification Service
// Extension WHICH mailbox an arriving `event_id` belongs to: event ids are
// per-daemon SQLite ints and two accounts hand out the same ones (the same
// collision `Notifier` prefixes its request identifiers against). The tag is a
// locally-minted UUID that names nothing about the mailbox, so the blind relay
// and APNs both stay blind while carrying it.

import Foundation
import UIKit
import UserNotifications

@MainActor
final class PushRegistration {
    static let shared = PushRegistration()

    private let session = Sessions.ephemeral(
        timeout: 15, resource: 30,
        cachePolicy: .reloadIgnoringLocalCacheData, emptyHeaders: true)
    private var token: String?
    private var syncing = false
    private var needsAnotherSync = false

    private init() {}

    func start() {
        Task { await registerAndSync() }
    }

    func registerAndSync() async {
        let center = UNUserNotificationCenter.current()
        let settings = await center.notificationSettings()
        switch settings.authorizationStatus {
        case .notDetermined:
            guard (try? await center.requestAuthorization(options: [.alert, .sound])) == true else {
                return
            }
        case .authorized, .provisional, .ephemeral:
            break
        case .denied:
            return
        @unknown default:
            return
        }

        UIApplication.shared.registerForRemoteNotifications()
        if token != nil { await syncAllAccounts() }
    }

    func received(_ data: Data) async {
        token = data.map { String(format: "%02x", $0) }.joined()
        await syncAllAccounts()
    }

    func registrationFailed() {
        token = nil
    }

    /// Tell one account's daemon to stop pushing to this device. Called on the
    /// way out of `AccountManager.remove`, while the credentials that authorize
    /// it are still in the keychain.
    ///
    /// BEST EFFORT, and deliberately not parked for retry the way a refused
    /// keychain delete is: what a missed unregister leaves behind is noise (a
    /// banner for a mailbox this install has forgotten, which the extension
    /// can no longer fetch content for and so renders generic), not a live
    /// capability. The daemon also drops the row by itself once APNs reports
    /// the token gone.
    func unregister(_ settings: ConnectionSettings) async {
        guard let token else { return }
        await post("/client/devices/unregister", body: ["token": token], with: settings)
    }

    private func syncAllAccounts() async {
        guard let token else { return }
        if syncing {
            needsAnotherSync = true
            return
        }
        syncing = true
        defer { syncing = false }

        repeat {
            needsAnotherSync = false
            let ids = AccountManager.shared.accounts.map(\.id)
            for id in ids {
                guard let settings = await AccountManager.shared.settings(for: id) else { continue }
                await register(token: token, tag: id, with: settings)
            }
        } while needsAnotherSync
    }

    private func register(token: String, tag: UUID, with settings: ConnectionSettings) async {
        // Registration is idempotent and retried whenever the app foregrounds.
        await post(
            "/client/devices",
            body: [
                "token": token,
                "platform": "ios",
                "tag": tag.uuidString,
            ],
            with: settings)
    }

    /// POST one authenticated JSON body to the human door. Neither the daemon
    /// URL nor the capability token reaches a log, here or in an error.
    private func post(_ path: String, body: [String: String], with settings: ConnectionSettings)
        async
    {
        var base = settings.serverURL.trimmingCharacters(in: .whitespacesAndNewlines)
        while base.hasSuffix("/") { base.removeLast() }
        guard let url = URL(string: base + path),
            let encoded = try? JSONSerialization.data(withJSONObject: body)
        else { return }

        var request = URLRequest(url: url, timeoutInterval: 15)
        request.httpMethod = "POST"
        request.setValue("Bearer \(settings.apiToken)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = encoded
        _ = try? await session.data(for: request)
    }
}
