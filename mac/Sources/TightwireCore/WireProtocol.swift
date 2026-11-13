import Foundation
import TightwireRT

/// Tightwire wire protocol v2 (SPEC §3, §3a). All integers little-endian.
///
/// `decode` checks structure only; tagged packets keep their tag in the
/// datagram and must be checked with `Wire.verify` before use.
public enum Wire {
    public static let magic: [UInt8] = Array("TWIR".utf8)
    public static let version: UInt8 = 2
    public static let headerLength = 8
    public static let audioHeaderLength = 24
    public static let tagLength = 8
    /// Keeps every packet inside one Ethernet frame.
    public static let maxPayload = 1400
    public static let maxChannels = 8
    public static let statsBodyLength = 48
    public static let helloBodyLength = 24
    public static let probeBodyLength = 16
    public static let idLength = 16
    public static let defaultPort: UInt16 = 4810
    public static let beaconPort: UInt16 = 4811
    public static let senderPort: UInt16 = 4812
    /// The Mac's handoff control socket (SPEC §9).
    public static let handoffPort: UInt16 = 4813
    public static let sampleRates: [UInt32] = [44100, 48000, 88200, 96000]
    public static let flagDiscontinuity: UInt8 = 0x01
    /// Latency marker: the receiver reports when this packet's first frame
    /// reached its output device (stats extension). 0.1 receivers ignore it.
    public static let flagMarker: UInt8 = 0x02
    /// Revision within version 2 (hello/stats extensions): 0 = 0.1.x,
    /// 1 = 0.2.0, 2 = 0.2.1 (stats health block).
    public static let protocolRevision: UInt8 = 2
    public static let statsExtLength = 88
    public static let statsHealthLength = 25
    public static let helloExtLength = 13
    public static let tuningBodyLength = 21
    /// Peer names (beacon host, pairing names) are shown next to the pairing
    /// code: at most this many characters and no control/format characters.
    public static let maxPeerNameCharacters = 40
    public static let maxMessageCharacters = 200

    public enum PacketType: UInt8, Sendable {
        case audio = 1, stats = 2, beacon = 3, hello = 4, probe = 5
        case commit = 8, response = 9, reveal = 10, confirm = 11, abort = 12
        case handoffOffer = 16, handoffAck = 17, handoffRelease = 18, deckRequest = 19, deckAck = 20
        case tuningSet = 21, tuningAck = 22

        /// 16–31 are reserved for authenticated control messages.
        public static func isTagged(_ raw: UInt8) -> Bool {
            [1, 2, 4, 5].contains(raw) || (16...31).contains(raw)
        }
    }

    public enum SampleFormat: UInt8, Sendable {
        case f32 = 0, s24 = 1, s16 = 2
        public var bytes: Int { self == .f32 ? 4 : self == .s24 ? 3 : 2 }
        public var name: String { self == .f32 ? "f32" : self == .s24 ? "s24" : "s16" }
    }

    public enum DecodeError: Error, Equatable, Sendable {
        case length, magic, version, type, sampleRate, channels, format, utf8, field
    }

    public enum Samples: Equatable, Sendable {
        case f32([Float])
        case s24([Int32])
        case s16([Int16])

        public var count: Int {
            switch self {
            case .f32(let s): s.count
            case .s24(let s): s.count
            case .s16(let s): s.count
            }
        }

        public var format: SampleFormat {
            switch self {
            case .f32: .f32
            case .s24: .s24
            case .s16: .s16
            }
        }
    }

    public struct AudioPacket: Equatable, Sendable {
        public var streamID: UInt16
        public var sequence: UInt32
        public var timestamp: UInt64
        public var sampleRate: UInt32
        public var channels: UInt8
        public var flags: UInt8
        public var samples: Samples

        public init(streamID: UInt16, sequence: UInt32, timestamp: UInt64, sampleRate: UInt32,
                    channels: UInt8, flags: UInt8, samples: Samples) {
            self.streamID = streamID
            self.sequence = sequence
            self.timestamp = timestamp
            self.sampleRate = sampleRate
            self.channels = channels
            self.flags = flags
            self.samples = samples
        }
    }

