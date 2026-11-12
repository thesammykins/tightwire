import Foundation
import Testing
@testable import TightwireCore

/// SPEC §9 with fakes: Longcable, the Stream Deck app and the PC transport.
/// FakeLongcable follows Longcable's Target state machine at dd0dcea
/// (vendor/longcable/Sources/LongcableCore/Target.swift, Server.swift), as
/// observed on a real Mac: `add` creates an *enabled* target in "Waiting for
/// device" whose serial is the generated label; `share` flips `enabled` and
/// leaves the state string alone; `unshare` sets "Available on this Mac"; the
/// descriptors (real serial, "Waiting for Windows") load only when a USB/IP
/// client imports or lists, which is also when "In use by …" can appear.
final class FakeLongcable: LongcableControlling, @unchecked Sendable {
    struct Dev {
        var id: String; var model: String; var label: String; var hardwareSerial: String; var location: String
        var enabled: Bool; var state = "Available on this Mac"; var identity = false; var lastError: String?
        var plugged = true
        init(id: String, model: String, serial: String, enabled: Bool, location: String = "01133100") {
            self.id = id; self.model = model; self.hardwareSerial = serial; self.enabled = enabled; self.location = location
            label = "USB" + model.replacingOccurrences(of: ":", with: "").uppercased()
        }
    }
    let lock = NSLock()
    var devices: [Dev] = []
    var available: [Longcable.LocalDevice] = []
    var allowed: [String] = ["100.64.0.0/10"]
    var ignoreAllowPeer = false
    var heldByHelper: Set<String> = []
    weak var apps: FakeApps?
    var calls: [String] = []
    var failShare = false

    func status() throws -> Longcable.Status {
        lock.withLock {
            var s = Longcable.Status()
            s.devices = devices.map { d in
                Longcable.Device(id: d.id, name: "Stream Deck Plus", model: d.model, serial: d.identity ? d.hardwareSerial : d.label,
                                 enabled: d.enabled, state: d.plugged ? d.state : "Not plugged in", lastError: d.lastError,
                                 location: d.location)
            }
            s.availableDevices = available
            s.allowedPeers = allowed
            return s
        }
    }
    func set(_ id: String, _ on: Bool) throws {
        try lock.withLock {
            calls.append("\(on ? "share" : "unshare") \(id)")
            if on && failShare { throw LongcableError("disk full") }
            guard let i = devices.firstIndex(where: { $0.id == id }) else { throw LongcableError("Unknown device") }
            devices[i].enabled = on
            if !on { devices[i].state = "Available on this Mac" }
        }
    }
    func share(_ busID: String) throws { try set(busID, true) }
    func unshare(_ busID: String) throws { try set(busID, false) }
    func allowPeer(_ ipv4: String) throws {
        lock.withLock {
            calls.append("allow \(ipv4)")
            if !ignoreAllowPeer { allowed.append(ipv4) }
        }
    }
    func forgetPeer(_ peer: String) throws {
        lock.withLock {
            calls.append("forget \(peer)")
            allowed.removeAll { $0 == peer }
        }
    }
    func add(_ d: Longcable.LocalDevice) throws {
        lock.withLock {
            calls.append("add \(d.vendor):\(d.product)")
            available.removeAll { $0 == d }
            var dev = Dev(id: "1-\(devices.count + 1)", model: "\(d.vendor):\(d.product)", serial: d.serial, enabled: true, location: d.location)
            dev.state = "Waiting for device"
            devices.append(dev)
        }
    }
    /// What a USB/IP import from the PC does to the target.
    func importDevice(_ id: String) -> Bool {
        lock.withLock {
            guard let i = devices.firstIndex(where: { $0.id == id }), devices[i].enabled, devices[i].plugged else { return false }
            if heldByHelper.contains(id) && (apps?.running ?? false) {
                devices[i].lastError = "In use by Stream Deck (pid 412). Close it to share this device."
                return false
            }
            devices[i].identity = true
            devices[i].lastError = nil
            devices[i].state = "Connected to Windows"
            return true
        }
    }
    var shared: [String] { lock.withLock { devices.filter(\.enabled).map(\.id) } }
}

