import Darwin
import Foundation
import TightwireRT

/// Handle to the C packetizer. Its process functions are designed to be
/// called from one producer thread at a time while other threads only touch
/// atomics, which is why sharing the pointer across threads is sound.
public struct RTSender: @unchecked Sendable {
    public let ptr: OpaquePointer
}

/// Where audio comes from. Implementations push frames into the C packetizer.
public protocol AudioSource: AnyObject, Sendable {
    var sampleRate: UInt32 { get }
    /// Channels the source delivers (device channels for capture).
    var channelCount: Int { get }
    /// Frames per IO callback (each packet leaves when its buffer is full).
    var bufferFrames: UInt32 { get }
    /// Device-reported input latency + safety offset (frames), an estimate.
    var deviceLatencyFrames: UInt32 { get }
    func start(sender: RTSender) throws
    func stop()
}

public enum ChannelSelection: Equatable, Sendable, Codable {
    /// Mono from this device channel (0-based).
    case mono(Int)
    /// All device channels (up to 8).
    case all
}

public struct SessionConfig: Sendable {
    public var pairing: Pairing
    /// "host:port" override; defaults to the pairing's last address.
    public var address: String?
    public var framesPerPacket: UInt32
    public var channels: ChannelSelection

    public init(pairing: Pairing, address: String? = nil, framesPerPacket: UInt32 = 48, channels: ChannelSelection = .mono(0)) {
        self.pairing = pairing
        self.address = address
        self.framesPerPacket = framesPerPacket
        self.channels = channels
    }
}

/// Result of a buffer-limits request (type 21/22).
public enum TuningState: Equatable, Sendable {
    case sending
    case applied(floorMicros: UInt32, capMicros: UInt32)
    case rejected(String)
    case noAnswer
}

/// Snapshot for the UI and CLI.
public struct SessionStatus: Sendable {
    public enum Phase: Sendable, Equatable { case searching, streaming }
    public var phase: Phase = .searching
    public var receiver: String?
    public var packetsSent: UInt64 = 0
    public var packetsGated: UInt64 = 0
    public var sendErrors: UInt64 = 0
    public var peak: Float = 0
    public var stats: Wire.Stats?
    public var statsAge: TimeInterval?
    public var lastError: String?
    /// The receiver's address moved (verified: the receiver signed that it
    /// is at the datagram's source address); callers may persist it.
    public var learnedAddress: String?
    /// Round-trip times of the last 30 probes (ms).
    public var rtt: Distribution?
    /// Receiver protocol revision from its hello: nil = no hello yet,
    /// 0 = Tightwire before 0.2.0 (update it).
    public var receiverRevision: UInt8?
    public var stream: (rate: UInt32, channels: Int, bufferFrames: UInt32, packetFrames: UInt32, deviceLatencyFrames: UInt32) = (0, 0, 0, 0, 0)
    /// Running latency test: 0…1.
    public var testProgress: Double?
    public var testResult: LatencyTestResult?
    public var tuning: TuningState?
    /// Latest marker measurement (latency test only).
    public var lastMarker: MarkerMeasurement?

    public init() {}
}

/// One streaming run: socket, real-time packetizer, liveness control thread
/// and an audio source. Stream id is fresh per session (SPEC §3).
public final class Session: @unchecked Sendable {
    public let config: SessionConfig
    private let source: AudioSource
    private let socket: UDPSocket
    private let sender: OpaquePointer
    private let keys: SessionKeys
    private let wireChannels: Int
    /// Packets sent in one liveness window, for the stats freshness check.
    private let packetsPerWindow: UInt32
    private let state = Locked(SessionStatus())
    private let liveness = Locked(Liveness())
    private let running = Locked(true)
    private var thread: Thread?
    private let clock0 = Date()
    /// Control requests from other threads, handled on the control thread.
    private let requests = Locked<[Request]>([])

    enum Request {
        case startTest
        case tuning(floorMicros: UInt32?, capMicros: UInt32)
    }

    /// Probe interval for RTT while live: once a second, 5/s during a test.
    static let rttInterval: TimeInterval = 1
    static let testRTTInterval: TimeInterval = 0.2