    public struct Stats: Equatable, Sendable {
        public var streamID: UInt16
        public var depthMicros: UInt32
        public var received: UInt32
        public var lost: UInt32
        public var late: UInt32
        public var driftPPM: Int32
        public var underruns: UInt32
        public var authDropped: UInt32
        public var echoSequence: UInt32
        public var sessionNonce: UInt64
        /// 0.2.0 receivers.
        public var ext: StatsExt?

        public init(streamID: UInt16, depthMicros: UInt32, received: UInt32, lost: UInt32, late: UInt32,
                    driftPPM: Int32, underruns: UInt32, authDropped: UInt32, echoSequence: UInt32,
                    sessionNonce: UInt64, ext: StatsExt? = nil) {
            self.ext = ext
            self.streamID = streamID
            self.depthMicros = depthMicros
            self.received = received
            self.lost = lost
            self.late = late
            self.driftPPM = driftPPM
            self.underruns = underruns
            self.authDropped = authDropped
            self.echoSequence = echoSequence
            self.sessionNonce = sessionNonce
        }
    }

    /// How the receiver's output runs (stats extension).
    public enum OutputMode: UInt8, Sendable {
        case none = 0, wasapiExclusive = 1, wasapiSharedLowLatency = 2, wasapiShared = 3, cpal = 4, file = 5
        public var name: String {
            switch self {
            case .none: "no output"
            case .wasapiExclusive: "WASAPI exclusive"
            case .wasapiSharedLowLatency: "WASAPI shared (IAudioClient3)"
            case .wasapiShared: "WASAPI shared"
            case .cpal: "cpal"
            case .file: "file"
            }
        }
    }

    /// Stats extension (SPEC §3). Times in µs; `*Micros` clock values are the
    /// receiver's monotonic clock.
    public struct StatsExt: Equatable, Sendable {
        public var outputMode: OutputMode = .none
        public var streamChannels: UInt8 = 0
        public var outputChannels: UInt8 = 0
        public var targetMicros: UInt32 = 0
        public var jitterMeanMicros: UInt32 = 0
        public var jitterPeakMicros: UInt32 = 0
        public var minTargetMicros: UInt32 = 0
        public var maxTargetMicros: UInt32 = 0
        public var outputPeriodFrames: UInt32 = 0
        public var outputRate: UInt32 = 0
        public var streamRate: UInt32 = 0
        public var driftPPB: Int32 = 0
        public var duplicates: UInt32 = 0
        public var concealedFrames: UInt32 = 0
        public var resets: UInt32 = 0
        public var ringFill: UInt16 = 0
        public var ringCapacity: UInt16 = 0
        public var ringFull: UInt32 = 0
        public var markerTimestamp: UInt64 = 0
        public var markerOutputMicros: UInt64 = 0
        public var receiverNowMicros: UInt64 = 0
        public var receiverIPv4: [UInt8] = [0, 0, 0, 0]
        /// Revision 2 (0.2.1) receivers.
        public var health: StatsHealth?
        public init() {}

        public var outputPeriodMillis: Double {
            outputRate == 0 ? 0 : Double(outputPeriodFrames) * 1000 / Double(outputRate)
        }
    }

    /// Output health (stats extension revision 2, 0.2.1 receivers).
    public struct StatsHealth: Equatable, Sendable {
        public static let outputOpen: UInt8 = 1 << 0
        public static let saturated: UInt8 = 1 << 1
        public static let underruns: UInt8 = 1 << 2
        public static let exclusiveFallback: UInt8 = 1 << 3

        public var flags: UInt8 = 0
        /// Audio queued in the PC's output endpoint beyond its jitter buffer (µs).
        public var deviceQueueMicros: UInt32 = 0
        /// The PC output device's measured rate vs nominal (ppb; 0 = unknown).
        public var deviceRatePPB: Int32 = 0
        /// How far the device pulls ahead of its playout (µs).
        public var deviceLeadMicros: UInt32 = 0
        public var trims: UInt32 = 0
        public var skips: UInt32 = 0
        public var inserts: UInt32 = 0
        public init() {}