final class FakeApps: HelperAppControlling, @unchecked Sendable {
    var running = true
    var quits = 0, launches = 0
    var refuseQuit = false
    func isRunning(_ id: String) -> Bool { running }
    func quit(_ id: String, timeout: TimeInterval) -> Bool {
        quits += 1
        if !refuseQuit { running = false }
        return !running
    }
    func launch(_ id: String) { launches += 1; running = true }
}

/// The PC: an offer makes it import each BUSID from the fake Longcable.
final class FakeTransport: HandoffTransport, @unchecked Sendable {
    enum Mode { case ok, fail(String), silent }
    var offerMode: Mode = .ok
    var releaseMode: Mode = .ok
    weak var longcable: FakeLongcable?
    var offers: [[String]] = [], releases: [[String]] = []
    func reply(_ m: Mode, _ ids: [String], importing: Bool) throws -> [Wire.HandoffItem] {
        switch m {
        case .ok:
            return ids.enumerated().map { n, id in
                let ok = !importing || (longcable?.importDevice(id) ?? true)
                return .init(busID: id, result: ok ? .ok : .failed, port: ok ? UInt8(n + 1) : 0, message: ok ? "" : "usbip: error: import failed")
            }
        case .fail(let msg): return ids.map { .init(busID: $0, result: .failed, port: 0, message: msg) }
        case .silent: throw StoreError(description: "no answer from the PC")
        }
    }
    func offer(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
        offers.append(busIDs)
        return try reply(offerMode, busIDs, importing: true)
    }
    func release(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
        releases.append(busIDs)
        return try reply(releaseMode, busIDs, importing: false)
    }
}

let deck = HandoffDevice(vendor: 0x0fd9, product: 0x0084, serial: "A1", name: "Stream Deck +", location: "01133100", busID: "1-1")

func rig(busy: Bool = false) -> (HandoffSequencer, FakeLongcable, FakeApps, FakeTransport, Locked<[String]>) {
    let lc = FakeLongcable(), apps = FakeApps(), tr = FakeTransport()
    lc.apps = apps
    tr.longcable = lc
    lc.devices = [.init(id: "1-1", model: "0fd9:0084", serial: "A1", enabled: false)]
    if busy { lc.heldByHelper = ["1-1"] }
    let logs = Locked([String]())
    let s = HandoffSequencer(longcable: lc, apps: apps, transport: tr, sleep: { _ in }) { m in logs.update { $0.append(m) } }
    return (s, lc, apps, tr, logs)
}

struct SequencerTests {
    @Test func handoverHappyPath() throws {
        let (s, lc, apps, tr, _) = rig()
        let ids = try s.handOver([deck], pcAddress: "192.168.1.20")
        #expect(ids == ["1-1"])
        #expect(lc.calls == ["allow 192.168.1.20", "share 1-1"])
        #expect(tr.offers == [["1-1"]])
        #expect(apps.quits == 0, "nothing holds the deck, so the Stream Deck app is left alone")
        #expect(lc.shared == ["1-1"])
    }

    /// Real Longcable leaves the state at "Available on this Mac" after
    /// `share`; the old wait for "Waiting for Windows" always timed out.
    @Test func sharedTargetIsOfferedWhileStateStringIsStale() throws {
        let (s, lc, _, tr, _) = rig()
        _ = try s.handOver([deck], pcAddress: "192.168.1.20")
        #expect(tr.offers == [["1-1"]])
        #expect(lc.devices[0].state == "Connected to Windows")
    }

