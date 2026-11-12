import Foundation

/// A receiver announcing pairing mode (SPEC §3a).
public struct DiscoveredReceiver: Identifiable, Hashable, Sendable {
    /// Receiver id and address: a second machine announcing the same id is
    /// listed separately instead of replacing the first.
    public var id: String { "\(Hex.encode(receiverID))@\(endpoint)" }
    public let receiverID: [UInt8]
    public let host: String
    /// Pairing endpoint (sender address + announced port).
    public let endpoint: Endpoint
    public let firstSeen: Date
    public var lastSeen: Date
    /// Another address announces the same receiver id: one of them is
    /// impersonating the other (pairing is then ambiguous; the UI says so).
    public var conflict = false
}

/// Listens for pairing-mode beacons on UDP 4811. Run it only while the user
/// is looking for a receiver: beacons exist only in pairing mode anyway, and
/// not listening keeps the app off the network otherwise.
public final class BeaconListener: @unchecked Sendable {
    private let found = Locked([String: DiscoveredReceiver]())
    private let running = Locked(true)
    private let socket: UDPSocket

    public init() throws {
        // Shared binding so the app and a CLI can both listen.
        socket = try UDPSocket(preferredPort: Wire.beaconPort, fallbackToEphemeral: false, shared: true)
        let t = Thread { [self] in loop() }
        t.name = "tightwire-beacons"
        t.start()
    }

    private func loop() {
        let s = socket
        while running.value {
            guard let (data, from) = s.receive(timeout: 0.25),
                  case .beacon(let b) = try? Wire.decode(data),
                  let ep = try? Endpoint.resolve("\(from.host):\(b.port)", defaultPort: b.port) else { continue }
            let now = Date()
            found.update { f in
                let r = DiscoveredReceiver(receiverID: b.receiverID, host: b.host, endpoint: ep, firstSeen: now, lastSeen: now)
                if f[r.id] != nil {
                    // Keep the first announcement's name; only refresh liveness.
                    f[r.id]?.lastSeen = now
                } else {
                    f = f.filter { now.timeIntervalSince($0.value.lastSeen) < 10 }
                    if f.count < Self.maxEntries { f[r.id] = r }
                }
            }
        }
    }

    /// Bound on what a flood of beacons can make us keep.
    public static let maxEntries = 32

    /// Receivers heard from within the last 3 s.
    public var receivers: [DiscoveredReceiver] { Self.visible(Array(found.value.values), now: Date()) }

    /// Recent entries, flagged when one receiver id comes from two places.
    public static func visible(_ all: [DiscoveredReceiver], now: Date) -> [DiscoveredReceiver] {
        let recent = all.filter { now.timeIntervalSince($0.lastSeen) < 3 }
        let counts = Dictionary(grouping: recent, by: \.receiverID).mapValues(\.count)
        return recent.map { r in
            var r = r
            r.conflict = (counts[r.receiverID] ?? 0) > 1
            return r
        }.sorted { ($0.host, $0.firstSeen) < ($1.host, $1.firstSeen) }
    }

    public func stop() {
        running.value = false
    }
}
