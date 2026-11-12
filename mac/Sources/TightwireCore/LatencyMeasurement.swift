import Darwin
import Foundation

/// Monotonic nanoseconds (CLOCK_UPTIME_RAW): the same clock the C packetizer
/// stamps latency markers with (mach_absolute_time converted to ns).
public func monotonicNanos() -> UInt64 { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }

/// One probe/hello exchange: Mac send (t1) and receive (t4) times, and the
/// receiver's clock when it built the hello. NTP-style: assuming the two
/// one-way delays are equal, receiver clock − Mac clock = tr − (t1 + t4)/2,
/// and the error of that assumption is at most RTT/2 (both delays ≥ 0).
public struct ClockSample: Equatable, Sendable {
    public var t1: UInt64
    public var t4: UInt64
    public var receiverNanos: UInt64

    public init(t1: UInt64, t4: UInt64, receiverNanos: UInt64) {
        self.t1 = t1
        self.t4 = t4
        self.receiverNanos = receiverNanos
    }

    public var rttNanos: Double { Double(t4 &- t1) }
    public var midNanos: Double { (Double(t1) + Double(t4)) / 2 }
    /// Receiver clock minus Mac clock (ns).
    public var offsetNanos: Double { Double(receiverNanos) - midNanos }
}

/// Relates the receiver's monotonic clock to the Mac's. The two crystals run
/// at slightly different rates (≤ ~100 ppm), so the offset is fitted as a
/// line over time through the best (lowest-RTT) exchange of each 2 s slot;
/// the asymmetric-delay error is bounded by half the RTT of those samples.
public struct ClockSync: Sendable {
    public private(set) var samples: [ClockSample] = []
    public static let keep = 900
    public static let slotNanos: UInt64 = 2_000_000_000

    public init() {}

    public mutating func add(_ s: ClockSample) {
        guard s.t4 >= s.t1 else { return }
        samples.append(s)
        if samples.count > Self.keep { samples.removeFirst(samples.count - Self.keep) }
    }

    /// Lowest-RTT sample per 2 s slot.
    var best: [ClockSample] {
        var bySlot: [UInt64: ClockSample] = [:]
        for s in samples {
            let k = s.t1 / Self.slotNanos
            if let cur = bySlot[k], cur.rttNanos <= s.rttNanos { continue }
            bySlot[k] = s
        }
        return bySlot.keys.sorted().compactMap { bySlot[$0] }
    }

    /// Receiver clock − Mac clock at Mac time `t` (ns), with an uncertainty
    /// bound (ns): half the largest RTT among the samples used plus the
    /// residual of the line fit.
    public func offset(at t: UInt64) -> (offset: Double, uncertainty: Double)? {
        let pts = best
        guard let first = pts.first else { return nil }
        let halfRTT = pts.map(\.rttNanos).max()! / 2
        guard pts.count >= 3 else { return (first.offsetNanos, halfRTT) }
        // Least squares offset = a + b·(mid − m0); m0 keeps the numbers small.
        let m0 = pts[0].midNanos
        let xs = pts.map { $0.midNanos - m0 }
        let ys = pts.map(\.offsetNanos)
        let n = Double(pts.count)
        let mx = xs.reduce(0, +) / n
        let my = ys.reduce(0, +) / n
        let sxx = xs.reduce(0) { $0 + ($1 - mx) * ($1 - mx) }
        let sxy = zip(xs, ys).reduce(0) { $0 + ($1.0 - mx) * ($1.1 - my) }
        let b = sxx > 0 ? sxy / sxx : 0
        let a = my - b * mx
        let residual = sqrt(zip(xs, ys).reduce(0) { $0 + pow($1.1 - (a + b * $1.0), 2) } / n)
        return (a + b * (Double(t) - m0), halfRTT + residual)
    }

    /// Crystal rate difference seen by the fit (ppm), once there is enough span.
    public var driftPPM: Double? {
        let pts = best
        guard pts.count >= 3, let a = pts.first, let z = pts.last, z.midNanos - a.midNanos > 5e9 else { return nil }
        return (z.offsetNanos - a.offsetNanos) / (z.midNanos - a.midNanos) * 1e6
    }
}

/// Summary of a list of values (ms).
public struct Distribution: Equatable, Sendable {
    public var count: Int
    public var min: Double
    public var median: Double
    public var p95: Double
    public var max: Double
    public var mean: Double

    public init?(_ values: [Double]) {
        guard !values.isEmpty else { return nil }
        let v = values.sorted()
        func q(_ p: Double) -> Double { v[Swift.min(v.count - 1, Int((Double(v.count - 1) * p).rounded()))] }
        count = v.count
        min = v[0]
        median = q(0.5)
        p95 = q(0.95)
        max = v[v.count - 1]
        mean = v.reduce(0, +) / Double(v.count)
    }
}