        /// An output device is open on the PC and rendering.
        public var outputOpen: Bool { flags & Self.outputOpen != 0 }
        /// The PC's drift loop has been at its limit for seconds.
        public var isSaturated: Bool { flags & Self.saturated != 0 }
        /// More than one underrun on the PC in the last 10 s.
        public var underrunsRising: Bool { flags & Self.underruns != 0 }
        /// The PC gave up on exclusive mode and fell back to shared.
        public var exclusiveFellBack: Bool { flags & Self.exclusiveFallback != 0 }
    }

    /// Hello extension (0.2.0 receivers).
    public struct HelloExt: Equatable, Sendable {
        public var revision: UInt8
        public var receiverIPv4: [UInt8]
        public var receiverTimeMicros: UInt64
        public init(revision: UInt8, receiverIPv4: [UInt8], receiverTimeMicros: UInt64) {
            self.revision = revision
            self.receiverIPv4 = receiverIPv4
            self.receiverTimeMicros = receiverTimeMicros
        }
    }

    public struct TuningAck: Equatable, Sendable {
        public var requestID: UInt32
        /// 0 applied, 1 rejected.
        public var result: UInt8
        public var minTargetMicros: UInt32
        public var maxTargetMicros: UInt32
        public var message: String
        public init(requestID: UInt32, result: UInt8, minTargetMicros: UInt32, maxTargetMicros: UInt32, message: String) {
            self.requestID = requestID
            self.result = result
            self.minTargetMicros = minTargetMicros
            self.maxTargetMicros = maxTargetMicros
            self.message = message
        }
    }

    public struct Beacon: Equatable, Sendable {
        public var port: UInt16
        public var receiverID: [UInt8]
        public var host: String
        public init(port: UInt16, receiverID: [UInt8], host: String) {
            self.port = port
            self.receiverID = receiverID
            self.host = host
        }
    }

    public struct HandoffItem: Equatable, Sendable {
        public enum Result: UInt8, Sendable { case ok = 0, failed = 1, usbipMissing = 2 }
        public var busID: String
        public var result: Result
        public var port: UInt8
        public var message: String
        public init(busID: String, result: Result, port: UInt8, message: String) {
            self.busID = busID
            self.result = result
            self.port = port
            self.message = message
        }
    }

    public enum DeckTarget: UInt8, Sendable { case mac = 0, pc = 1, toggle = 2 }

    public enum Packet: Equatable, Sendable {
        case audio(AudioPacket)
        case stats(Stats)
        case beacon(Beacon)
        case hello(sessionNonce: UInt64, echo: UInt64, ext: HelloExt? = nil)
        case probe(nonce: UInt64)
        case commit(id: [UInt8], commitment: [UInt8], name: String)
        case response(id: [UInt8], publicKey: [UInt8], name: String)
        case reveal(publicKey: [UInt8])
        case confirm(role: UInt8, mac: [UInt8])
        case abort(reason: UInt8)
        case handoffOffer(requestID: UInt32, usbipPort: UInt16, host: String, busIDs: [String])
        /// op: 1 attach, 2 detach
        case handoffAck(requestID: UInt32, op: UInt8, items: [HandoffItem])
        case handoffRelease(requestID: UInt32, busIDs: [String])
        case deckRequest(requestID: UInt32, sentAtMillis: UInt64, target: DeckTarget)
        case deckAck(requestID: UInt32, accepted: Bool, ownerIsPC: Bool, message: String)
        case tuningSet(requestID: UInt32, adaptive: Bool, minTargetMicros: UInt32, maxTargetMicros: UInt32)
        case tuningAck(TuningAck)
    }

    // MARK: Peer-supplied text

