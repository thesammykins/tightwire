import AppKit
import AVFoundation
import Foundation
import Observation
import ServiceManagement
import TightwireCore

/// App state and the only owner of the streaming session. Main-actor bound;
/// background work (pairing, capture callbacks, Longcable) hops back here to
/// publish. A model built with `live: false` touches nothing outside itself
/// (no Core Audio, Keychain, Longcable, defaults or timers); the snapshot
/// harness fills it with canned state.
@MainActor
@Observable
final class AppModel {
    enum Phase: Equatable {
        case unpaired, idle, searching, streaming
        case error(String)
    }

    enum PairingFlow: Equatable {
        case none
        case browsing
        case connecting(String)
        case confirm(code: String, peer: String)
        case failed(String)
    }

    enum Pane: String, CaseIterable, Identifiable, Hashable {
        case stream, receiver, handoff, setup
        var id: String { rawValue }
    }

    struct PairingCandidate: Identifiable, Equatable {
        let id: String
        let host: String
        let address: String
        var conflict = false
    }

    /// One USB device in the handoff list: a Longcable target or a connected
    /// device Longcable could register.
    struct HandoffRow: Identifiable, Equatable {
        var device: HandoffDevice
        var registered: Bool
        var shared: Bool
        var state: String
        var roles: [USBDeviceRole]
        var verdict: HandoffPolicy.Verdict
        var suggested: Bool
        var id: String { device.id }
    }

    // Persisted settings (UserDefaults; nil for a preview model).
    /// The user's saved input (UID); empty = the system default input.
    var deviceUID: String {
        didSet {
            defaults?.set(deviceUID, forKey: "deviceUID")
            if live, oldValue != deviceUID { log("app", "input device: \(selectedDevice?.name ?? deviceUID)") }
        }
    }
    /// Channel choice per input device ("channel.<uid>"); a device without a
    /// saved choice sends all its channels when it has more than one. (The
    /// 0.1 app stored one global "channel", which defaulted to mono ch 1 and
    /// sent stereo mixes left-only; it is no longer read.)
    var channel: Int {
        get {
            _ = channelRevision
            guard let d = selectedDevice else { return 0 }
            if let saved = defaults?.object(forKey: "channel.\(d.uid)") as? Int ?? previewChannels[d.uid] {
                return saved == Self.allChannels || saved < d.channels ? saved : 0
            }
            return AudioDevices.defaultChannels(for: d) == .all ? Self.allChannels : 0
        }
        set {
            guard let d = selectedDevice else { return }
            if let defaults { defaults.set(newValue, forKey: "channel.\(d.uid)") } else { previewChannels[d.uid] = newValue }
            channelRevision += 1
        }
    }
    private var channelRevision = 0
    private var previewChannels: [String: Int] = [:]
    /// tightwire://deck/… URLs act only when the owner allowed them (asked
    /// once, the first time one arrives); any app or web page can open a URL.
    var allowURLTriggers: Bool { didSet { defaults?.set(allowURLTriggers, forKey: "allowURLTriggers") } }
    @ObservationIgnored private var urlTriggersAsked: Bool {
        get { defaults?.bool(forKey: "urlTriggersAsked") ?? false }
        set { defaults?.set(newValue, forKey: "urlTriggersAsked") }
    }
    var framesPerPacket: Int { didSet { defaults?.set(framesPerPacket, forKey: "framesPerPacket") } }
    var addressOverride: String { didSet { defaults?.set(addressOverride, forKey: "addressOverride") } }
    var handoffDevices: [HandoffDevice] {
        didSet {
            defaults?.set(try? JSONEncoder().encode(handoffDevices), forKey: "handoffDevices")
            handoffSettings.value = handoffDevices
        }
    }
    var helperBundleID: String { didSet { defaults?.set(helperBundleID, forKey: "helperBundleID") } }
    /// Longcable BUSIDs Tightwire registered; start-up unshares the ones no
    /// longer selected.
    var addedBusIDs: [String] { didSet { defaults?.set(addedBusIDs, forKey: "longcableAddedBusIDs") } }
    private var handoffMode: HandoffMachine.Mode {
        didSet { if oldValue != handoffMode { defaults?.set(handoffMode.rawValue, forKey: "handoffMode") } }
    }

    // Display state (set directly by the snapshot harness).
    var devices: [InputDevice] = []
    var pairing: Pairing?
    var phase: Phase = .unpaired
    var level: Float = 0
    var stats: Wire.Stats?
    var statsFresh = false
    var lastError: String?
    /// Session details for the Stream pane (latency breakdown, test).
    var sessionStatus: SessionStatus?
    /// Result of the last latency test (kept after the stream stops).
    var testResult: LatencyTestResult?
    var testProgress: Double?
    var tuning: TuningState?
    var keepingAwake = false
    var allowListMessage: String?
    var flow: PairingFlow = .none
    var candidates: [PairingCandidate] = []
    var handoffState = HandoffState()
    var handoffRows: [HandoffRow] = []
    var longcableError: String?
    var allowedPeers: [String] = []
    var helperRunning = false
    var registering: Set<String> = []
    var rowMessages: [String: String] = [:]
    var pendingWarning: HandoffRow?
    var setup: SetupStatus
    var setupMessage: String?
    var pane: Pane = .stream
    /// Set by the menu/window: opens (or focuses) the main window. SwiftUI
    /// hands it over only once it has built the menu, which can be after
    /// launch finished; a request made before then is replayed here.
    var openWindowAction: (() -> Void)? {
        didSet {
            if pendingShow, openWindowAction != nil {
                pendingShow = false
                showWindow()
            }
        }
    }
    @ObservationIgnored private var pendingShow = false

