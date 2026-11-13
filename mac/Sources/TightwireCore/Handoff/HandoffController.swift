import AppKit
import Foundation

/// NSWorkspace-backed helper-app control.
public struct WorkspaceApps: HelperAppControlling {
    public init() {}

    public func isRunning(_ bundleID: String) -> Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).isEmpty
    }

    public func quit(_ bundleID: String, timeout: TimeInterval) -> Bool {
        let apps = NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
        apps.forEach { _ = $0.terminate() }  // a normal Quit, not a kill
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if apps.allSatisfy(\.isTerminated) { return true }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return apps.allSatisfy(\.isTerminated)
    }

    public func launch(_ bundleID: String) {
        guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) else { return }
        let cfg = NSWorkspace.OpenConfiguration()
        cfg.activates = false
        NSWorkspace.shared.openApplication(at: url, configuration: cfg) { _, _ in }
    }
}

/// Snapshot for the UI.
public struct HandoffState: Sendable, Equatable {
    public var owner: HandoffMachine.Owner = .mac
    public var mode: HandoffMachine.Mode = .auto
    public var busy: HandoffMachine.Command?
    public var receiverAlive = false
    /// The PC's protocol revision (0 = Tightwire before 0.2.0).
    public var receiverRevision: UInt8?
    public var lastError: String?
    public var log: [String] = []
    public init(owner: HandoffMachine.Owner = .mac, mode: HandoffMachine.Mode = .auto, busy: HandoffMachine.Command? = nil,
                receiverAlive: Bool = false, lastError: String? = nil, log: [String] = []) {
        self.owner = owner
        self.mode = mode
        self.busy = busy
        self.receiverAlive = receiverAlive
        self.lastError = lastError
        self.log = log
    }
}

/// Rate-limits log lines about packets anyone on the LAN can send.
public struct DropLog: Sendable {
    public static let every: TimeInterval = 5
    private var last: Date?
    private var suppressed = 0
    public init() {}

    /// The line to log now, or nil while within the interval (counted).
    public mutating func note(_ what: String, at now: Date) -> String? {
        if let l = last, now.timeIntervalSince(l) < Self.every {
            suppressed += 1
            return nil
        }
        last = now
        defer { suppressed = 0 }
        return "dropped \(what)" + (suppressed > 0 ? " (+\(suppressed) similar since the last report)" : "")
    }
}

/// Runs the handoff channel (SPEC §9): its own UDP socket on 4813 with
/// probe/hello liveness, offers/releases/acks, deck requests from the PC,
/// and the ownership state machine. Sequences run on one serial queue.
public final class HandoffController: @unchecked Sendable {
    public typealias Settings = (devices: [HandoffDevice], pairing: Pairing?)

    private let socket: UDPSocket
    private let keys: SessionKeys
    fileprivate var target: Endpoint
    private let settings: @Sendable () -> Settings
    private let liveness = Locked(Liveness())
    private let machine: Locked<HandoffMachine>
    private let state = Locked(HandoffState())
    private let acks = Locked([UInt32: [Wire.HandoffItem]]())
    private let seenDeckIDs = Locked([UInt32: Date]())
    /// Rate limit for logging unauthenticated packets on UDP 4813 (anyone on
    /// the LAN can send them): one line per 5 s with a count of the rest.
    private let dropLog = Locked(DropLog())
    private let running = Locked(true)
    /// When `waitingForDevice` last looked for a plugged-in device.
    private let lastDeviceCheck = Locked<TimeInterval>(0)
    static let deviceCheckInterval: TimeInterval = 3
    private let queue = DispatchQueue(label: "tightwire.handoff")
    private let clock0 = Date()
    private(set) var sequencer: HandoffSequencer!
    public var onChange: (@Sendable (HandoffState) -> Void)?
    private let logSink: @Sendable (String) -> Void

    /// `mode`: the persisted ownership mode, so a restart keeps "Back to Mac".
    /// `extraBusIDs`: Longcable targets Tightwire registered earlier that are
    /// no longer selected; start-up unshares them along with the selection.
    public init(pairing: Pairing, longcable: LongcableControlling, apps: HelperAppControlling,
                helperBundleID: String, mode: HandoffMachine.Mode = .auto, extraBusIDs: Set<String> = [],
                onResolved: (@Sendable (HandoffDevice, String, String?) -> Void)? = nil,
                logSink: @escaping @Sendable (String) -> Void = { AppLog.shared.write("handoff", $0) },
                localPort: UInt16 = Wire.handoffPort,
                settings: @escaping @Sendable () -> Settings) throws {
        keys = pairing.keys
        self.logSink = logSink
        self.settings = settings
        machine = Locked(HandoffMachine(mode: mode))
        // The receiver's audio port: it answers probes and handoff messages there.
        guard let ep = pairing.lastAddress.flatMap({ try? Endpoint.resolve($0, defaultPort: Wire.defaultPort) })
            ?? pairing.hostName.flatMap({ try? Session.resolveHost($0) })
        else { throw StoreError(description: "The PC's address is not known yet") }
        target = ep
        socket = try UDPSocket(family: target.family, preferredPort: localPort, fallbackToEphemeral: true)
        sequencer = HandoffSequencer(longcable: longcable, apps: apps, transport: Transport(owner: self),
                                     helperBundleID: helperBundleID) { [weak self] in self?.log($0) }
        sequencer.onResolved = onResolved
        log("started: PC \(target.host), mode \(mode.rawValue), devices \(settings().devices.map(\.name).joined(separator: ", "))")
        let t = Thread { [weak self] in self?.loop() }
        t.name = "tightwire-handoff"
        t.start()
        // Start from a known state: nothing shared while the Mac owns the devices.
        queue.async { [self] in
            let still = sequencer.handBack(settings().devices, notifyPC: false, extraBusIDs: extraBusIDs)
            if !still.isEmpty { state.update { $0.lastError = "Longcable still shares \(still.joined(separator: ", "))" }; publish() }
        }
    }