    @Test func peerListedExactlyIsNotAddedAgainNorRemoved() throws {
        let (s, lc, _, _, _) = rig()
        lc.allowed = ["100.64.0.0/10", "192.168.50.20"]  // a tailnet range plus the PC
        _ = try s.handOver([deck], pcAddress: "192.168.50.20")
        s.handBack([deck])
        #expect(!lc.calls.contains { $0.hasPrefix("allow") || $0.hasPrefix("forget") })
        #expect(lc.allowed == ["100.64.0.0/10", "192.168.50.20"])
    }

    /// Security: a subnet that happens to contain the PC is not permission;
    /// the PC is listed exactly for the handover and removed afterwards.
    @Test func wideRangeIsNotTakenAsPermissionAndAddedPeerIsRemovedOnHandback() throws {
        let (s, lc, _, _, _) = rig()
        lc.allowed = ["192.168.50.0/24"]
        _ = try s.handOver([deck], pcAddress: "192.168.50.20")
        #expect(lc.calls.contains("allow 192.168.50.20"))
        #expect(s.addedPeer == "192.168.50.20")
        s.handBack([deck])
        #expect(lc.calls.contains("forget 192.168.50.20"))
        #expect(lc.allowed == ["192.168.50.0/24"])
        #expect(s.addedPeer == nil)
    }

    @Test func narrowingDropsWideEntriesAndKeepsOnlyThePC() throws {
        let (s, lc, _, _, _) = rig()
        lc.allowed = ["100.64.0.0/10", "192.168.50.0/24", "192.168.50.20", "192.168.50.7"]
        let after = try s.narrowAllowList(to: "192.168.50.20", forgetting: ["192.168.50.7"])
        #expect(after == ["192.168.50.20"])
        #expect(Longcable.wideEntries(["any", "10.0.0.0/8", "10.0.0.1/32", "10.0.0.2"]) == ["any", "10.0.0.0/8"])
        #expect(Longcable.listsExactly("10.0.0.1", in: ["10.0.0.1/32"]))
        #expect(!Longcable.listsExactly("10.0.0.1", in: ["10.0.0.0/8"]))
    }

    @Test func allowPeerThatDoesNotTakeEffectFailsBeforeSharing() {
        let (s, lc, _, tr, _) = rig()
        lc.ignoreAllowPeer = true
        #expect(throws: HandoffSequencer.Failure.self) { try s.handOver([deck], pcAddress: "192.168.50.20") }
        #expect(!lc.calls.contains("share 1-1"))
        #expect(tr.offers.isEmpty)
        #expect(lc.shared.isEmpty)
    }

    @Test func busyImportQuitsHelperRetriesAndRelaunchesOnHandback() throws {
        let (s, lc, apps, tr, _) = rig(busy: true)
        let ids = try s.handOver([deck], pcAddress: "10.0.0.2")
        #expect(ids == ["1-1"])
        #expect(tr.offers == [["1-1"], ["1-1"]], "first import hit 'In use by', retried after the quit")
        #expect(apps.quits == 1)
        #expect(s.helperQuit)
        s.handBack([deck])
        #expect(tr.releases == [["1-1"]])
        #expect(lc.shared.isEmpty)
        #expect(apps.launches == 1)
        #expect(!s.helperQuit)
    }

    @Test func busyErrorFromAnEarlierAttemptQuitsBeforeOffering() throws {
        let (s, lc, apps, tr, _) = rig(busy: true)
        lc.devices[0].lastError = "In use by Stream Deck (pid 412). Close it to share this device."
        _ = try s.handOver([deck], pcAddress: "10.0.0.2")
        #expect(apps.quits == 1)
        #expect(tr.offers == [["1-1"]])
    }

    @Test func busyWithoutQuitOptionFailsAndReclaims() {
        let (s, lc, apps, _, _) = rig(busy: true)
        var d = deck
        d.quitHelperApp = false
        #expect(throws: HandoffSequencer.Failure.self) { try s.handOver([d], pcAddress: "10.0.0.2") }
        #expect(apps.quits == 0)
        #expect(lc.shared.isEmpty, "never leave a device shared with nobody attached")
    }