    public init(config: SessionConfig, source: AudioSource) throws {
        self.config = config
        self.source = source
        let keys = config.pairing.keys
        self.keys = keys
        guard let code = Wire.sampleRates.firstIndex(of: source.sampleRate) else {
            throw StoreError(description: "unsupported sample rate \(source.sampleRate) Hz (need 44.1/48/88.2/96 kHz)")
        }
        let target = try Session.resolveTarget(config)
        socket = try UDPSocket(family: target.family, preferredPort: Wire.senderPort)
        try socket.connect(target)
        var c = tw_sender_config()
        c.fd = socket.fd
        c.stream_id = UInt16.random(in: 1...UInt16.max)
        c.rate_code = UInt8(code)
        switch config.channels {
        case .mono(let ch):
            guard ch < source.channelCount else { throw StoreError(description: "channel \(ch + 1) not available") }
            c.channels = 1
            c.source_channel = Int32(ch)
        case .all:
            c.channels = UInt8(min(source.channelCount, Wire.maxChannels))
            c.source_channel = -1
        }
        wireChannels = Int(c.channels)
        c.frames_per_packet = config.framesPerPacket
        withUnsafeMutableBytes(of: &c.key) { $0.copyBytes(from: keys.m2r) }
        guard let s = tw_sender_create(&c) else { throw StoreError(description: "could not create sender") }
        sender = s
        packetsPerWindow = UInt32(Liveness.window * Double(source.sampleRate) / Double(tw_sender_frames_per_packet(s))) + 8
        state.update { $0.receiver = target.description }
        do {
            try source.start(sender: RTSender(ptr: s))
        } catch {
            tw_sender_destroy(s)
            throw error
        }
        let packet = tw_sender_frames_per_packet(s)
        let channels = wireChannels
        state.update {
            $0.stream = (source.sampleRate, channels, source.bufferFrames, min(packet, max(source.bufferFrames, 1)),
                         source.deviceLatencyFrames)
        }
        let t = Thread { [weak self] in self?.controlLoop(target: target) }
        t.name = "tightwire-control"
        t.qualityOfService = .userInitiated
        thread = t
        t.start()
    }

    static func resolveTarget(_ config: SessionConfig) throws -> Endpoint {
        if let a = config.address ?? config.pairing.lastAddress {
            return try Endpoint.resolve(a, defaultPort: Wire.defaultPort)
        }
        if let h = config.pairing.hostName { return try resolveHost(h) }
        throw StoreError(description: "no address for \(config.pairing.peerName)")
    }

    static func resolveHost(_ name: String) throws -> Endpoint {
        do { return try Endpoint.resolve("\(name).local", defaultPort: Wire.defaultPort) } catch {}
        return try Endpoint.resolve(name, defaultPort: Wire.defaultPort)
    }

    public var status: SessionStatus {
        var s = state.value
        var c = tw_sender_counters()
        tw_sender_read_counters(sender, &c)
        s.packetsSent = c.packets_sent
        s.packetsGated = c.packets_gated
        s.sendErrors = c.send_errors
        s.peak = c.peak
        return s
    }

    /// Starts a 30 s latency test (RTT, receiver stats and, with a 0.2.0
    /// receiver, latency markers). The result lands in `status.testResult`.
    public func startLatencyTest() { requests.update { $0.append(.startTest) } }

    /// Sets the receiver's jitter-buffer floor (nil = adaptive) and cap; the
    /// receiver persists them. Result in `status.tuning`.
    public func applyTuning(floorMicros: UInt32?, capMicros: UInt32) {
        requests.update { $0.append(.tuning(floorMicros: floorMicros, capMicros: capMicros)) }
    }

    private var now: TimeInterval { Date().timeIntervalSince(clock0) }

    // Control-thread state.
    private var probeSentAt: [UInt64: UInt64] = [:]
    private var rttWindow: [Double] = []
    private var clockSync = ClockSync()
    private var test: LatencyTest?
    private var nextMarkerAt: UInt64 = 0
    private var lastMarkerSeq: UInt32 = 0
    private var pendingMarkers: [UInt64: tw_marker] = [:]
    private var lastMarkerReported: UInt64 = 0
    private var tuningRequest: (id: UInt32, body: [UInt8], sends: Int, nextAt: TimeInterval)?
    /// A re-resolved address being tried while gated; adopted only after a
    /// verified hello from it (SPEC §3a, security).
    private var candidate: (endpoint: Endpoint, until: TimeInterval)?

