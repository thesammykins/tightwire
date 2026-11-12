import Foundation
import Testing
@testable import TightwireCore

/// Cross-language handoff run on localhost (SPEC §9): the Swift
/// HandoffController (with fake Longcable and Stream Deck app) talks to the
/// real Rust receiver, whose usbip.exe is scripts/fake-usbip.sh. Then the
/// receiver's `deck` command asks for the devices back. Runs only when
/// `mise run loopback` provides TIGHTWIRE_RECV.
struct HandoffIntegrationTests {
    static let recv = ProcessInfo.processInfo.environment["TIGHTWIRE_RECV"]

    @Test(.enabled(if: recv != nil), .timeLimit(.minutes(1)))
    func swiftControllerDrivesRustReceiver() async throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        let work = FileManager.default.temporaryDirectory.appendingPathComponent("tw-handoff-\(getpid())")
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: work) }
        let rootKey = secureRandom(32)
        let port = UInt16.random(in: 42000...48000)
        let state = work.appendingPathComponent("usbip-state")

        let receiver = Process()
        receiver.executableURL = URL(fileURLWithPath: Self.recv!)
        receiver.arguments = ["--backend", "file", "--out", work.appendingPathComponent("x.wav").path,
                              "--bind", "127.0.0.1", "--port", "\(port)", "--psk-hex", Hex.encode(rootKey),
                              "--usbip", root.appendingPathComponent("scripts/fake-usbip.sh").path, "--duration", "40"]
        receiver.environment = ["FAKE_USBIP_STATE": state.path]
        receiver.standardError = FileHandle(forWritingAtPath: "/dev/null")
        try receiver.run()
        defer { receiver.terminate() }

        let lc = FakeLongcable(), apps = FakeApps()
        lc.apps = apps
        lc.devices = [.init(id: "1-1", model: "0fd9:0084", serial: "A1", enabled: false)]
        // The real PC import is the stand-in usbip.exe, which never consults
        // the fake Longcable; record "held" as an earlier attempt would have.
        lc.heldByHelper = ["1-1"]
        lc.devices[0].lastError = "In use by Stream Deck (pid 412). Close it to share this device."
        let pairing = Pairing(peerName: "test-pc", peerID: Array(repeating: 0, count: 16), rootKey: rootKey,
                              lastAddress: "127.0.0.1:\(port)", hostName: nil)
        // Never 4813: a running Tightwire app owns it, and this test must
        // not send it anything.
        let macPort = UInt16.random(in: 48001...48999)
        let controller = try HandoffController(pairing: pairing, longcable: lc, apps: apps,
                                               helperBundleID: "com.elgato.StreamDeck",
                                               logSink: { _ in }, localPort: macPort) { ([deck], pairing) }
        try #require(controller.localPort == macPort, "test port \(macPort) is taken")

        // Auto mode: once the receiver answers probes the deck moves to the PC.
        try await waitFor(10) { controller.snapshot.owner == .pc }
        #expect(lc.shared == ["1-1"])
        #expect(apps.quits == 1, "the Stream Deck app held the deck, so it was quit")
        let attached = try String(contentsOf: state, encoding: .utf8)
        #expect(attached.contains("127.0.0.1 1-1"), "receiver ran usbip attach -r <mac> -b 1-1: \(attached)")

        // The PC's Stream Deck button: `tightwire-recv deck mac`.
        let conf = work.appendingPathComponent("pairing.conf")
        try """
        self_id = \(Hex.encode(Array(repeating: 1, count: 16)))
        peer_name = test-mac
        peer_id = \(Hex.encode(Array(repeating: 2, count: 16)))
        root_key = \(Hex.encode(rootKey))
        last_address = 127.0.0.1:\(Wire.senderPort)
        """.write(to: conf, atomically: true, encoding: .utf8)
        let deckCmd = Process()
        deckCmd.executableURL = URL(fileURLWithPath: Self.recv!)
        deckCmd.arguments = ["deck", "mac", "--config", conf.path, "--mac-port", "\(macPort)"]
        let out = Pipe()
        deckCmd.standardOutput = out
        try deckCmd.run()
        deckCmd.waitUntilExit()
        let text = String(decoding: out.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        #expect(deckCmd.terminationStatus == 0, "deck command: \(text)")
        #expect(text.contains("accepted"))

        try await waitFor(10) { controller.snapshot.owner == .mac && controller.snapshot.busy == nil }
        #expect(controller.snapshot.mode == .heldOnMac)
        #expect(lc.shared.isEmpty)
        #expect(apps.launches == 1, "Stream Deck app relaunched after handback")
        let after = try String(contentsOf: state, encoding: .utf8)
        #expect(!after.contains("1-1"), "receiver detached the device: \(after)")
        let calls = try String(contentsOf: URL(fileURLWithPath: state.path + ".calls"), encoding: .utf8)
        #expect(calls.contains("detach -p 1") && calls.contains("-b 1-1 -x"), "\(calls)")

        // An unauthenticated deck request is dropped.
        let bogus = Wire.seal(Wire.encode(.deckRequest(requestID: 5, sentAtMillis: UInt64(Date().timeIntervalSince1970 * 1000), target: .pc)),
                              key: Array(repeating: 9, count: 16))
        let s = try UDPSocket(preferredPort: 0)
        let to = try Endpoint.resolve("127.0.0.1:\(macPort)", defaultPort: 0)
        for _ in 0..<20 { s.send(bogus, to: to) }
        try await Task.sleep(for: .milliseconds(500))
        #expect(controller.snapshot.owner == .mac)
        // Logged once (rate-limited), not once per packet.
        #expect(controller.snapshot.log.filter { $0.contains("dropped unauthenticated deck request") }.count == 1)
        controller.shutdown()
    }

    func waitFor(_ seconds: Double, _ cond: () -> Bool) async throws {
        let end = Date().addingTimeInterval(seconds)
        while Date() < end {
            if cond() { return }
            try await Task.sleep(for: .milliseconds(100))
        }
        Issue.record("condition not met within \(seconds) s")
        throw CancellationError()
    }
}