    static let packetSizes = [32, 48, 64, 128, 256]
    /// Channel value meaning "send all channels".
    static let allChannels = -1

    let live: Bool
    private let defaults: UserDefaults?
    private var store: PairingStore?
    private var session: Session?
    private var listener: BeaconListener?
    private var discovered: [DiscoveredReceiver] = []
    private var initiator: PairingInitiator?
    private var verdict: Locked<Bool?>?
    private var timer: Timer?
    private var handoff: HandoffController?
    private let longcable = LongcableSocketClient()
    private let handoffSettings = Locked<[HandoffDevice]>([])
    /// Longcable work while no handoff controller runs (nothing paired yet).
    @ObservationIgnored private lazy var offline = HandoffSequencer(longcable: longcable, apps: WorkspaceApps(), transport: NoPCTransport()) {
        AppLog.shared.write("handoff", $0)
    }
    private let longcableQueue = DispatchQueue(label: "tightwire.longcable")

    init(live: Bool = true) {
        self.live = live
        defaults = live ? .standard : nil
        deviceUID = defaults?.string(forKey: "deviceUID") ?? ""
        allowURLTriggers = defaults?.bool(forKey: "allowURLTriggers") ?? false
        framesPerPacket = defaults?.object(forKey: "framesPerPacket") as? Int ?? 48
        addressOverride = defaults?.string(forKey: "addressOverride") ?? ""
        handoffDevices = defaults?.data(forKey: "handoffDevices").flatMap { try? JSONDecoder().decode([HandoffDevice].self, from: $0) } ?? []
        helperBundleID = defaults?.string(forKey: "helperBundleID") ?? HandoffSequencer.defaultHelperBundleID
        addedBusIDs = defaults?.stringArray(forKey: "longcableAddedBusIDs") ?? []
        handoffMode = (defaults?.string(forKey: "handoffMode")).flatMap(HandoffMachine.Mode.init(rawValue:)) ?? .auto
        setup = SetupStatus.placeholder
        handoffSettings.value = handoffDevices
        guard live else { return }

        log("app", "\(BuildInfo.label) started (pid \(getpid()), macOS \(ProcessInfo.processInfo.operatingSystemVersionString))")
        store = KeychainPairingStore()
        refreshDevices()
        log("app", "input device: \(selectedDevice.map { "\($0.name) [\($0.uid)]" } ?? "none")")
        do {
            pairing = try store?.load()
        } catch {
            lastError = "\(error)"
            log("error", "could not read the pairing from the Keychain: \(error)")
        }
        log("app", pairing.map { "paired with \($0.peerName) (\($0.lastAddress ?? $0.hostName ?? "address unknown"))" } ?? "not paired")
        phase = pairing == nil ? .unpaired : .idle
        log("app", "handoff devices: \(handoffDevices.isEmpty ? "none" : handoffDevices.map { "\($0.name) \($0.model) \($0.busID ?? "unregistered")" }.joined(separator: ", ")); mode \(handoffMode.rawValue)")
        refreshSetup()
        restartHandoff()
        refreshLongcable()
        if !setup.complete { pane = .setup }
        timer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
    }

    func log(_ category: String, _ message: String) {
        guard live else { return }
        AppLog.shared.write(category, message)
    }

    var isRunning: Bool { session != nil || phase == .streaming || phase == .searching }

    /// The saved input when present, else the system default input.
    var selectedDevice: InputDevice? {
        live ? AudioDevices.pick(saved: deviceUID, in: devices)
            : AudioDevices.pick(saved: deviceUID, in: devices, systemDefault: devices.first?.id)
    }

    var peerName: String { pairing?.peerName ?? "PC" }

    /// The PC's IPv4 from the pairing, as Longcable's allow-list needs it.
    var pcIPv4: String? {
        guard let a = pairing?.lastAddress else { return nil }
        let host = a.hasPrefix("[") ? String(a.dropFirst().prefix { $0 != "]" }) : String(a.split(separator: ":").first ?? "")
        var probe = in_addr()
        return inet_pton(AF_INET, host, &probe) == 1 ? host : nil
    }

    /// One line for the menu and the window's sidebar.
    var statusLine: String {
        switch phase {
        case .unpaired: "Not paired"
        case .idle: "Paired with \(peerName)"
        case .searching: "Looking for \(peerName)…"
        case .streaming: "Streaming to \(peerName)"
        case .error(let m): "Stopped: \(m)"
        }
    }