    /// Unicode Cc and Cf (bidi overrides/isolates, zero-width characters, BOM…)
    /// plus line/paragraph separators: anything that can move, hide or
    /// reorder text around a pairing code. Same set as the receiver's.
    public static func isUnsafe(_ u: Unicode.Scalar) -> Bool {
        switch u.properties.generalCategory {
        case .control, .format, .lineSeparator, .paragraphSeparator: return true
        default: return false
        }
    }

    /// Drops unsafe characters and cuts to `max` characters ("…" added).
    public static func sanitize(_ s: String, max: Int) -> String {
        var scalars = String.UnicodeScalarView()
        scalars.append(contentsOf: s.unicodeScalars.filter { !isUnsafe($0) })
        let clean = String(scalars).trimmingCharacters(in: .whitespaces)
        guard clean.count > max else { return clean }
        return String(clean.prefix(max - 1)).trimmingCharacters(in: .whitespaces) + "…"
    }

    public static func validPeerName(_ s: String) -> Bool {
        s.count <= maxPeerNameCharacters && s.unicodeScalars.count <= 4 * maxPeerNameCharacters
            && !s.unicodeScalars.contains(where: isUnsafe)
    }

    /// What this side sends as its own name.
    public static func outgoingName(_ s: String) -> String { sanitize(s, max: maxPeerNameCharacters) }

    /// USB/IP bus ids: `^[0-9]+-[0-9]+(\.[0-9]+)*$`, at most 32 bytes.
    public static func validBusID(_ id: String) -> Bool {
        guard !id.isEmpty, id.utf8.count <= 32 else { return false }
        let parts = id.split(separator: "-", maxSplits: 1, omittingEmptySubsequences: false)
        guard parts.count == 2 else { return false }
        func digits(_ s: Substring) -> Bool { !s.isEmpty && s.allSatisfy { $0.isASCII && $0.isNumber } }
        return digits(parts[0]) && parts[1].split(separator: ".", omittingEmptySubsequences: false).allSatisfy(digits)
    }

    // MARK: Authentication

    public static func tag(key: [UInt8], nonce: UInt64?, body: [UInt8]) -> [UInt8] {
        var out = [UInt8](repeating: 0, count: tagLength)
        precondition(key.count == 16, "SipHash key must be 16 bytes")
        let prefix = nonce.map { withUnsafeBytes(of: $0.littleEndian, Array.init) } ?? []
        body.withUnsafeBufferPointer { b in
            prefix.withUnsafeBufferPointer { p in
                tw_siphash24(key, p.baseAddress, p.count, b.baseAddress, b.count, &out)
            }
        }
        return out
    }

    public static func seal(_ body: [UInt8], key: [UInt8], nonce: UInt64? = nil) -> [UInt8] {
        body + tag(key: key, nonce: nonce, body: body)
    }

    /// Constant-time comparison (no early exit on the first difference).
    public static func constantTimeEqual(_ a: [UInt8], _ b: [UInt8]) -> Bool {
        guard a.count == b.count else { return false }
        return zip(a, b).reduce(0) { $0 | ($1.0 ^ $1.1) } == 0
    }

    /// Constant-time check of the trailing tag.
    public static func verify(_ datagram: [UInt8], key: [UInt8], nonce: UInt64? = nil) -> Bool {
        guard datagram.count >= tagLength else { return false }
        let body = Array(datagram.dropLast(tagLength))
        return constantTimeEqual(tag(key: key, nonce: nonce, body: body), Array(datagram.suffix(tagLength)))
    }

    // MARK: Decoding

