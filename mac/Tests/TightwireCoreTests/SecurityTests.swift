import Foundation
import Testing
@testable import TightwireCore

/// Regression tests for the 0.2.0 security fixes (see SPEC §3a "Security").
struct SecurityTests {
    static let hostile = [
        "PC\ncode: 111 111",
        "PC\r\u{1b}[2A",
        "\u{202e}111 111 :edoc",
        "GAMING\u{200b}PC",
        "a\u{2066}b\u{2069}",
        "line\u{2028}break",
        String(repeating: "M", count: 41),
    ]

    @Test(arguments: hostile)
    func hostilePeerNamesAreRejectedAtDecode(name: String) {
        // Beacon with that host name (unauthenticated type).
        var b: [UInt8] = Wire.magic + [Wire.version, 3, 0, 0]
        b += [0xca, 0x12] + [UInt8](repeating: 1, count: 16)
        let raw = Array(name.utf8.prefix(255))
        b += [UInt8(raw.count)] + raw
        #expect(throws: Wire.DecodeError.field) { try Wire.decode(b) }
    }

    @Test(arguments: hostile)
    func sanitisedTextHasNoControlOrFormatCharacters(name: String) {
        let s = Wire.sanitize(name, max: Wire.maxPeerNameCharacters)
        #expect(!s.unicodeScalars.contains(where: Wire.isUnsafe))
        #expect(s.count <= Wire.maxPeerNameCharacters)
        #expect(!s.contains("\n"))
        #expect(Wire.validPeerName(Wire.outgoingName(name)))
    }