    var menuIcon: MenuIcon.State {
        if case .confirm = flow { return .code }
        switch phase {
        case .unpaired, .idle: return .idle
        case .searching: return .searching
        case .streaming: return .streaming
        case .error: return .error
        }
    }

    /// The saved input is not connected: the system default is used meanwhile.
    var savedInputMissing: Bool { !deviceUID.isEmpty && AudioDevices.find(deviceUID, in: devices) == nil }

    func refreshDevices() {
        guard live else { return }
        devices = AudioDevices.inputs()
        if savedInputMissing {
            log("app", "saved input \(deviceUID) is not connected; using the system default (\(selectedDevice?.name ?? "none"))")
        }
        channelRevision += 1
    }

    /// The PC runs Tightwire before 0.2.0 (no revision in its hello).
    var receiverOutdated: Bool { sessionStatus?.receiverRevision == 0 || handoffState.receiverRevision == 0 }

    // MARK: Window

    /// Opens the main window on `pane` and brings it in front of other apps
    /// (Tightwire is a menu bar app, so it has to activate itself).
    func showWindow(_ pane: Pane? = nil) {
        if let pane { self.pane = pane }
        guard let open = openWindowAction else {
            pendingShow = true
            return
        }
        open()
        NSApp.activate(ignoringOtherApps: true)
        for w in NSApp.windows where w.identifier?.rawValue == MainWindow.identifier {
            w.makeKeyAndOrderFront(nil)
            w.orderFrontRegardless()
        }
    }

    func openLog() {
        let url = AppLog.shared.fileURL
        if !FileManager.default.fileExists(atPath: url.path) { log("app", "log opened") }
        NSWorkspace.shared.open(url)
    }

    // MARK: Streaming

    func toggle() {
        if session != nil { stop(reason: "stopped by the user") } else { start() }
    }

    func start() {
        guard live else { return }
        guard let pairing else { phase = .unpaired; return }
        guard let device = selectedDevice else { fail("No input device"); return }
        lastError = nil
        log("stream", "start requested: \(device.name), \(channel == Self.allChannels ? "all channels" : "mono ch \(channel + 1)"), \(framesPerPacket) frames, to \(addressOverride.isEmpty ? pairing.lastAddress ?? pairing.hostName ?? "?" : addressOverride)")
        Task {
            // Ask before touching the HAL so the prompt is ours, not a silent failure.
            guard await AVCaptureDevice.requestAccess(for: .audio) else {
                fail("Microphone access is off. Enable Tightwire in System Settings › Privacy & Security › Microphone.")
                return
            }
            do {
                let capture = try CoreAudioCapture(device: device, framesPerBuffer: UInt32(framesPerPacket)) { msg in
                    Task { @MainActor in self.interrupted(msg) }
                }
                let cfg = SessionConfig(
                    pairing: pairing,
                    address: addressOverride.isEmpty ? nil : addressOverride,
                    framesPerPacket: UInt32(framesPerPacket),
                    channels: channel == Self.allChannels ? .all : .mono(channel))
                session = try Session(config: cfg, source: capture)
                phase = .searching
                updateKeepAwake()
                log("stream", "started (\(capture.bufferFrames) frames at \(capture.sampleRate) Hz); waiting for \(pairing.peerName)")
            } catch {
                fail("\(error)")
            }
        }
    }

    func stop(reason: String) {
        if session != nil { log("stream", "stopped: \(reason)") }
        session?.stop()
        session = nil
        stats = nil
        statsFresh = false
        sessionStatus = nil
        testProgress = nil
        level = 0
        phase = pairing == nil ? .unpaired : .idle
        updateKeepAwake()
    }

    // MARK: Keep awake

    @ObservationIgnored private lazy var sleepGuard = SleepGuard { AppLog.shared.write("power", $0) }

    /// Idle sleep would cut the stream or strand a device on the PC.
    func updateKeepAwake() {
        guard live else { return }
        let streaming = session != nil
        let deckOnPC = handoffState.owner == .pc || handoffState.busy != nil
        let want = streaming || deckOnPC
        let why = [streaming ? "streaming to \(peerName)" : nil, deckOnPC ? "a USB device is on the PC" : nil]
            .compactMap { $0 }.joined(separator: " and ")
        sleepGuard.set(want, reason: want ? "Tightwire: \(why)" : "stopped streaming and devices are on the Mac")
        keepingAwake = sleepGuard.isHeld
    }

    // MARK: Latency

    func runLatencyTest() {
        guard let session else { return }
        testResult = nil
        tuning = nil
        log("latency", "latency test started (30 s)")
        session.startLatencyTest()
    }

    /// Applies the test's recommendation: receiver floor/cap now (persisted
    /// on the PC), and the packet size, which restarts the stream.
    func applyRecommendation() {
        guard let r = testResult?.recommendation else { return }
        log("latency", "applying: \(r.framesPerPacket) frames, floor \(r.floorMicros.map { "\($0) µs" } ?? "adaptive"), cap \(r.capMicros) µs")
        session?.applyTuning(floorMicros: r.floorMicros, capMicros: r.capMicros)
        tuning = .sending
        pendingPacketSize = r.framesPerPacket != framesPerPacket ? r.framesPerPacket : nil
    }
    /// Packet size to switch to once the receiver acknowledged its limits.
    @ObservationIgnored private var pendingPacketSize: Int?
    @ObservationIgnored private var lastSummary = Date.distantPast