    public static func decode(_ data: some Collection<UInt8>) throws(DecodeError) -> Packet {
        var b = Array(data)
        guard b.count >= headerLength else { throw .length }
        guard Array(b[0..<4]) == magic else { throw .magic }
        guard b[4] == version else { throw .version }
        let rawType = b[5]
        if PacketType.isTagged(rawType) {
            guard b.count >= headerLength + tagLength else { throw .length }
            b.removeLast(tagLength)
        }
        guard let type = PacketType(rawValue: rawType) else { throw .type }
        let streamID = b.u16(6)
        func need(_ n: Int) throws(DecodeError) { if b.count < n { throw .length } }
        func name(at o: Int) throws(DecodeError) -> String {
            try need(o + 1)
            let len = Int(b[o])
            try need(o + 1 + len)
            guard let s = String(bytes: b[(o + 1)..<(o + 1 + len)], encoding: .utf8) else { throw .utf8 }
            return s
        }
        /// Name plus the offset after it.
        func field(at o: Int) throws(DecodeError) -> (String, Int) {
            let s = try name(at: o)
            return (s, o + 1 + Int(b[o]))
        }
        /// Peer names are rejected, not cleaned, when unsafe or too long.
        func peerName(at o: Int) throws(DecodeError) -> String {
            let s = try name(at: o)
            guard validPeerName(s) else { throw .field }
            return s
        }
        func message(at o: Int) throws(DecodeError) -> (String, Int) {
            let (s, n) = try field(at: o)
            return (sanitize(s, max: maxMessageCharacters), n)
        }
        func busIDs(at o: Int) throws(DecodeError) -> ([String], Int) {
            try need(o + 1)
            var out: [String] = []
            var at = o + 1
            for _ in 0..<Int(b[o]) {
                let (id, next) = try field(at: at)
                guard validBusID(id) else { throw .field }
                out.append(id)
                at = next
            }
            return (out, at)
        }
        switch type {
        case .audio:
            try need(audioHeaderLength)
            guard Int(b[20]) < sampleRates.count else { throw .sampleRate }
            let channels = b[21]
            guard channels >= 1, Int(channels) <= maxChannels else { throw .channels }
            guard let format = SampleFormat(rawValue: b[22]) else { throw .format }
            let payload = b[audioHeaderLength...]
            let frameBytes = Int(channels) * format.bytes
            guard !payload.isEmpty, payload.count <= maxPayload, payload.count % frameBytes == 0 else {
                throw .length
            }
            let start = audioHeaderLength
            let n = payload.count / format.bytes
            let samples: Samples
            switch format {
            case .f32:
                samples = .f32((0..<n).map { Float(bitPattern: b.u32(start + $0 * 4)) })
            case .s24:
                samples = .s24((0..<n).map { i in
                    let o = start + i * 3
                    // Shift into the top 24 bits so the arithmetic shift sign-extends.
                    let raw = UInt32(b[o]) << 8 | UInt32(b[o + 1]) << 16 | UInt32(b[o + 2]) << 24
                    return Int32(bitPattern: raw) >> 8
                })
            case .s16:
                samples = .s16((0..<n).map { Int16(bitPattern: b.u16(start + $0 * 2)) })
            }
            return .audio(AudioPacket(streamID: streamID, sequence: b.u32(8), timestamp: b.u64(12),
                                      sampleRate: sampleRates[Int(b[20])], channels: channels,
                                      flags: b[23], samples: samples))
        case .stats:
            try need(statsBodyLength)
            var ext: StatsExt?
            let e = statsBodyLength
            if b.count >= e + statsExtLength, b[e] >= 1 {
                var x = StatsExt()
                x.outputMode = OutputMode(rawValue: b[e + 1]) ?? .none
                x.streamChannels = b[e + 2]
                x.outputChannels = b[e + 3]
                x.targetMicros = b.u32(e + 4)
                x.jitterMeanMicros = b.u32(e + 8)
                x.jitterPeakMicros = b.u32(e + 12)
                x.minTargetMicros = b.u32(e + 16)
                x.maxTargetMicros = b.u32(e + 20)
                x.outputPeriodFrames = b.u32(e + 24)
                x.outputRate = b.u32(e + 28)
                x.streamRate = b.u32(e + 32)
                x.driftPPB = Int32(bitPattern: b.u32(e + 36))
                x.duplicates = b.u32(e + 40)
                x.concealedFrames = b.u32(e + 44)
                x.resets = b.u32(e + 48)
                x.ringFill = b.u16(e + 52)
                x.ringCapacity = b.u16(e + 54)
                x.ringFull = b.u32(e + 56)
                x.markerTimestamp = b.u64(e + 60)
                x.markerOutputMicros = b.u64(e + 68)
                x.receiverNowMicros = b.u64(e + 76)
                x.receiverIPv4 = Array(b[(e + 84)..<(e + 88)])
                let h = e + statsExtLength
                if b[e] >= 2, b.count >= h + statsHealthLength {
                    var y = StatsHealth()
                    y.flags = b[h]
                    y.deviceQueueMicros = b.u32(h + 1)
                    y.deviceRatePPB = Int32(bitPattern: b.u32(h + 5))
                    y.deviceLeadMicros = b.u32(h + 9)
                    y.trims = b.u32(h + 13)
                    y.skips = b.u32(h + 17)
                    y.inserts = b.u32(h + 21)
                    x.health = y
                }
                ext = x
            }
            return .stats(Stats(streamID: streamID, depthMicros: b.u32(8), received: b.u32(12),
                                lost: b.u32(16), late: b.u32(20), driftPPM: Int32(bitPattern: b.u32(24)),
                                underruns: b.u32(28), authDropped: b.u32(32), echoSequence: b.u32(36),
                                sessionNonce: b.u64(40), ext: ext))
        case .hello:
            try need(helloBodyLength)
            let h = helloBodyLength
            let ext = b.count >= h + helloExtLength && b[h] >= 1
                ? HelloExt(revision: b[h], receiverIPv4: Array(b[(h + 1)..<(h + 5)]), receiverTimeMicros: b.u64(h + 5))
                : nil
            return .hello(sessionNonce: b.u64(8), echo: b.u64(16), ext: ext)
        case .probe:
            try need(probeBodyLength)
            return .probe(nonce: b.u64(8))
        case .beacon:
            try need(headerLength + 2 + idLength + 1)
            return .beacon(Beacon(port: b.u16(8), receiverID: Array(b[10..<26]), host: try peerName(at: 26)))
        case .commit:
            try need(headerLength + idLength + 32 + 1)
            return .commit(id: Array(b[8..<24]), commitment: Array(b[24..<56]), name: try peerName(at: 56))
        case .response:
            try need(headerLength + idLength + 32 + 1)
            return .response(id: Array(b[8..<24]), publicKey: Array(b[24..<56]), name: try peerName(at: 56))
        case .reveal:
            try need(headerLength + 32)
            return .reveal(publicKey: Array(b[8..<40]))
        case .confirm:
            try need(headerLength + 1 + 16)
            guard b[8] <= 1 else { throw .field }
            return .confirm(role: b[8], mac: Array(b[9..<25]))
        case .abort:
            try need(headerLength + 1)
            return .abort(reason: b[8])
        case .handoffOffer:
            try need(headerLength + 6)
            let (host, o) = try field(at: 14)
            let (ids, _) = try busIDs(at: o)
            return .handoffOffer(requestID: b.u32(8), usbipPort: b.u16(12), host: host, busIDs: ids)
        case .handoffAck:
            try need(headerLength + 6)
            guard b[12] == 1 || b[12] == 2 else { throw .field }
            var items: [HandoffItem] = []
            var o = 14
            for _ in 0..<Int(b[13]) {
                let (bus, n) = try field(at: o)
                try need(n + 2)
                guard let result = HandoffItem.Result(rawValue: b[n]) else { throw .field }
                let (msg, next) = try message(at: n + 2)
                items.append(HandoffItem(busID: bus, result: result, port: b[n + 1], message: msg))
                o = next
            }
            return .handoffAck(requestID: b.u32(8), op: b[12], items: items)
        case .handoffRelease:
            try need(headerLength + 5)
            return .handoffRelease(requestID: b.u32(8), busIDs: try busIDs(at: 12).0)
        case .deckRequest:
            try need(headerLength + 13)
            guard let t = DeckTarget(rawValue: b[20]) else { throw .field }
            return .deckRequest(requestID: b.u32(8), sentAtMillis: b.u64(12), target: t)
        case .deckAck:
            try need(headerLength + 7)
            guard b[12] <= 1, b[13] <= 1 else { throw .field }
            return .deckAck(requestID: b.u32(8), accepted: b[12] == 1, ownerIsPC: b[13] == 1, message: try message(at: 14).0)
        case .tuningSet:
            try need(tuningBodyLength)
            guard b[12] <= 1 else { throw .field }
            return .tuningSet(requestID: b.u32(8), adaptive: b[12] == 1, minTargetMicros: b.u32(13), maxTargetMicros: b.u32(17))
        case .tuningAck:
            try need(headerLength + 14)
            guard b[12] <= 1 else { throw .field }
            return .tuningAck(TuningAck(requestID: b.u32(8), result: b[12], minTargetMicros: b.u32(13),
                                        maxTargetMicros: b.u32(17), message: try message(at: 21).0))
        }
    }