    private func controlLoop(target initial: Endpoint) {
        var target = initial
        var lastStats: TimeInterval?
        var lastResolve = now
        var lastRTTProbe: TimeInterval = 0
        while running.value {
            let t = now
            handleRequests(at: t)
            let isLive = liveness.value.isLive(at: t)
            let rttDue = isLive && t - lastRTTProbe >= (test != nil ? Self.testRTTInterval : Self.rttInterval)
            if liveness.value.shouldProbe(at: t) || rttDue {
                // Receiver silent for a while and we know its name: try the
                // name's current address, but only adopt it once a hello from
                // there proves (signed) that the receiver really is there.
                if !isLive, candidate == nil, let host = config.pairing.hostName, t - lastResolve > 5 {
                    lastResolve = t
                    if let ep = try? Session.resolveHost(host), ep != target, (try? socket.connect(ep)) != nil {
                        candidate = (ep, t + 3)
                    }
                }
                let nonce = secureRandomU64() | 1
                liveness.update { $0.probeSent(nonce: nonce, at: t) }
                probeSentAt[nonce] = monotonicNanos()
                probeSentAt = probeSentAt.filter { monotonicNanos() &- $0.value < 3_000_000_000 }
                socket.send(Wire.seal(Wire.encode(.probe(nonce: nonce)), key: keys.m2r))
                lastRTTProbe = t
            }
            if let c = candidate, t > c.until {
                // No verified answer from the new address: back to the known one.
                candidate = nil
                try? socket.connect(target)
            }
            if let (data, from) = socket.receive(timeout: 0.05) {
                let t4 = monotonicNanos()
                if handle(data, from: from, receivedAt: t4, lastStats: &lastStats), let c = candidate, from == c.endpoint {
                    target = c.endpoint
                    candidate = nil
                    state.update { $0.receiver = target.description }
                }
            }
            pollMarker()
            sendTuningIfDue(at: now)
            finishTestIfDue()
            let live = liveness.value.isLive(at: now)
            tw_sender_set_live(sender, live, liveness.value.sessionNonce)
            state.update {
                $0.phase = live ? .streaming : .searching
                $0.statsAge = lastStats.map { now - $0 }
                if let test = self.test { $0.testProgress = test.progress(at: monotonicNanos()) }
            }
        }
        tw_sender_set_live(sender, false, 0)
    }

    private func handleRequests(at t: TimeInterval) {
        let pending = requests.value
        guard !pending.isEmpty else { return }
        requests.value = []
        for r in pending {
            switch r {
            case .startTest:
                test = LatencyTest(startedAt: monotonicNanos())
                nextMarkerAt = 0
                pendingMarkers = [:]
                state.update { $0.testResult = nil; $0.testProgress = 0; $0.lastMarker = nil }
            case .tuning(let floor, let cap):
                let id = UInt32.random(in: 1...UInt32.max)
                let body = Wire.encode(.tuningSet(requestID: id, adaptive: floor == nil, minTargetMicros: floor ?? 0,
                                                  maxTargetMicros: cap))
                tuningRequest = (id, body, 0, t)
                state.update { $0.tuning = .sending }
            }
        }
    }

    /// Resends the tuning request every 0.5 s (3 tries) until acked.
    private func sendTuningIfDue(at t: TimeInterval) {
        guard var r = tuningRequest, t >= r.nextAt else { return }
        let nonce = liveness.value.sessionNonce
        guard r.sends < 3, nonce != 0 else {
            if r.sends >= 3 {
                tuningRequest = nil
                state.update { $0.tuning = .noAnswer }
            }
            return
        }
        socket.send(Wire.seal(r.body, key: keys.m2r, nonce: nonce))
        r.sends += 1
        r.nextAt = t + 0.5
        tuningRequest = r
    }

    /// During a test with a 0.2.0 receiver: one marker per second; remembers
    /// what the packetizer stamped so the receiver's report can be matched.
    private func pollMarker() {
        guard test != nil, state.value.receiverRevision ?? 0 >= 1 else { return }
        var m = tw_marker()
        tw_sender_read_marker(sender, &m)
        if m.seq != lastMarkerSeq {
            lastMarkerSeq = m.seq
            pendingMarkers[m.timestamp] = m
            if pendingMarkers.count > 16, let oldest = pendingMarkers.keys.min() { pendingMarkers[oldest] = nil }
        }
        let n = monotonicNanos()
        if n >= nextMarkerAt, liveness.value.isLive(at: now) {
            tw_sender_request_marker(sender)
            nextMarkerAt = n + 1_000_000_000
        }
    }

    private func finishTestIfDue() {
        guard let t = test, t.progress(at: monotonicNanos()) >= 1 else { return }
        let result = t.finish(at: monotonicNanos(), currentCapMicros: state.value.stats?.ext?.maxTargetMicros)
        test = nil
        state.update { $0.testResult = result; $0.testProgress = nil }
    }