    private func interrupted(_ msg: String) {
        stop(reason: "device interrupted: \(msg)")
        refreshDevices()
        fail(msg)
    }

    private func fail(_ msg: String) {
        lastError = msg
        phase = .error(msg)
        log("error", msg)
    }

    private func tick() {
        guard let s = session else { return }
        let st = s.status
        sessionStatus = st
        stats = st.stats
        testProgress = st.testProgress
        if let r = st.testResult, r != testResult {
            testResult = r
            log("latency", r.logLine)
        }
        if let t = st.tuning, t != tuning || pendingPacketSize != nil {
            if t != tuning { log("latency", "receiver buffer limits: \(t)") }
            tuning = t
            if case .applied = t, let frames = pendingPacketSize {
                pendingPacketSize = nil
                framesPerPacket = frames
                log("latency", "restarting the stream with \(frames)-frame packets")
                stop(reason: "packet size changed by the latency test")
                start()
                return
            }
        }
        if Date().timeIntervalSince(lastSummary) >= 10, st.phase == .streaming {
            lastSummary = Date()
            log("stats", Self.summaryLine(st))
        }
        statsFresh = st.stats != nil && (st.statsAge ?? 99) < 3
        // Peak-hold meter with ~300 ms release.
        level = max(st.peak, level * 0.7)
        let next: Phase = st.phase == .streaming ? .streaming : .searching
        if next != phase {
            log("stream", next == .streaming ? "\(peerName) is listening; sending audio" : "\(peerName) stopped answering; audio gated, probing")
            phase = next
        }
        if let learned = st.learnedAddress, var p = pairing, p.lastAddress != learned {
            let old = pcIPv4
            log("stream", "\(p.peerName) moved to \(learned) (verified: the PC signed this address)")
            p.lastAddress = learned
            pairing = p
            try? store?.save(p)
            if let old, old != pcIPv4 { forgetOldPeer(old) }
            restartHandoff()
        }
    }

    /// One line every 10 s while streaming, for reading drift over hours.
    static func summaryLine(_ st: SessionStatus) -> String {
        var parts: [String] = []
        if let s = st.stats {
            parts.append(String(format: "buffer %.2f ms", Double(s.depthMicros) / 1000))
            if let x = s.ext {
                parts.append(String(format: "target %.2f ms", Double(x.targetMicros) / 1000))
                parts.append(String(format: "jitter %.2f/%.2f ms", Double(x.jitterMeanMicros) / 1000, Double(x.jitterPeakMicros) / 1000))
            }
            parts.append("lost \(s.lost) late \(s.late) underruns \(s.underruns)")
            parts.append(String(format: "drift %+.1f ppm", s.ext.map { Double($0.driftPPB) / 1000 } ?? Double(s.driftPPM)))
        } else {
            parts.append("no receiver stats")
        }
        parts.append(st.rtt.map { String(format: "rtt %.2f ms (min %.2f)", $0.median, $0.min) } ?? "rtt n/a")
        return parts.joined(separator: " ")
    }

    /// The PC moved: its old address leaves Longcable's allow-list.
    private func forgetOldPeer(_ old: String) {
        guard longcable.isInstalled else { return }
        longcableQueue.async { [longcable] in
            guard let peers = try? longcable.status().allowedPeers, Longcable.listsExactly(old, in: peers) else { return }
            let r = Result { try longcable.forgetPeer(old) }
            Task { @MainActor in
                switch r {
                case .success: self.log("handoff", "Longcable no longer allows the PC's old address \(old)")
                case .failure(let e): self.log("error", "could not remove \(old) from Longcable: \(e)")
                }
                self.refreshLongcable()
            }
        }
    }

    /// Wide allow-list entries (subnet, tailnet, any) besides the PC itself.
    var wideAllowEntries: [String] { Longcable.wideEntries(allowedPeers) }

    /// "Allow only this PC": removes subnet/tailnet entries from Longcable's
    /// allow-list and lists the PC exactly.
    func narrowAllowList() {
        guard let pc = pcIPv4 else { allowListMessage = "The PC's address is not known yet"; return }
        allowListMessage = "Updating…"
        log("handoff", "narrowing Longcable's allowed peers to \(pc) (was \(allowedPeers.joined(separator: ", ")))")
        let done: @Sendable (Result<[String], Error>) -> Void = { r in
            Task { @MainActor in
                switch r {
                case .success(let peers): self.allowListMessage = "Only \(peers.joined(separator: ", ")) may connect."
                case .failure(let e): self.allowListMessage = "Could not narrow: \(e)"
                }
                self.refreshLongcable()
            }
        }
        if let handoff {
            handoff.narrowAllowList(forgetting: [], completion: done)
        } else {
            longcableQueue.async { [offline] in done(Result { try offline.narrowAllowList(to: pc) }) }
        }
    }

    // MARK: Pairing