    @Test func helperThatWontQuitFailsAndReclaims() {
        let (s, lc, apps, _, _) = rig(busy: true)
        apps.refuseQuit = true
        #expect(throws: HandoffSequencer.Failure.self) { try s.handOver([deck], pcAddress: "10.0.0.2") }
        #expect(lc.shared.isEmpty)
        #expect(apps.launches == 0, "we did not quit it, so we do not relaunch it")
    }

    @Test func pcAttachFailureReclaims() {
        let (s, lc, _, tr, _) = rig()
        tr.offerMode = .fail("usbip: error: cannot connect")
        #expect(throws: HandoffSequencer.Failure.self) { try s.handOver([deck], pcAddress: "10.0.0.2") }
        #expect(lc.shared.isEmpty)
    }

    @Test func receiverVanishesMidHandoverReclaims() {
        let (s, lc, apps, tr, _) = rig(busy: true)
        lc.devices[0].lastError = "In use by Stream Deck (pid 412). Close it to share this device."
        tr.offerMode = .silent
        tr.releaseMode = .silent
        #expect(throws: HandoffSequencer.Failure.self) { try s.handOver([deck], pcAddress: "10.0.0.2") }
        #expect(lc.shared.isEmpty)
        #expect(apps.launches == 1, "Stream Deck app comes back after a failed handover")
    }

    @Test func handbackUnsharesEvenIfPCSilent() throws {
        let (s, lc, _, tr, _) = rig()
        _ = try s.handOver([deck], pcAddress: "10.0.0.2")
        tr.releaseMode = .silent
        #expect(s.handBack([deck]).isEmpty)
        #expect(lc.shared.isEmpty)
    }

    @Test func handbackCatchesDevicesSharedBySomeoneElse() {
        let (s, lc, _, _, _) = rig()
        lc.devices[0].enabled = true  // e.g. Longcable re-shared it after a restart
        s.handBack([deck], notifyPC: false)
        #expect(lc.shared.isEmpty)
    }

    @Test func startupReconcileUnsharesDevicesTightwireAddedEarlier() {
        let (s, lc, _, _, _) = rig()
        lc.devices.append(.init(id: "1-2", model: "0fd9:00a6", serial: "XLR", enabled: true, location: "01133200"))
        s.handBack([deck], notifyPC: false, extraBusIDs: ["1-2"])
        #expect(lc.shared.isEmpty)
    }

    @Test func registeringLeavesTheDeviceUnshared() throws {
        let (s, lc, _, _, _) = rig()
        lc.devices = []
        lc.available = [.init(vendor: "0fd9", product: "0084", location: "01133100", name: "Stream Deck Plus", serial: "EL00A0A00001")]
        let t = try s.register(lc.available[0])
        #expect(t.id == "1-1")
        #expect(t.enabled == false)
        #expect(lc.calls == ["add 0fd9:0084", "unshare 1-1"])
        #expect(lc.shared.isEmpty)
    }

    @Test func unknownDeviceIsRegisteredThenShared() throws {
        let (s, lc, _, _, _) = rig()
        lc.devices = []
        lc.available = [.init(vendor: "0fd9", product: "0084", location: "01133100", name: "Stream Deck Plus", serial: "EL00A0A00001")]
        let learned = Locked([String]())
        s.onResolved = { _, id, loc in learned.update { $0.append("\(id)@\(loc ?? "")") } }
        let legacy = HandoffDevice(vendor: 0x0fd9, product: 0x0084, serial: "EL00A0A00001", name: "Stream Deck Plus")
        let ids = try s.handOver([legacy], pcAddress: "10.0.0.2")
        #expect(ids == ["1-1"])
        #expect(Array(lc.calls.prefix(2)) == ["add 0fd9:0084", "unshare 1-1"])
        #expect(learned.value == ["1-1@01133100"])
    }