    // MARK: Encoding (bodies; tagged types are sealed by the caller)

    public static func encode(_ packet: Packet) -> [UInt8] {
        var out: [UInt8] = []
        switch packet {
        case .audio(let p):
            out.reserveCapacity(audioHeaderLength + p.samples.count * p.samples.format.bytes + tagLength)
            out.putHeader(.audio, p.streamID)
            out.put(p.sequence)
            out.put(p.timestamp)
            out.append(UInt8(sampleRates.firstIndex(of: p.sampleRate) ?? 1))
            out.append(p.channels)
            out.append(p.samples.format.rawValue)
            out.append(p.flags)
            switch p.samples {
            case .f32(let s): s.forEach { out.put($0.bitPattern) }
            case .s24(let s): s.forEach { v in
                let u = UInt32(bitPattern: v)
                out.append(contentsOf: [UInt8(u & 0xFF), UInt8(u >> 8 & 0xFF), UInt8(u >> 16 & 0xFF)])
            }
            case .s16(let s): s.forEach { out.put(UInt16(bitPattern: $0)) }
            }
        case .stats(let s):
            out.putHeader(.stats, s.streamID)
            [s.depthMicros, s.received, s.lost, s.late, UInt32(bitPattern: s.driftPPM), s.underruns,
             s.authDropped, s.echoSequence].forEach { out.put($0) }
            out.put(s.sessionNonce)
            if let x = s.ext {
                out.append(x.health == nil ? 1 : 2)
                out.append(x.outputMode.rawValue)
                out.append(x.streamChannels)
                out.append(x.outputChannels)
                [x.targetMicros, x.jitterMeanMicros, x.jitterPeakMicros, x.minTargetMicros, x.maxTargetMicros,
                 x.outputPeriodFrames, x.outputRate, x.streamRate, UInt32(bitPattern: x.driftPPB), x.duplicates,
                 x.concealedFrames, x.resets].forEach { out.put($0) }
                out.put(x.ringFill)
                out.put(x.ringCapacity)
                out.put(x.ringFull)
                out.put(x.markerTimestamp)
                out.put(x.markerOutputMicros)
                out.put(x.receiverNowMicros)
                out.append(contentsOf: x.receiverIPv4.prefix(4))
                if let y = x.health {
                    out.append(y.flags)
                    [y.deviceQueueMicros, UInt32(bitPattern: y.deviceRatePPB), y.deviceLeadMicros, y.trims,
                     y.skips, y.inserts].forEach { out.put($0) }
                }
            }
        case .hello(let nonce, let echo, let ext):
            out.putHeader(.hello, 0)
            out.put(nonce)
            out.put(echo)
            if let x = ext {
                out.append(x.revision)
                out.append(contentsOf: x.receiverIPv4.prefix(4))
                out.put(x.receiverTimeMicros)
            }
        case .probe(let nonce):
            out.putHeader(.probe, 0)
            out.put(nonce)
        case .beacon(let b):
            out.putHeader(.beacon, 0)
            out.put(b.port)
            out.append(contentsOf: b.receiverID)
            out.putName(b.host)
        case .commit(let id, let commitment, let name):
            out.putHeader(.commit, 0)
            out.append(contentsOf: id)
            out.append(contentsOf: commitment)
            out.putName(name)
        case .response(let id, let publicKey, let name):
            out.putHeader(.response, 0)
            out.append(contentsOf: id)
            out.append(contentsOf: publicKey)
            out.putName(name)
        case .reveal(let publicKey):
            out.putHeader(.reveal, 0)
            out.append(contentsOf: publicKey)
        case .confirm(let role, let mac):
            out.putHeader(.confirm, 0)
            out.append(role)
            out.append(contentsOf: mac)
        case .abort(let reason):
            out.putHeader(.abort, 0)
            out.append(reason)
        case .handoffOffer(let id, let port, let host, let ids):
            out.putHeader(.handoffOffer, 0)
            out.put(id)
            out.put(port)
            out.putName(host)
            out.putBusIDs(ids)
        case .handoffAck(let id, let op, let items):
            out.putHeader(.handoffAck, 0)
            out.put(id)
            out.append(op)
            out.append(UInt8(min(items.count, 255)))
            for i in items.prefix(255) {
                out.putName(i.busID)
                out.append(i.result.rawValue)
                out.append(i.port)
                out.putName(i.message)
            }
        case .handoffRelease(let id, let ids):
            out.putHeader(.handoffRelease, 0)
            out.put(id)
            out.putBusIDs(ids)
        case .deckRequest(let id, let ms, let target):
            out.putHeader(.deckRequest, 0)
            out.put(id)
            out.put(ms)
            out.append(target.rawValue)
        case .deckAck(let id, let accepted, let pc, let message):
            out.putHeader(.deckAck, 0)
            out.put(id)
            out.append(accepted ? 1 : 0)
            out.append(pc ? 1 : 0)
            out.putName(message)
        case .tuningSet(let id, let adaptive, let minUs, let maxUs):
            out.putHeader(.tuningSet, 0)
            out.put(id)
            out.append(adaptive ? 1 : 0)
            out.put(minUs)
            out.put(maxUs)
        case .tuningAck(let a):
            out.putHeader(.tuningAck, 0)
            out.put(a.requestID)
            out.append(a.result)
            out.put(a.minTargetMicros)
            out.put(a.maxTargetMicros)
            out.putName(a.message)
        }
        return out
    }
}