    func beginPairing() {
        guard live else { return }
        flow = .browsing
        candidates = []
        log("pairing", "listening for PCs in pairing mode")
        do {
            listener = try BeaconListener()
        } catch {
            flow = .failed("Cannot listen for PCs: \(error)")
            log("error", "pairing listener: \(error)")
        }
        Task { @MainActor in
            while flow == .browsing, let l = listener {
                discovered = l.receivers
                candidates = discovered.map { PairingCandidate(id: $0.id, host: $0.host, address: $0.endpoint.host, conflict: $0.conflict) }
                try? await Task.sleep(for: .milliseconds(500))
            }
        }
    }

    func cancelPairing() {
        if flow != .none { log("pairing", "cancelled") }
        initiator?.cancel()
        verdict?.value = false
        listener?.stop()
        listener = nil
        initiator = nil
        flow = .none
    }

    func pair(with c: PairingCandidate) {
        guard let r = discovered.first(where: { $0.id == c.id }) else { return }
        listener?.stop()
        listener = nil
        flow = .connecting(r.host)
        log("pairing", "contacting \(r.host) at \(r.endpoint)")
        let selfID: [UInt8]
        do { selfID = try store?.selfID() ?? [] } catch { flow = .failed("\(error)"); return }
        let ini = PairingInitiator(target: r.endpoint, name: Host.current().localizedName ?? "Mac", selfID: selfID)
        initiator = ini
        let verdict = Locked<Bool?>(nil)
        self.verdict = verdict
        let host = r.host
        let audioHost = r.endpoint.host
        Task.detached {
            let result = Result { () throws -> Pairing in
                try ini.run { code, peer in
                    Task { @MainActor in
                        self.flow = .confirm(code: code, peer: peer)
                        self.log("pairing", "confirmation code ready for \(peer)")
                        // The owner may be looking at the PC: make the code impossible to miss.
                        self.showWindow(.receiver)
                        NSApp.requestUserAttention(.criticalRequest)
                    }
                    // Blocks the pairing thread until the user answers.
                    while verdict.value == nil { usleep(50_000) }
                    return verdict.value == true
                }
            }
            await MainActor.run { [weak self] in self?.finishPairing(result, host: host, audioHost: audioHost) }
        }
    }

    func answer(_ match: Bool) {
        log("pairing", match ? "owner confirmed the codes match" : "owner said the codes differ")
        verdict?.value = match
    }

    private func finishPairing(_ result: Result<Pairing, Error>, host: String, audioHost: String) {
        initiator = nil
        verdict = nil
        switch result {
        case .success(var p):
            // Stream to the receiver's audio port, not the pairing port.
            p.lastAddress = "\(audioHost):\(Wire.defaultPort)"
            // The authenticated name from the handshake, not the (unsigned)
            // beacon's: used to find the PC again by name.
            p.hostName = p.peerName
            do {
                stop(reason: "paired with a new PC")
                try store?.save(p)
                pairing = p
                phase = .idle
                flow = .none
                log("pairing", "paired with \(p.peerName) at \(p.lastAddress ?? "?")")
                restartHandoff()
                refreshSetup()
            } catch {
                flow = .failed("Could not save the pairing: \(error)")
                log("error", "saving pairing: \(error)")
            }
        case .failure(let e):
            flow = .failed("Pairing failed: \(e)")
            log("pairing", "failed: \(e)")
        }
    }

    func forget() {
        stop(reason: "pairing forgotten")
        try? store?.forget()
        log("pairing", "forgot \(pairing?.peerName ?? "pairing")")
        pairing = nil
        phase = .unpaired
        restartHandoff()
        refreshSetup()
    }

    // MARK: Handoff

    var longcableInstalled: Bool { live ? longcable.isInstalled : helperRunning }
    var handoffConfigured: Bool { !handoffDevices.isEmpty && pairing != nil && longcableInstalled }

    /// "On the Mac", "On the PC", "Moving to the PC…".
    var deckOwnerText: String {
        let st = handoffState
        if st.busy == .handOver { return "Moving to the PC…" }
        if st.busy == .handBack { return "Moving to the Mac…" }
        return st.owner == .pc ? "On the PC" : "On the Mac"
    }

    /// (Re)creates the handoff controller for the current pairing. The
    /// device list is read live, so ticking devices does not restart it.
    func restartHandoff() {
        guard live else { return }
        if handoff != nil { log("handoff", "restarting the handoff channel") }
        handoff?.shutdown()
        handoff = nil
        handoffState = HandoffState(mode: handoffMode)
        guard let pairing, longcable.isInstalled else { return }
        let selectedIDs = Set(handoffDevices.compactMap(\.busID))
        let stale = Set(addedBusIDs).subtracting(selectedIDs)
        do {
            let box = handoffSettings
            let c = try HandoffController(
                pairing: pairing, longcable: longcable, apps: WorkspaceApps(), helperBundleID: helperBundleID,
                mode: handoffMode, extraBusIDs: stale,
                onResolved: { d, id, loc in Task { @MainActor in self.learned(d, busID: id, location: loc) } }
            ) { (box.value, pairing) }
            c.onChange = { st in
                Task { @MainActor in
                    self.handoffState = st
                    self.handoffMode = st.mode
                    self.updateKeepAwake()
                }
            }
            handoff = c
        } catch {
            handoffState.lastError = "\(error)"
            log("error", "handoff: \(error)")
        }
    }

