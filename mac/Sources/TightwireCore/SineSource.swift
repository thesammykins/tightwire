import Darwin
import Foundation
import TightwireRT

/// Synthetic test tone paced by the host clock (needs no microphone permission).
/// Each tick emits one packet's worth of frames at an absolute deadline, so
/// timing errors never accumulate.
public final class SineSource: AudioSource, @unchecked Sendable {
    public let sampleRate: UInt32
    public let channelCount: Int
    let frequency: Double
    let amplitude: Float
    let framesPerTick: Int
    public var bufferFrames: UInt32 { UInt32(framesPerTick) }
    /// A synthetic source has no hardware latency.
    public var deviceLatencyFrames: UInt32 { 0 }
    private let running = Locked(false)
    private var thread: Thread?

    public init(frequency: Double, sampleRate: UInt32 = 48_000, channels: Int = 1, framesPerTick: Int = 48, amplitude: Float = 0.5) {
        self.frequency = frequency
        self.sampleRate = sampleRate
        self.channelCount = channels
        self.framesPerTick = framesPerTick
        self.amplitude = amplitude
    }

    public func start(sender: RTSender) throws {
        running.value = true
        let t = Thread { [self] in run(sender) }
        t.name = "tightwire-sine"
        t.qualityOfService = .userInteractive
        thread = t
        t.start()
    }

    private func run(_ sender: RTSender) {
        var tb = mach_timebase_info_data_t()
        mach_timebase_info(&tb)
        let ticksPerSecond = 1e9 * Double(tb.denom) / Double(tb.numer)
        // Same scheduling class as a CoreAudio IO thread, so the synthetic
        // source paces like a device rather than like a sleeping thread.
        var policy = thread_time_constraint_policy_data_t(
            period: UInt32(Double(framesPerTick) / Double(sampleRate) * ticksPerSecond),
            computation: UInt32(Double(framesPerTick) / Double(sampleRate) * ticksPerSecond / 4),
            constraint: UInt32(Double(framesPerTick) / Double(sampleRate) * ticksPerSecond),
            preemptible: 1)
        let policyCount = MemoryLayout<thread_time_constraint_policy_data_t>.size / MemoryLayout<integer_t>.size
        withUnsafeMutablePointer(to: &policy) {
            $0.withMemoryRebound(to: integer_t.self, capacity: policyCount) {
                _ = thread_policy_set(mach_thread_self(), thread_policy_flavor_t(THREAD_TIME_CONSTRAINT_POLICY), $0,
                                      mach_msg_type_number_t(policyCount))
            }
        }
        let start = mach_absolute_time()
        var buf = [Float](repeating: 0, count: framesPerTick * channelCount)
        var n: UInt64 = 0
        let w = 2 * Double.pi * frequency / Double(sampleRate)
        while running.value {
            let deadline = start + UInt64(Double(n + UInt64(framesPerTick)) / Double(sampleRate) * ticksPerSecond)
            mach_wait_until(deadline)
            for i in 0..<framesPerTick {
                let v = amplitude * Float(sin(w * Double(n + UInt64(i))))
                for c in 0..<channelCount { buf[i * channelCount + c] = v }
            }
            buf.withUnsafeBufferPointer {
                tw_sender_process_interleaved(sender.ptr, $0.baseAddress!, UInt32(channelCount), UInt32(framesPerTick))
            }
            n += UInt64(framesPerTick)
        }
    }

    public func stop() {
        running.value = false
        while thread?.isFinished == false { usleep(1000) }
    }
}