extension Array where Element == UInt8 {
    func u16(_ o: Int) -> UInt16 { UInt16(self[o]) | UInt16(self[o + 1]) << 8 }
    func u32(_ o: Int) -> UInt32 { UInt32(u16(o)) | UInt32(u16(o + 2)) << 16 }
    func u64(_ o: Int) -> UInt64 { UInt64(u32(o)) | UInt64(u32(o + 4)) << 32 }

    mutating func put<T: FixedWidthInteger>(_ v: T) {
        Swift.withUnsafeBytes(of: v.littleEndian) { append(contentsOf: $0) }
    }

    fileprivate mutating func putHeader(_ type: Wire.PacketType, _ streamID: UInt16) {
        append(contentsOf: Wire.magic)
        append(Wire.version)
        append(type.rawValue)
        put(streamID)
    }

    fileprivate mutating func putBusIDs(_ ids: [String]) {
        append(UInt8(Swift.min(ids.count, 255)))
        ids.prefix(255).forEach { putName($0) }
    }

    /// u8 length + UTF-8, truncated to 255 bytes on a character boundary.
    fileprivate mutating func putName(_ s: String) {
        var name = Array(s.utf8)
        if name.count > 255 {
            var end = 255
            while end > 0, name[end] & 0xC0 == 0x80 { end -= 1 }
            name = Array(name[..<end])
        }
        append(UInt8(name.count))
        append(contentsOf: name)
    }
}