    private func learned(_ d: HandoffDevice, busID: String, location: String?) {
        guard let i = handoffDevices.firstIndex(where: { $0.sameDevice(d) }) else { return }
        handoffDevices[i].busID = busID
        if let location { handoffDevices[i].location = location }
        if !addedBusIDs.contains(busID) { addedBusIDs.append(busID) }
        refreshLongcable()
    }

    /// Selections saved by the first build have no BUSID/location; fill them
    /// in from Longcable so they show as ticked and match reliably.
    private func migrateSelection(_ s: Longcable.Status) {
        for (i, d) in handoffDevices.enumerated() where d.busID == nil {
            guard let t = Longcable.target(for: d, in: s), let id = t.id else { continue }
            handoffDevices[i].busID = id
            handoffDevices[i].location = t.location
            log("handoff", "\(d.name) is Longcable target \(id) at \(t.location ?? "?")")
        }
    }

    func handoffRequest(_ t: Wire.DeckTarget, source: String = "menu") {
        guard let handoff else {
            handoffState.lastError = handoffDevices.isEmpty ? "Choose a device first" : pairing == nil ? "Pair with a PC first" : "The handoff helper is not running"
            log("handoff", "request \(t) from \(source) ignored: \(handoffState.lastError ?? "")")
            return
        }
        handoff.request(t, source: source)
    }

    /// tightwire://deck/pc | mac | toggle. Off until the owner allows it.
    func handleURL(_ url: URL) {
        var decision = URLTrigger.decide(url, allowed: allowURLTriggers, alreadyAsked: urlTriggersAsked || !live)
        if case .ask(let t) = decision {
            urlTriggersAsked = true
            allowURLTriggers = Self.askURLTriggers(url)
            log("handoff", "owner \(allowURLTriggers ? "allowed" : "did not allow") tightwire:// URL triggers")
            decision = allowURLTriggers ? .act(t) : .ignore("the owner did not allow URL triggers")
        }
        switch decision {
        case .act(let t): handoffRequest(t, source: "URL")
        case .ignore(let why):
            log("handoff", "ignored \(url.absoluteString): \(why)")
        case .ask: break
        }
    }

    func refreshLongcable() {
        guard live else { return }
        longcableError = nil
        helperRunning = longcable.isInstalled
        guard helperRunning else { handoffRows = []; allowedPeers = []; return }
        let input = selectedDevice
        longcableQueue.async { [longcable] in
            let result = Result { try longcable.status() }
            let rows: [HandoffRow]
            switch result {
            case .success(let s): rows = Self.rows(from: s, inputUID: input?.uid ?? "", inputName: input?.name ?? "")
            case .failure: rows = []
            }
            Task { @MainActor in
                switch result {
                case .success(let s):
                    self.allowedPeers = s.allowedPeers ?? []
                    self.handoffRows = rows
                    self.migrateSelection(s)
                case .failure(let e):
                    self.longcableError = "\(e)"
                    self.log("error", "Longcable status: \(e)")
                }
            }
        }
    }

    /// Builds the device list. Runs off the main thread (IORegistry reads).
    nonisolated static func rows(from s: Longcable.Status, inputUID: String, inputName: String) -> [HandoffRow] {
        var rows: [HandoffRow] = []
        func make(_ d: HandoffDevice, registered: Bool, shared: Bool, state: String) -> HandoffRow {
            let roles = d.location.flatMap(USBRegistry.interfaces(location:)).map(USBDeviceRole.roles) ?? []
            let feeds = HandoffPolicy.feedsStream(vendor: d.vendor, serial: d.serial, location: d.location, roles: roles,
                                                  inputUID: inputUID, inputName: inputName)
            return HandoffRow(device: d, registered: registered, shared: shared, state: state, roles: roles,
                              verdict: HandoffPolicy.verdict(name: d.name, roles: roles, feedsStream: feeds),
                              suggested: HandoffPolicy.suggested(vendor: d.vendor, roles: roles))
        }
        for t in s.devices ?? [] {
            guard let model = t.model, let (v, p) = Self.split(model) else { continue }
            // Longcable's serial is a generated label until it reads the
            // descriptors; the IORegistry serial is filled in at selection.
            let label = String(format: "USB%04X%04X", v, p)
            let serial = (t.serial ?? "").hasPrefix(label) ? "" : t.serial ?? ""
            let d = HandoffDevice(vendor: v, product: p, serial: serial, name: t.name ?? model, location: t.location, busID: t.id)
            rows.append(make(d, registered: true, shared: t.enabled == true, state: t.state ?? ""))
        }
        for l in s.availableDevices ?? [] {
            guard let v = UInt16(l.vendor, radix: 16), let p = UInt16(l.product, radix: 16) else { continue }
            let d = HandoffDevice(vendor: v, product: p, serial: l.serial, name: l.name.isEmpty ? "\(l.vendor):\(l.product)" : l.name,
                                  location: l.location)
            rows.append(make(d, registered: false, shared: false, state: "Not registered"))
        }
        return rows.sorted { ($0.suggested ? 0 : 1, $0.device.name.lowercased()) < ($1.suggested ? 0 : 1, $1.device.name.lowercased()) }
    }