    /// The bug from the first real test: saved serial "EL00A0A00001" (IORegistry)
    /// vs Longcable's "USB0FD90084" before it reads the descriptors.
    @Test func targetIsFoundAfterAddDespiteLabelSerial() throws {
        let (s, lc, _, tr, _) = rig()
        lc.devices = []
        lc.available = [.init(vendor: "0fd9", product: "0084", location: "01133100", name: "Stream Deck Plus", serial: "EL00A0A00001")]
        let saved = HandoffDevice(vendor: 0x0fd9, product: 0x0084, serial: "EL00A0A00001", name: "Stream Deck Plus")
        try s.register(lc.available[0])
        #expect(try lc.status().devices?.first?.serial == "USB0FD90084")
        _ = try s.handOver([saved], pcAddress: "10.0.0.2")
        #expect(tr.offers == [["1-1"]])
        s.handBack([saved])
        #expect(lc.shared.isEmpty, "handBack finds it too")
    }

    @Test func deselectUnsharesAndReleases() throws {
        let (s, lc, _, tr, _) = rig()
        _ = try s.handOver([deck], pcAddress: "10.0.0.2")
        s.deselect(deck)
        #expect(tr.releases == [["1-1"]])
        #expect(lc.shared.isEmpty)
    }

    @Test func unpluggedDeviceFails() {
        let (s, lc, _, _, _) = rig()
        lc.devices[0].plugged = false
        #expect(throws: HandoffSequencer.Failure.self) { try s.handOver([deck], pcAddress: "10.0.0.2") }
    }

    @Test func shareErrorReclaims() {
        let (s, lc, _, _, _) = rig()
        lc.failShare = true
        #expect(throws: HandoffSequencer.Failure.self) { try s.handOver([deck], pcAddress: "10.0.0.2") }
        #expect(lc.shared.isEmpty)
    }

    @Test func ipv6PCIsRejectedClearly() {
        let (s, lc, _, _, _) = rig()
        #expect(throws: HandoffSequencer.Failure("Longcable only accepts IPv4 peers; the PC is known as fe80::1")) {
            try s.handOver([deck], pcAddress: "fe80::1")
        }
        #expect(lc.shared.isEmpty)
    }
}

struct MachineTests {
    @Test func autoHandsOverWhenAliveAndBackAfterFiveSecondsOfSilence() {
        var m = HandoffMachine()
        #expect(m.update(alive: true, now: 0) == .handOver)
        #expect(m.update(alive: true, now: 1) == nil, "one operation at a time")
        m.finished(.handOver, success: true, now: 2)
        #expect(m.owner == .pc)
        #expect(m.update(alive: false, now: 4) == nil, "within the 5 s grace")
        #expect(m.update(alive: false, now: 7.1) == .handBack)
        m.finished(.handBack, success: true, now: 8)
        #expect(m.owner == .mac)
    }

    @Test func backToMacHoldsUntilPCReconnectsAfterGoingAway() {
        var m = HandoffMachine()
        _ = m.update(alive: true, now: 0)
        m.finished(.handOver, success: true, now: 0)
        #expect(m.request(.mac, alive: true, now: 1) == .handBack)
        m.finished(.handBack, success: true, now: 1)
        #expect(m.mode == .heldOnMac)
        #expect(m.update(alive: true, now: 2) == nil, "held while the PC stays up")
        #expect(m.update(alive: false, now: 10) == nil)
        #expect(m.update(alive: true, now: 20) == .handOver, "PC came back after going away: auto again")
        #expect(m.mode == .auto)
    }

    @Test func restoredHoldSurvivesFirstContact() {
        var m = HandoffMachine(mode: .heldOnMac)
        #expect(m.update(alive: false, now: 0) == nil)
        #expect(m.update(alive: true, now: 3) == nil, "the PC was never seen before, so it did not 'come back'")
        #expect(m.mode == .heldOnMac)
        #expect(m.update(alive: false, now: 20) == nil)
        #expect(m.update(alive: true, now: 30) == .handOver, "after going away and returning, auto again")
    }