    @Test func ordinaryNamesStillPair() {
        for n in ["GAMING-PC", "Stúdio-PC ✓", "Alex’s MacBook Pro", ""] { #expect(Wire.validPeerName(n)) }
    }

    @Test func busIDsAreStrict() {
        for ok in ["1-1", "1-4.2", "12-3.1.4"] { #expect(Wire.validBusID(ok)) }
        for bad in ["", "1", "1-", "-1", "1-a", "1 1", "1-1;calc", "1-1.", "1--1", "a-1"] { #expect(!Wire.validBusID(bad)) }
    }

    @Test func constantTimeCompare() {
        #expect(Wire.constantTimeEqual([1, 2, 3], [1, 2, 3]))
        #expect(!Wire.constantTimeEqual([1, 2, 3], [1, 2, 4]))
        #expect(!Wire.constantTimeEqual([1, 2], [1, 2, 3]))
    }

    /// A second machine announcing the same receiver id is shown next to the
    /// first (flagged), not silently swapped in for it.
    @Test func beaconWithReusedIDIsFlaggedNotReplacing() throws {
        let id = [UInt8](repeating: 7, count: 16)
        let t0 = Date()
        let real = DiscoveredReceiver(receiverID: id, host: "GAMING-PC", endpoint: try Endpoint.resolve("192.168.50.20:4814", defaultPort: 4814),
                                      firstSeen: t0, lastSeen: t0)
        let fake = DiscoveredReceiver(receiverID: id, host: "GAMING-PC", endpoint: try Endpoint.resolve("192.168.50.66:4814", defaultPort: 4814),
                                      firstSeen: t0.addingTimeInterval(1), lastSeen: t0.addingTimeInterval(1))
        #expect(real.id != fake.id)
        let v = BeaconListener.visible([real, fake], now: t0.addingTimeInterval(1.5))
        #expect(v.count == 2)
        #expect(v.allSatisfy { $0.conflict })
        #expect(v.first?.endpoint.host == "192.168.50.20", "first announcement listed first")
    }

    /// The Mac adopts a new PC address only when the receiver signed that
    /// address inside the tagged body and it equals the datagram source.
    @Test func addressIsLearnedOnlyFromASignedMatchingAddress() throws {
        let from = try Endpoint.resolve("192.168.50.20:4810", defaultPort: 4810)
        #expect(Session.addressMatches([192, 168, 50, 20], from))
        #expect(!Session.addressMatches([192, 168, 50, 66], from), "relayed from another machine")
        #expect(!Session.addressMatches(nil, from), "0.1.x receiver signs no address")
        #expect(!Session.addressMatches([0, 0, 0, 0], from))
    }

    @Test func unauthenticatedDropsAreRateLimited() {
        var d = DropLog()
        let t0 = Date()
        #expect(d.note("x", at: t0) == "dropped x")
        for i in 1...50 { #expect(d.note("x", at: t0.addingTimeInterval(Double(i) * 0.05)) == nil) }
        #expect(d.note("x", at: t0.addingTimeInterval(5.1)) == "dropped x (+50 similar since the last report)")
    }

    @Test func urlTriggersAreIgnoredUnlessAllowed() {
        let u = URL(string: "tightwire://deck/pc")!
        #expect(URLTrigger.decide(u, allowed: false, alreadyAsked: true) == .ignore("URL triggers are off (Stream Deck pane)"))
        #expect(URLTrigger.decide(u, allowed: false, alreadyAsked: false) == .ask(.pc))
        #expect(URLTrigger.decide(u, allowed: true, alreadyAsked: true) == .act(.pc))
        #expect(URLTrigger.decide(URL(string: "tightwire://deck/x")!, allowed: true, alreadyAsked: true) == .ignore("unknown URL tightwire://deck/x"))
    }

    /// The root command never runs the user-writable bundle in place.
    @Test func rootInstallCommandCopiesVerifiesAndClearsTheEnvironment() {
        let cmd = HelperInstallCommand.make(appPath: "/Applications/Tight wire's.app", cdhash: "ABCDEF0123",
                                            script: "install-mac.sh", args: ["0.0.0.0", "192.168.50.20"])
        let steps = cmd.components(separatedBy: "; ")
        #expect(steps[0].hasPrefix("T=$(/usr/bin/mktemp -d /private/tmp/tightwire-helper."))
        #expect(steps.contains("/bin/chmod 700 \"$T\""))
        #expect(steps.contains { $0.hasPrefix("trap '/bin/rm -rf \"$T\"' EXIT") })
        #expect(steps.contains("/usr/bin/ditto '/Applications/Tight wire'\\''s.app' \"$T/Tightwire.app\" || exit 1"))
        let verify = steps.firstIndex { $0.contains("/usr/bin/codesign --verify --strict --deep -R='cdhash H\"abcdef0123\"' \"$T/Tightwire.app\"") }
        let run = steps.firstIndex { $0.hasPrefix("/usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin /bin/bash \"$T/Tightwire.app/Contents/Resources/longcable/scripts/install-mac.sh\" --skip-build '0.0.0.0' '192.168.50.20'") }
        #expect(verify != nil && run != nil && verify! < run!, "verify the copy before running it")
        #expect(!cmd.contains("/Applications/Tight wire's.app/Contents/Resources"), "nothing runs from the original bundle")
        let un = HelperInstallCommand.make(appPath: "/A.app", cdhash: "00", script: "uninstall-mac.sh", args: [])
        #expect(un.contains("/bin/bash \"$T/Tightwire.app/Contents/Resources/longcable/scripts/uninstall-mac.sh\" 2>&1"))
    }

    @Test func cliPairingFileIsCreatedOwnerOnly() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tw-store-\(getpid())")
        try? FileManager.default.removeItem(at: dir)
        let url = dir.appendingPathComponent("cli-pairing.json")
        let store = FilePairingStore(url: url)
        try store.save(Pairing(peerName: "PC", peerID: [UInt8](repeating: 1, count: 16), rootKey: [UInt8](repeating: 2, count: 32),
                               lastAddress: nil, hostName: nil))
        let mode = (try FileManager.default.attributesOfItem(atPath: url.path)[.posixPermissions] as! NSNumber).intValue
        #expect(mode == 0o600)
        let loaded = try store.load()
        #expect(loaded?.peerName == "PC")
        try? FileManager.default.removeItem(at: dir)
    }
}

struct LatencyMeasurementTests {
    /// Receiver clock = Mac clock + 5 s, running 80 ppm fast, symmetric
    /// 0.2 ms one-way delay plus some slow outliers: the fit recovers the
    /// offset within the reported uncertainty.
    @Test func clockSyncTracksOffsetAndDrift() throws {
        var c = ClockSync()
        let offset0 = 5e9, ppm = 80e-6
        for i in 0..<120 {
            let t1 = UInt64(1e9 + Double(i) * 250e6)
            let extra = i % 7 == 0 ? 3e6 : 0
            let tr = Double(t1) + 0.2e6 + extra + offset0 + Double(t1) * ppm
            let t4 = UInt64(Double(t1) + 0.4e6 + extra)
            c.add(ClockSample(t1: t1, t4: t4, receiverNanos: UInt64(tr)))
        }
        let t: UInt64 = 20_000_000_000
        let (o, u) = try #require(c.offset(at: t))
        let truth = offset0 + Double(t) * ppm
        #expect(abs(o - truth) <= u, "offset error \(abs(o - truth)) ns > uncertainty \(u) ns")
        #expect(u < 1e6)
        #expect(abs((c.driftPPM ?? 0) - 80) < 2)
    }

    @Test func recommendationFollowsTheMeasurements() {
        let clean = LatencyTest.recommend(jitterPeakMillis: 0.3, lost: 0, late: 0, underruns: 0, maxTargetMillis: 2.6, currentCapMicros: 20000)
        #expect(clean.framesPerPacket == 32 && clean.floorMicros == nil && clean.capMicros == 20000)
        let wifi = LatencyTest.recommend(jitterPeakMillis: 6, lost: 3, late: 2, underruns: 1, maxTargetMillis: 9.2, currentCapMicros: 20000)
        #expect(wifi.framesPerPacket == 256)
        #expect(wifi.floorMicros == 10000)
        #expect(wifi.capMicros == 24000)
    }

    @Test func distribution() throws {
        let d = try #require(Distribution([5, 1, 3, 2, 4]))
        #expect(d.min == 1 && d.median == 3 && d.max == 5 && d.mean == 3)
    }
}