    nonisolated static func split(_ model: String) -> (UInt16, UInt16)? {
        let parts = model.split(separator: ":")
        guard parts.count == 2, let v = UInt16(parts[0], radix: 16), let p = UInt16(parts[1], radix: 16) else { return nil }
        return (v, p)
    }

    func selected(_ row: HandoffRow) -> HandoffDevice? {
        handoffDevices.first { sel in
            if let a = sel.busID, let b = row.device.busID { return a == b && sel.model == row.device.model }
            if let a = sel.location, let b = row.device.location { return a == b && sel.model == row.device.model }
            return sel.sameDevice(row.device)
        }
    }

    /// The checkbox. Refused devices never get selected; risky ones ask first.
    func setHandoff(_ row: HandoffRow, _ on: Bool) {
        rowMessages[row.id] = nil
        if !on {
            deselect(row)
            return
        }
        switch row.verdict {
        case .refuse(let why):
            rowMessages[row.id] = why
            log("handoff", "refused to select \(row.device.name) (\(row.device.model)): \(why)")
        case .warn:
            pendingWarning = row
        case .ok:
            select(row)
        }
    }

    func confirmWarning(_ accept: Bool) {
        guard let row = pendingWarning else { return }
        pendingWarning = nil
        if accept {
            log("handoff", "owner accepted the warning for \(row.device.name)")
            select(row)
        }
    }

    private func select(_ row: HandoffRow) {
        log("handoff", "selected \(row.device.name) (\(row.device.model) at \(row.device.location ?? "?"), \(row.registered ? row.device.busID ?? "" : "not registered"))")
        var d = row.device
        if row.registered {
            // Fill in the hardware serial Longcable may not show yet.
            if d.serial.isEmpty, let prior = handoffDevices.first(where: { $0.sameDevice(d) }) { d.serial = prior.serial }
            handoffDevices.removeAll { $0.sameDevice(d) || ($0.busID == nil && $0.model == d.model) }
            handoffDevices.append(d)
            // Selecting must not share it: only a handover does.
            if row.shared, handoffState.owner == .mac {
                let device = d
                longcableQueue.async { [offline] in _ = offline.handBack([device], notifyPC: false) }
            }
            refreshLongcable()
            return
        }
        guard let location = d.location else { return }
        let local = Longcable.LocalDevice(vendor: String(format: "%04x", d.vendor), product: String(format: "%04x", d.product),
                                          location: location, name: d.name, serial: d.serial)
        registering.insert(row.id)
        let device = d
        let done: @Sendable (Result<Longcable.Device, Error>) -> Void = { result in
            Task { @MainActor in self.registered(row, device, result) }
        }
        if let handoff {
            handoff.register(local, completion: done)
        } else {
            longcableQueue.async { [offline] in done(Result { try offline.register(local) }) }
        }
    }

    private func registered(_ row: HandoffRow, _ d: HandoffDevice, _ result: Result<Longcable.Device, Error>) {
        registering.remove(row.id)
        switch result {
        case .success(let t):
            var d = d
            d.busID = t.id
            d.location = t.location ?? d.location
            handoffDevices.removeAll { $0.sameDevice(d) }
            handoffDevices.append(d)
            if let id = t.id, !addedBusIDs.contains(id) { addedBusIDs.append(id) }
        case .failure(let e):
            rowMessages[row.id] = "Could not register: \(e)"
            log("error", "registering \(d.name): \(e)")
        }
        refreshLongcable()
    }

    private func deselect(_ row: HandoffRow) {
        guard let d = selected(row) else { return }
        log("handoff", "deselected \(d.name) (\(d.model) \(d.busID ?? ""))")
        handoffDevices.removeAll { $0 == d }
        if let handoff {
            handoff.deselect(d)
        } else {
            longcableQueue.async { [offline] in offline.deselect(d) }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in self?.refreshLongcable() }
    }

    func setQuitHelper(_ d: HandoffDevice, _ on: Bool) {
        guard let i = handoffDevices.firstIndex(of: d) else { return }
        handoffDevices[i].quitHelperApp = on
    }

    // MARK: Setup (SPEC §10)

    func refreshSetup() {
        guard live else { return }
        setup = SetupStatus.detect(paired: pairing != nil)
    }

    func requestMicrophone() {
        if setup.microphone == .notDetermined {
            Task {
                _ = await AVCaptureDevice.requestAccess(for: .audio)
                refreshSetup()
            }
        } else {
            NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")!)
        }
    }