    @Test func sendToPCClearsHold() {
        var m = HandoffMachine()
        _ = m.request(.mac, alive: true, now: 0)
        #expect(m.request(.pc, alive: true, now: 1) == .handOver)
        #expect(m.mode == .auto)
    }

    @Test func sendToPCWhileUnreachableWaits() {
        var m = HandoffMachine()
        #expect(m.request(.pc, alive: false, now: 0) == nil)
        #expect(m.update(alive: true, now: 3) == .handOver)
    }

    @Test func toggleFlipsOwner() {
        var m = HandoffMachine()
        _ = m.update(alive: true, now: 0)
        m.finished(.handOver, success: true, now: 0)
        #expect(m.request(.toggle, alive: true, now: 1) == .handBack)
        m.finished(.handBack, success: true, now: 1)
        #expect(m.request(.toggle, alive: true, now: 2) == .handOver)
    }

    @Test func duplicateRequestsDuringTransitionCoalesce() {
        var m = HandoffMachine()
        #expect(m.request(.pc, alive: true, now: 0) == .handOver)
        #expect(m.request(.pc, alive: true, now: 0.1) == nil)
        #expect(m.request(.pc, alive: true, now: 0.2) == nil)
        // A "mac" arriving mid-handover is applied after it finishes.
        #expect(m.request(.mac, alive: true, now: 0.3) == nil)
        m.finished(.handOver, success: true, now: 1)
        #expect(m.update(alive: true, now: 1.1) == .handBack)
    }

    @Test func failedHandoverBacksOffBeforeRetrying() {
        var m = HandoffMachine()
        #expect(m.update(alive: true, now: 0) == .handOver)
        m.finished(.handOver, success: false, now: 1)
        #expect(m.owner == .mac)
        #expect(m.update(alive: true, now: 5) == nil)
        #expect(m.update(alive: true, now: 31.5) == .handOver)
    }

    @Test func explicitRequestSkipsBackoff() {
        var m = HandoffMachine()
        _ = m.update(alive: true, now: 0)
        m.finished(.handOver, success: false, now: 1)
        #expect(m.request(.pc, alive: true, now: 2) == .handOver)
    }
}