    /// The owner unticked a device: release and unshare it if needed.
    public func deselect(_ d: HandoffDevice) {
        queue.async { [self] in sequencer.deselect(d) }
    }

    /// Registers a connected device with Longcable, unshared (see
    /// `HandoffSequencer.register`). Runs on the handoff queue.
    public func register(_ local: Longcable.LocalDevice, completion: @escaping @Sendable (Result<Longcable.Device, Error>) -> Void) {
        queue.async { [self] in completion(Result { try sequencer.register(local) }) }
    }

    private var now: TimeInterval { Date().timeIntervalSince(clock0) }

    public var snapshot: HandoffState { state.value }

    /// The UDP port actually bound (4813 unless taken).
    public var localPort: UInt16 { socket.localPort }

    func log(_ s: String) {
        let line = "\(Self.timeFormatter.string(from: Date())) \(s)"
        logSink(s)
        state.update {
            $0.log.append(line)
            if $0.log.count > 200 { $0.log.removeFirst($0.log.count - 200) }
        }
        publish()
    }

    static let timeFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss"
        return f
    }()

    private func publish() {
        let m = machine.value
        let s = state.update { st -> HandoffState in
            st.owner = m.owner
            st.mode = m.mode
            st.busy = m.running
            return st
        }
        onChange?(s)
    }

    // MARK: Commands

    public func request(_ target: Wire.DeckTarget, source: String) {
        log("request \(target) from \(source)")
        let alive = liveness.value.isLive(at: now)
        if let cmd = machine.update({ $0.request(target, alive: alive, now: now) }) { run(cmd) }
        if target != .mac, !alive { log("PC is not answering; devices will move when it does") }
        publish()
    }

    /// "Allow only this PC" (Stream Deck pane), on the handoff queue.
    public func narrowAllowList(forgetting old: [String], completion: @escaping @Sendable (Result<[String], Error>) -> Void) {
        let pc = target.host
        queue.async { [self] in completion(Result { try sequencer.narrowAllowList(to: pc, forgetting: old) }) }
    }

    /// Synchronous reclaim for app quit (short timeouts).
    public func shutdown() {
        running.value = false
        queue.sync {
            sequencer.timeouts.release = 1
            sequencer.handBack(settings().devices, notifyPC: true)
        }
    }

    private func run(_ cmd: HandoffMachine.Command) {
        let devices = settings().devices
        let pcIP = target.host
        publish()
        queue.async { [self] in
            var ok = true
            var nothing = false
            switch cmd {
            case .handOver:
                do {
                    let ids = try sequencer.handOver(devices, pcAddress: pcIP)
                    log("devices on the PC: \(ids.joined(separator: ", "))")
                    state.update { $0.lastError = nil }
                } catch let e as HandoffSequencer.NothingConnected {
                    nothing = true
                    log("\(e); waiting until one is plugged in (or Send to PC) instead of retrying")
                    state.update { $0.lastError = "\(e): waiting for it to be plugged in" }
                } catch {
                    ok = false
                    state.update { $0.lastError = "\(error)" }
                }
            case .handBack:
                sequencer.handBack(devices, notifyPC: true)
                log("devices back on the Mac")
            }
            if nothing {
                machine.update { $0.finishedNothingConnected() }
                lastDeviceCheck.value = now
            } else {
                machine.update { $0.finished(cmd, success: ok, now: now) }
            }
            publish()
            // Re-evaluate: a request may have arrived meanwhile.
            let alive = liveness.value.isLive(at: now)
            if let next = machine.update({ $0.update(alive: alive, now: now) }) { run(next) }
        }
    }

    // MARK: Network

    private func loop() {
        var lastAliveFlag = false
        while running.value {
            let t = now
            if liveness.value.shouldProbe(at: t) {
                let nonce = secureRandomU64() | 1
                liveness.update { $0.probeSent(nonce: nonce, at: t) }
                socket.send(Wire.seal(Wire.encode(.probe(nonce: nonce)), key: keys.m2r), to: target)
            }
            if let (data, from) = socket.receive(timeout: 0.2) { handle(data, from: from) }
            let alive = liveness.value.isLive(at: now)
            if alive != lastAliveFlag {
                lastAliveFlag = alive
                state.update { $0.receiverAlive = alive }
                log(alive ? "PC is reachable" : "PC stopped answering")
            }
            let devices = settings().devices
            guard !devices.isEmpty else { continue }
            if machine.value.waitingForDevice, machine.value.running == nil,
               now - lastDeviceCheck.value >= Self.deviceCheckInterval {
                // A status read only: cheap, shares nothing, takes no assertion.
                lastDeviceCheck.value = now
                if sequencer.anyConnected(devices) {
                    log("a handoff device is connected again")
                    state.update { $0.lastError = nil }
                    if let cmd = machine.update({ $0.devicesAppeared(alive: alive, now: now) }) { run(cmd) }
                    publish()
                }
            }
            if let cmd = machine.update({ $0.update(alive: alive, now: now) }) { run(cmd) }
        }
    }

    private func handle(_ data: [UInt8], from: Endpoint) {
        guard let packet = try? Wire.decode(data) else { return }
        switch packet {
        case .hello(let nonce, let echo, let ext):
            guard from.host == target.host, Wire.verify(data, key: keys.r2m) else { return }
            liveness.update { $0.hello(sessionNonce: nonce, echo: echo, at: now) }
            let rev = ext?.revision ?? 0
            if state.value.receiverRevision != rev {
                state.update { $0.receiverRevision = rev }
                if rev == 0 { log("the PC runs Tightwire before 0.2.0: update Tightwire on the PC") }
            }
        case .handoffAck(let id, _, let items):
            guard from.host == target.host, Wire.verify(data, key: keys.r2m) else { return }
            acks.update { $0[id] = items }
        case .deckRequest(let id, let ms, let t):
            // Tag proves the paired PC; time window + id cache stop replays.
            guard Wire.verify(data, key: keys.r2m) else {
                if let line = dropLog.update({ $0.note("unauthenticated deck request from \(from)", at: Date()) }) { log(line) }
                return
            }
            let skew = abs(Date().timeIntervalSince1970 - Double(ms) / 1000)
            let fresh = seenDeckIDs.update { seen -> Bool in
                seen = seen.filter { Date().timeIntervalSince($0.value) < 120 }
                guard seen[id] == nil else { return false }
                seen[id] = Date()
                return true
            }
            var accepted = false
            var message: String
            if skew > 30 {
                message = "request clock is \(Int(skew)) s off; check both clocks"
            } else if !fresh {
                message = "duplicate request ignored"
                accepted = true
            } else {
                request(t, source: "PC (\(from.host))")
                accepted = true
                message = t == .mac ? "Moving devices to the Mac" : t == .pc ? "Moving devices to the PC" : "Toggling devices"
            }
            let m = machine.value
            let owner = (m.running == .handOver) || (m.owner == .pc && m.running != .handBack)
            let ack = Wire.encode(.deckAck(requestID: id, accepted: accepted, ownerIsPC: owner, message: message))
            socket.send(Wire.seal(ack, key: keys.m2r), to: from)
        default:
            break
        }
    }

    /// Sends a control packet until its ack arrives or `timeout` passes.
    fileprivate func exchange(_ body: (UInt32) -> Wire.Packet, timeout: TimeInterval) throws -> [Wire.HandoffItem] {
        let id = UInt32.random(in: 1...UInt32.max)
        let nonce = liveness.value.sessionNonce
        let packet = Wire.seal(Wire.encode(body(id)), key: keys.m2r, nonce: nonce)
        let deadline = Date().addingTimeInterval(timeout)
        var lastSend = Date.distantPast
        while Date() < deadline {
            if let items = acks.update({ $0.removeValue(forKey: id) }) { return items }
            if Date().timeIntervalSince(lastSend) >= 1 {
                socket.send(packet, to: target)
                lastSend = Date()
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw StoreError(description: "no answer from the PC within \(Int(timeout)) s")
    }

    private struct Transport: HandoffTransport {
        weak var owner: HandoffController?
        func offer(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
            guard let o = owner else { throw StoreError(description: "stopped") }
            // The Mac's own IPv4 goes inside the tagged offer: the receiver
            // (0.2.0+) attaches only from that address, and only when it is
            // also where the offer came from.
            guard let host = o.target.localIPv4Toward else {
                throw StoreError(description: "this Mac has no IPv4 route to the PC (\(o.target.host))")
            }
            return try o.exchange({ .handoffOffer(requestID: $0, usbipPort: Longcable.usbipPort, host: host, busIDs: busIDs) },
                                  timeout: timeout)
        }
        func release(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
            guard let o = owner else { throw StoreError(description: "stopped") }
            return try o.exchange({ .handoffRelease(requestID: $0, busIDs: busIDs) }, timeout: timeout)
        }
    }
}