    /// Only once paired: the installer allows exactly the PC's IPv4, never
    /// Longcable's subnet/tailnet default.
    func installHelper() {
        guard let peer = pcIPv4 else {
            setupMessage = "Pair with the PC first: the helper only admits the paired PC."
            return
        }
        setupMessage = "Waiting for your administrator password…"
        log("setup", "installing the handoff helper (allow only \(peer))")
        // Let the window redraw before the modal password prompt.
        DispatchQueue.main.async { [self] in
            MainActor.assumeIsolated {
                switch HelperInstaller.install(peer: peer) {
                case .success:
                    setupMessage = "Helper installed."
                    log("setup", "helper installed")
                case .failure(let e):
                    setupMessage = "Install failed: \(e)"
                    log("error", "helper install: \(e)")
                }
                refreshSetup()
                restartHandoff()
                refreshLongcable()
            }
        }
    }

    func uninstallHelper() {
        // Give devices back before the service goes away.
        handoff?.shutdown()
        handoff = nil
        setupMessage = "Waiting for your administrator password…"
        log("setup", "uninstalling the handoff helper")
        DispatchQueue.main.async { [self] in
            MainActor.assumeIsolated {
                switch HelperInstaller.uninstall() {
                case .success:
                    setupMessage = "Helper removed."
                    log("setup", "helper removed")
                case .failure(let e):
                    setupMessage = "Uninstall failed: \(e)"
                    log("error", "helper uninstall: \(e)")
                }
                refreshSetup()
                refreshLongcable()
            }
        }
    }

    func setLaunchAtLogin(_ on: Bool) {
        do {
            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            log("setup", "open at login \(on ? "on" : "off")")
        } catch {
            setupMessage = "Open at login: \(error.localizedDescription)"
            log("error", "open at login: \(error)")
        }
        refreshSetup()
        if setup.launchAtLogin == .requiresApproval { SMAppService.openSystemSettingsLoginItems() }
    }

    /// App is quitting: give devices back and stop streaming.
    func shutdown() {
        guard live else { return }
        log("app", "quitting")
        handoff?.shutdown()
        handoff = nil
        stop(reason: "app quit")
    }
}

extension AppModel {
    /// One-time consent the first time a tightwire:// URL arrives.
    static func askURLTriggers(_ url: URL) -> Bool {
        NSApp.activate(ignoringOtherApps: true)
        let a = NSAlert()
        a.messageText = "Allow Stream Deck URL buttons?"
        a.informativeText = "Something opened \(url.absoluteString). URL buttons let any app or web page move your handoff devices between the Mac and the PC. You can change this later in Tightwire › Stream Deck."
        a.addButton(withTitle: "Allow")
        a.addButton(withTitle: "Don’t Allow")
        return a.runModal() == .alertFirstButtonReturn
    }
}

/// One line of the Stream pane's latency breakdown.
struct LatencyTerm: Identifiable, Equatable {
    enum Kind: String { case measured = "measured", reported = "reported", fixed = "fixed", estimate = "estimate" }
    let label: String
    let detail: String
    let millis: Double?
    let kind: Kind
    var id: String { label }
}

extension AppModel {
    /// Mic → PC output device, term by term. What each number is (measured,
    /// reported by a driver, fixed by configuration, or an estimate) is part
    /// of the line, because the total is a sum of unlike things.
    var latencyTerms: [LatencyTerm] {
        guard let st = sessionStatus else { return [] }
        let rate = Double(max(st.stream.rate, 1))
        var t: [LatencyTerm] = []
        t.append(.init(label: "Capture device", detail: "input latency + safety offset, as the driver reports them",
                       millis: Double(st.stream.deviceLatencyFrames) / rate * 1000, kind: .reported))
        t.append(.init(label: "Capture buffer", detail: "\(st.stream.bufferFrames) frames; each \(st.stream.packetFrames)-frame packet leaves when its buffer is full",
                       millis: Double(st.stream.bufferFrames) / rate * 1000, kind: .fixed))
        t.append(.init(label: "Network", detail: st.rtt.map { String(format: "half the round trip (median of %d, min %.2f ms)", $0.count, $0.min) } ?? "waiting for probe answers",
                       millis: st.rtt.map { $0.median / 2 }, kind: .measured))
        let x = st.stats?.ext
        t.append(.init(label: "Jitter buffer", detail: x.map { String(format: "live depth on the PC; target %.2f ms", Double($0.targetMicros) / 1000) } ?? "live depth on the PC",
                       millis: st.stats.map { Double($0.depthMicros) / 1000 }, kind: .measured))
        let streamRate = Double(x?.streamRate ?? st.stream.rate)
        t.append(.init(label: "Resampler", detail: "drift-correction filter delay (24 frames)",
                       millis: streamRate > 0 ? 24 / streamRate * 1000 : nil, kind: .fixed))
        t.append(.init(label: "Output period", detail: x.map { "\($0.outputMode.name), \($0.outputPeriodFrames) frames at \($0.outputRate) Hz; the device may queue more" } ?? "needs Tightwire 0.2.0 on the PC",
                       millis: x.map(\.outputPeriodMillis), kind: .estimate))
        return t
    }

    /// Sum of the terms, nil until every term is known.
    var latencyTotal: Double? {
        let ms = latencyTerms.map(\.millis)
        guard !ms.isEmpty, ms.allSatisfy({ $0 != nil }) else { return nil }
        return ms.compactMap { $0 }.reduce(0, +)
    }
}