struct LongcableParsingTests {
    /// Shaped like JSONEncoder output of Longcable's ControlReply (Models.swift
    /// at dd0dcea): dates as reference-date seconds, optional keys present or not.
    static let reply = #"""
    {"ok":true,"message":"","status":{"version":"Swift 1.0","started":784000000.5,"listen":"0.0.0.0","peer":"192.168.1.0\/24",
     "devices":[{"id":"1-1","name":"Stream Deck +","model":"0fd9:0084","serial":"CL12K1A01234","enabled":true,
       "state":"Waiting for device","location":"02100000","completed":0,"bytes":0,"errors":0,"pending":0,
       "lastError":"In use by Stream Deck (pid 412). Close it to share this device."},
      {"id":"1-2","name":"VKB","model":"231d:0201","serial":"VKBLEFT0201","enabled":false,"state":"Not plugged in",
       "location":"02131200","completed":5,"bytes":320,"errors":0,"pending":0,"lastActivity":784000100}],
     "refusedPeers":[],"allowedPeers":["192.168.1.0\/24"],
     "availableDevices":[{"vendor":"0fd9","product":"0070","location":"01100000","name":"Wave:3","serial":"XYZ"}]}}
    """#

    @Test func decodesRealShape() throws {
        let r = try Longcable.decodeReply(Data(Self.reply.utf8))
        #expect(r.ok)
        let d = try #require(r.status?.devices?.first)
        #expect(d.id == "1-1")
        #expect(Longcable.isBusy(d.lastError))
        #expect(d.location == "02100000")
        #expect(r.status?.availableDevices?.first?.product == "0070")
    }

    @Test func toleratesMissingAndExtraFields() throws {
        let r = try Longcable.decodeReply(Data(#"{"ok":false,"message":"Unknown device","future":1}"#.utf8))
        #expect(!r.ok)
        #expect(r.status == nil)
        let r2 = try Longcable.decodeReply(Data(#"{"ok":true,"status":{"devices":[{"id":"1-3"}]}}"#.utf8))
        #expect(r2.status?.devices?.first?.state == nil)
    }

    @Test func busyMessagesFromLongcableSource() {
        #expect(Longcable.isBusy("In use by Stream Deck (pid 1). Close it to share this device."))
        #expect(Longcable.isBusy("Another app or driver is using this device. Close it to share the device."))
        #expect(Longcable.isBusy("In use by the macOS driver X"))
        #expect(!Longcable.isBusy("USB status -71 (0xe00002ed)"))
        #expect(!Longcable.isBusy(nil))
    }

    static func fixture(_ name: String) throws -> Longcable.Status {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("Fixtures/\(name).json")
        return try #require(try Longcable.decodeReply(Data(contentsOf: url)).status)
    }

    /// Real `longcable-server --control status` output (serial numbers and
    /// timestamps replaced with fake values before publication)
    /// (Longcable dd0dcea, Nov 2026), captured before and after Longcable had
    /// read the Stream Deck's descriptors. The selection saved by the first
    /// build had only VID:PID + IORegistry serial.
    @Test func realStatusFindsTheDeckBeforeAndAfterIdentityLoads() throws {
        let before = try Self.fixture("longcable-status-before-identity")
        let after = try Self.fixture("longcable-status-after-identity")
        #expect(before.devices?.first?.serial == "USB0FD90084")
        #expect(after.devices?.first?.serial == "EL00A0A00001")
        #expect(before.allowedPeers == ["100.64.0.0/10"])
        let legacy = HandoffDevice(vendor: 0x0fd9, product: 0x0084, serial: "EL00A0A00001", name: "Stream Deck Plus")
        let current = HandoffDevice(vendor: 0x0fd9, product: 0x0084, serial: "EL00A0A00001", name: "Stream Deck Plus",
                                    location: "01133100", busID: "1-1")
        for s in [before, after] {
            #expect(Longcable.target(for: legacy, in: s)?.id == "1-1")
            #expect(Longcable.target(for: current, in: s)?.id == "1-1")
            #expect(Longcable.target(for: current, in: s)?.enabled == false)
            #expect(Longcable.target(for: current, in: s)?.present == true)
        }
        // The XLR Dock target must never be mistaken for the deck.
        let xlr = HandoffDevice(vendor: 0x0fd9, product: 0x00a6, serial: "", name: "Elgato XLR Dock", location: "01133200")
        #expect(Longcable.target(for: xlr, in: before)?.id == "1-2")
        #expect(Longcable.target(for: current, in: before)?.model == "0fd9:0084")
    }

    @Test func allowListMatching() {
        #expect(!Longcable.peerAllowed("192.168.50.20", by: ["100.64.0.0/10"]))
        #expect(Longcable.peerAllowed("100.100.1.2", by: ["100.64.0.0/10"]))
        #expect(Longcable.peerAllowed("192.168.50.20", by: ["100.64.0.0/10", "192.168.50.20"]))
        #expect(Longcable.peerAllowed("192.168.50.20", by: ["192.168.50.0/24"]))
        #expect(Longcable.peerAllowed("10.1.2.3", by: ["any"]))
        #expect(!Longcable.peerAllowed("fe80::1", by: ["any"]))
    }

    @Test func elgatoDevicesDefaultToQuittingTheApp() {
        #expect(HandoffDevice(vendor: 0x0fd9, product: 0x0084, serial: "", name: "x").quitHelperApp)
        #expect(!HandoffDevice(vendor: 0x231d, product: 0x0201, serial: "", name: "x").quitHelperApp)
    }
}

struct HandoffPolicyTests {
    @Test func usbInterfaceClassesMapToRoles() {
        #expect(USBDeviceRole.roles([.init(3, 0, 0)]).isEmpty, "Stream Deck: HID, no boot protocol")
        #expect(USBDeviceRole.roles([.init(1, 1), .init(1, 2), .init(3)]) == [.audio])
        #expect(USBDeviceRole.roles([.init(3, 1, 1), .init(3, 1, 2)]) == [.keyboard, .mouse])
        #expect(USBDeviceRole.roles([.init(8, 6, 0x50)]) == [.storage])
    }

    @Test func streamInputInterfaceIsRefused() {
        // CoreAudio UID shapes as seen on a real Mac (serials replaced with fakes).
        #expect(HandoffPolicy.feedsStream(vendor: 0x0fd9, serial: "0000XLRDOCK001", location: "01133200", roles: [.audio],
                                          inputUID: "AppleUSBAudioEngine:Elgato Systems:Elgato XLR Dock:0000XLRDOCK001:2,1", inputName: "Elgato XLR Dock"))
        #expect(HandoffPolicy.feedsStream(vendor: 0x2e1a, serial: "", location: "00122100", roles: [.audio, .camera],
                                          inputUID: "AppleUSBAudioEngine:ExampleCam:Example Webcam:122100:3", inputName: "Example Webcam"))
        #expect(HandoffPolicy.feedsStream(vendor: 0x0fd9, serial: "", location: "01133200", roles: [.audio],
                                          inputUID: "ElgatoWaveLinkMicrophoneFXIn", inputName: "Wave Link MicrophoneFX"),
                "Wave Link reads from the Elgato interface")
        #expect(!HandoffPolicy.feedsStream(vendor: 0x0fd9, serial: "EL00A0A00001", location: "01133100", roles: [],
                                           inputUID: "ElgatoWaveLinkMicrophoneFXIn", inputName: "Wave Link MicrophoneFX"),
                "the Stream Deck is not audio")
        #expect(!HandoffPolicy.feedsStream(vendor: 0x2e1a, serial: "", location: "00122100", roles: [.audio],
                                           inputUID: "ElgatoWaveLinkMicrophoneFXIn", inputName: "Wave Link MicrophoneFX"))
        if case .refuse = HandoffPolicy.verdict(name: "Elgato XLR Dock", roles: [.audio], feedsStream: true) {} else { Issue.record("not refused") }
        if case .warn = HandoffPolicy.verdict(name: "Keyboard", roles: [.keyboard], feedsStream: false) {} else { Issue.record("no warning") }
        #expect(HandoffPolicy.verdict(name: "Stream Deck", roles: [], feedsStream: false) == .ok)
    }

    @Test func onlyNonAudioElgatoDevicesAreSuggested() {
        #expect(HandoffPolicy.suggested(vendor: 0x0fd9, roles: []))
        #expect(!HandoffPolicy.suggested(vendor: 0x0fd9, roles: [.audio]), "never the XLR Dock")
        #expect(!HandoffPolicy.suggested(vendor: 0x1b1c, roles: []))
    }
}

struct AppLogTests {
    @Test func rotatesAtTheSizeLimitKeepingThree() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tw-log-\(getpid())-\(UInt32.random(in: 0...UInt32.max))")
        defer { try? FileManager.default.removeItem(at: dir) }
        let log = AppLog(directory: dir, maxBytes: 200, keep: 3)
        for i in 0..<40 { log.write("test", "line \(i) padding padding padding") }
        let names = try FileManager.default.contentsOfDirectory(atPath: dir.path).sorted()
        #expect(names == ["tightwire.log", "tightwire.log.1", "tightwire.log.2", "tightwire.log.3"])
        let current = try String(contentsOf: log.fileURL, encoding: .utf8)
        #expect(current.contains("line 39"))
        #expect(current.contains("[test]"))
    }
}
