import Foundation

/// Liveness gate (SPEC §3a): the sender may transmit audio only while it holds
/// fresh authenticated evidence from the paired receiver. Pure state so tests
/// can drive it with a fake clock.
public struct Liveness: Sendable {
    public static let window: TimeInterval = 3
    /// Probe again before evidence expires so a healthy stream never gates.
    public static let refreshAfter: TimeInterval = 1.5
    public static let probeInterval: TimeInterval = 1

    public private(set) var lastProof: TimeInterval?
    public private(set) var sessionNonce: UInt64 = 0
    private var probes: [(nonce: UInt64, sentAt: TimeInterval)] = []
    private var lastProbeAt: TimeInterval = -.infinity
    private var probeSoon = false

    public init() {}

    public func isLive(at now: TimeInterval) -> Bool {
        guard let t = lastProof else { return false }
        return now - t < Self.window
    }

    public func shouldProbe(at now: TimeInterval) -> Bool {
        let stale = lastProof.map { now - $0 >= Self.refreshAfter } ?? true
        return (stale || probeSoon) && (now - lastProbeAt >= Self.probeInterval || probeSoon)
    }

    public mutating func probeSent(nonce: UInt64, at now: TimeInterval) {
        probes.append((nonce, now))
        probes.removeAll { now - $0.sentAt >= Self.window }
        lastProbeAt = now
        probeSoon = false
    }

    /// Authenticated hello. Only an echo of one of our recent probes proves the
    /// receiver is there now; an unsolicited one (echo 0, sent at receiver
    /// start-up) could be a replay, so it just prompts an immediate probe.
    @discardableResult
    public mutating func hello(sessionNonce nonce: UInt64, echo: UInt64, at now: TimeInterval) -> Bool {
        guard echo != 0, let i = probes.firstIndex(where: { $0.nonce == echo && now - $0.sentAt < Self.window })
        else {
            probeSoon = true
            return false
        }
        probes.remove(at: i)
        sessionNonce = nonce
        lastProof = now
        return true
    }

    /// Authenticated stats. Fresh if it echoes a sequence we sent within the
    /// window, under the session nonce we are using.
    @discardableResult
    public mutating func stats(echoSequence: UInt32, sessionNonce nonce: UInt64, lastSentSequence: UInt32,
                               packetsPerWindow: UInt32, at now: TimeInterval) -> Bool {
        guard lastProof != nil, nonce == sessionNonce else {
            probeSoon = true
            return false
        }
        let behind = lastSentSequence &- echoSequence
        guard behind <= packetsPerWindow else { return false }
        lastProof = now
        return true
    }

    /// Paired receiver changed or was forgotten.
    public mutating func reset() {
        self = Liveness()
    }
}