    /// Returns true for a verified hello (used to adopt a candidate address).
    private func handle(_ data: [UInt8], from: Endpoint, receivedAt t4: UInt64, lastStats: inout TimeInterval?) -> Bool {
        // The socket is connected, so only the receiver's address gets here;
        // the tag is what proves it is the paired receiver.
        guard Wire.verify(data, key: keys.r2m), let packet = try? Wire.decode(data) else { return false }
        let t = now
        var signedIPv4: [UInt8]?
        var verifiedHello = false
        switch packet {
        case .hello(let nonce, let echo, let ext):
            let fresh = liveness.update { $0.hello(sessionNonce: nonce, echo: echo, at: t) }
            state.update { $0.receiverRevision = ext?.revision ?? 0 }
            if let t1 = probeSentAt.removeValue(forKey: echo) {
                let rtt = Double(t4 &- t1) / 1e6
                rttWindow.append(rtt)
                if rttWindow.count > 30 { rttWindow.removeFirst() }
                test?.rttMillis.append(rtt)
                if let x = ext {
                    clockSync.add(ClockSample(t1: t1, t4: t4, receiverNanos: x.receiverTimeMicros &* 1000))
                }
                state.update { $0.rtt = Distribution(self.rttWindow) }
            }
            signedIPv4 = ext?.receiverIPv4
            verifiedHello = fresh && Self.addressMatches(signedIPv4, from)
        case .stats(let s):
            let seq = tw_sender_last_sequence(sender)
            liveness.update {
                $0.stats(echoSequence: s.echoSequence, sessionNonce: s.sessionNonce, lastSentSequence: seq,
                         packetsPerWindow: packetsPerWindow, at: t)
            }
            lastStats = t
            test?.add(stats: s)
            if let x = s.ext { matchMarker(x) }
            state.update { $0.stats = s }
            signedIPv4 = s.ext?.receiverIPv4
        case .tuningAck(let a):
            guard let r = tuningRequest, r.id == a.requestID else { return false }
            tuningRequest = nil
            state.update {
                $0.tuning = a.result == 0 ? .applied(floorMicros: a.minTargetMicros, capMicros: a.maxTargetMicros)
                    : .rejected(a.message)
            }
            return false
        default:
            return false
        }
        // Remember a new address only when the receiver itself signed that
        // it is at the address the packet came from: a relayed or replayed
        // packet from elsewhere never moves where audio (and the Longcable
        // allow-list) goes. Receivers before 0.2.0 sign no address, so the
        // stored one is kept.
        if liveness.value.isLive(at: t), from.description != config.pairing.lastAddress,
           Self.addressMatches(signedIPv4, from) {
            state.update { $0.learnedAddress = from.description }
        }
        return verifiedHello
    }

    /// True when `signed` (from inside a tagged body) is `from`'s IPv4.
    public static func addressMatches(_ signed: [UInt8]?, _ from: Endpoint) -> Bool {
        guard let s = signed, s.count == 4, s != [0, 0, 0, 0] else { return false }
        return s.map(String.init).joined(separator: ".") == from.host
    }

    /// Relates a reported marker to what the Mac stamped, via the clock fit.
    private func matchMarker(_ x: Wire.StatsExt) {
        guard x.markerTimestamp != 0, x.markerTimestamp != lastMarkerReported,
              let m = pendingMarkers.removeValue(forKey: x.markerTimestamp) else { return }
        lastMarkerReported = x.markerTimestamp
        let fromCapture = m.capture_ns != 0
        let start = fromCapture ? m.capture_ns : m.send_ns
        guard let (offset, unc) = clockSync.offset(at: m.send_ns) else { return }
        let outMac = Double(x.markerOutputMicros) * 1000 - offset
        let ms = (outMac - Double(start)) / 1e6
        // A marker the receiver reports from before it was sent is a clock
        // or matching error, not a measurement.
        guard ms > 0, ms < 1000 else { return }
        let meas = MarkerMeasurement(millis: ms, uncertaintyMillis: unc / 1e6, fromCapture: fromCapture)
        test?.markers.append(meas)
        state.update { $0.lastMarker = meas }
    }

    public func stop() {
        running.value = false
        source.stop()
        while thread?.isFinished == false { usleep(10_000) }
        tw_sender_destroy(sender)
    }
}