/// One measured marker: capture (or send) on the Mac → first frame handed
/// to the PC's output device.
public struct MarkerMeasurement: Equatable, Sendable {
    public var millis: Double
    public var uncertaintyMillis: Double
    /// True when measured from Core Audio's capture timestamp; false when
    /// from the packet's send time (synthetic sources).
    public var fromCapture: Bool
    public init(millis: Double, uncertaintyMillis: Double, fromCapture: Bool) {
        self.millis = millis
        self.uncertaintyMillis = uncertaintyMillis
        self.fromCapture = fromCapture
    }
}

/// What a 30 s latency test collected (receiver stats once per second, RTT
/// per probe, markers once per second) and what it recommends.
public struct LatencyTestResult: Equatable, Sendable {
    public var seconds: Double
    public var rtt: Distribution?
    public var depth: Distribution?
    public var target: Distribution?
    public var jitterPeakMaxMillis: Double
    public var jitterMeanMillis: Double
    public var lost: UInt32
    public var late: UInt32
    public var underruns: UInt32
    public var driftPPM: Distribution?
    public var marker: Distribution?
    public var markerUncertaintyMillis: Double?
    public var markerFromCapture: Bool
    public var recommendation: Recommendation
    /// Receiver is older than 0.2.0: buffer/jitter details and markers missing.
    public var receiverOutdated: Bool

    public struct Recommendation: Equatable, Sendable {
        public var framesPerPacket: Int
        /// nil = adaptive (no floor).
        public var floorMicros: UInt32?
        public var capMicros: UInt32
        public var reason: String
        public init(framesPerPacket: Int, floorMicros: UInt32?, capMicros: UInt32, reason: String) {
            self.framesPerPacket = framesPerPacket
            self.floorMicros = floorMicros
            self.capMicros = capMicros
            self.reason = reason
        }
    }

    public init(seconds: Double, rtt: Distribution?, depth: Distribution?, target: Distribution?, jitterPeakMaxMillis: Double,
                jitterMeanMillis: Double, lost: UInt32, late: UInt32, underruns: UInt32, driftPPM: Distribution?,
                marker: Distribution?, markerUncertaintyMillis: Double?, markerFromCapture: Bool,
                recommendation: Recommendation, receiverOutdated: Bool) {
        self.seconds = seconds
        self.rtt = rtt
        self.depth = depth
        self.target = target
        self.jitterPeakMaxMillis = jitterPeakMaxMillis
        self.jitterMeanMillis = jitterMeanMillis
        self.lost = lost
        self.late = late
        self.underruns = underruns
        self.driftPPM = driftPPM
        self.marker = marker
        self.markerUncertaintyMillis = markerUncertaintyMillis
        self.markerFromCapture = markerFromCapture
        self.recommendation = recommendation
        self.receiverOutdated = receiverOutdated
    }

    /// Short text for the window.
    public var summary: String {
        var parts: [String] = []
        if let r = rtt { parts.append(String(format: "RTT %.2f ms median, %.2f ms p95", r.median, r.p95)) }
        parts.append(String(format: "jitter peak %.2f ms", jitterPeakMaxMillis))
        parts.append("lost \(lost), late \(late), underruns \(underruns)")
        if let m = marker, let u = markerUncertaintyMillis {
            parts.append(String(format: "measured %@ → PC output %.1f ms median (± %.1f ms)", markerFromCapture ? "capture" : "send", m.median, u))
        }
        return parts.joined(separator: " · ")
    }

    /// One log line with everything.
    public var logLine: String {
        func d(_ x: Distribution?) -> String {
            x.map { String(format: "min %.2f med %.2f p95 %.2f max %.2f", $0.min, $0.median, $0.p95, $0.max) } ?? "n/a"
        }
        return String(format: "latency test %.0f s: rtt ms [%@] buffer ms [%@] target ms [%@] jitter mean %.2f peak %.2f ms lost %u late %u underruns %u drift ppm [%@] marker ms [%@]%@ → recommend %d frames, floor %@, cap %.1f ms (%@)",
                      seconds, d(rtt), d(depth), d(target), jitterMeanMillis, jitterPeakMaxMillis, lost, late, underruns,
                      d(driftPPM), d(marker), markerUncertaintyMillis.map { String(format: " ±%.2f", $0) } ?? "",
                      recommendation.framesPerPacket,
                      recommendation.floorMicros.map { String(format: "%.1f ms", Double($0) / 1000) } ?? "adaptive",
                      Double(recommendation.capMicros) / 1000, recommendation.reason)
    }
}

/// Accumulates samples during a test; pure, so the recommendation logic is
/// unit-tested with synthetic numbers.
public struct LatencyTest: Sendable {
    public static let duration: TimeInterval = 30
    public let startedAt: UInt64
    public var rttMillis: [Double] = []
    public var depthMillis: [Double] = []
    public var targetMillis: [Double] = []
    public var jitterMeanMillis: [Double] = []
    public var jitterPeakMillis: [Double] = []
    public var driftPPM: [Double] = []
    public var markers: [MarkerMeasurement] = []
    var first: Wire.Stats?
    var last: Wire.Stats?

    public init(startedAt: UInt64) { self.startedAt = startedAt }

    public func progress(at now: UInt64) -> Double {
        min(1, Double(now &- startedAt) / 1e9 / Self.duration)
    }

    public mutating func add(stats s: Wire.Stats) {
        if first == nil { first = s }
        last = s
        depthMillis.append(Double(s.depthMicros) / 1000)
        driftPPM.append(Double(s.ext.map { Double($0.driftPPB) / 1000 } ?? Double(s.driftPPM)))
        if let x = s.ext {
            targetMillis.append(Double(x.targetMicros) / 1000)
            jitterMeanMillis.append(Double(x.jitterMeanMicros) / 1000)
            jitterPeakMillis.append(Double(x.jitterPeakMicros) / 1000)
        }
    }

    /// Counters can restart with the receiver; a negative delta reads as 0.
    static func delta(_ a: UInt32?, _ b: UInt32?) -> UInt32 {
        guard let a, let b, b >= a else { return 0 }
        return b - a
    }

    public func finish(at now: UInt64, currentCapMicros: UInt32?) -> LatencyTestResult {
        let lost = Self.delta(first?.lost, last?.lost)
        let late = Self.delta(first?.late, last?.late)
        let under = Self.delta(first?.underruns, last?.underruns)
        let peak = jitterPeakMillis.max() ?? 0
        let rtt = Distribution(rttMillis)
        let rec = Self.recommend(jitterPeakMillis: peak, lost: lost, late: late, underruns: under,
                                 maxTargetMillis: targetMillis.max() ?? depthMillis.max() ?? 0,
                                 currentCapMicros: currentCapMicros)
        return LatencyTestResult(
            seconds: Double(now &- startedAt) / 1e9, rtt: rtt, depth: Distribution(depthMillis),
            target: Distribution(targetMillis), jitterPeakMaxMillis: peak,
            jitterMeanMillis: jitterMeanMillis.isEmpty ? 0 : jitterMeanMillis.reduce(0, +) / Double(jitterMeanMillis.count),
            lost: lost, late: late, underruns: under, driftPPM: Distribution(driftPPM),
            marker: Distribution(markers.map(\.millis)),
            markerUncertaintyMillis: markers.map(\.uncertaintyMillis).max(),
            markerFromCapture: markers.first?.fromCapture ?? false,
            recommendation: rec, receiverOutdated: last?.ext == nil && last != nil)
    }

    /// Packet size from the measured network: smaller packets cut capture and
    /// packetisation delay but only pay off when arrivals are tight. Floor
    /// only when the adaptive buffer underran or dropped late packets.
    public static func recommend(jitterPeakMillis peak: Double, lost: UInt32, late: UInt32, underruns: UInt32,
                                 maxTargetMillis: Double, currentCapMicros: UInt32?) -> LatencyTestResult.Recommendation {
        let clean = lost == 0 && late == 0 && underruns == 0
        let frames: Int
        let why: String
        switch peak {
        case ..<0.5 where clean:
            frames = 32
            why = "arrivals within 0.5 ms and nothing lost"
        case ..<2:
            frames = 48
            why = clean ? "arrivals within 2 ms" : "some loss or underruns"
        case ..<5:
            frames = 128
            why = "jitter peaks up to 5 ms"
        default:
            frames = 256
            why = "jitter peaks above 5 ms"
        }
        var floor: UInt32?
        var reason = why
        if underruns > 0 || late > 0 {
            // Half a millisecond above the largest target seen, rounded up.
            let ms = (ceil((maxTargetMillis + 0.5) * 2) / 2)
            floor = UInt32(ms * 1000)
            reason += "; \(underruns) underruns and \(late) late packets: fix a floor above the largest target seen"
        } else {
            reason += "; the adaptive buffer kept up"
        }
        let base = UInt32(20_000)
        var cap = max(currentCapMicros ?? base, base)
        if let f = floor { cap = max(cap, 2 * f + 4000) }
        cap = min(cap, 100_000)
        return .init(framesPerPacket: frames, floorMicros: floor, capMicros: cap, reason: reason)
    }
}
