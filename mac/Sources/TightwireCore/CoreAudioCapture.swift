import CoreAudio
import Foundation
import TightwireRT

/// A CoreAudio device with input channels.
public struct InputDevice: Identifiable, Hashable, Sendable {
    public let id: AudioObjectID
    public let uid: String
    public let name: String
    public let channels: Int
    public let sampleRate: Double

    public init(id: AudioObjectID, uid: String, name: String, channels: Int, sampleRate: Double) {
        self.id = id
        self.uid = uid
        self.name = name
        self.channels = channels
        self.sampleRate = sampleRate
    }
}

public enum AudioDevices {
    static func address(_ selector: AudioObjectPropertySelector,
                        _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain)
    }

    static func string(_ obj: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String? {
        var a = address(selector)
        var value: Unmanaged<CFString>?
        var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        guard AudioObjectGetPropertyData(obj, &a, 0, nil, &size, &value) == noErr, let v = value else { return nil }
        return v.takeRetainedValue() as String
    }

    static func inputChannels(_ dev: AudioObjectID) -> Int {
        var a = address(kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeInput)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(dev, &a, 0, nil, &size) == noErr, size > 0 else { return 0 }
        let raw = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: MemoryLayout<AudioBufferList>.alignment)
        defer { raw.deallocate() }
        guard AudioObjectGetPropertyData(dev, &a, 0, nil, &size, raw) == noErr else { return 0 }
        let abl = UnsafeMutableAudioBufferListPointer(raw.assumingMemoryBound(to: AudioBufferList.self))
        return abl.reduce(0) { $0 + Int($1.mNumberChannels) }
    }

    static func nominalRate(_ dev: AudioObjectID) -> Double {
        var a = address(kAudioDevicePropertyNominalSampleRate)
        var rate: Float64 = 0
        var size = UInt32(MemoryLayout<Float64>.size)
        AudioObjectGetPropertyData(dev, &a, 0, nil, &size, &rate)
        return rate
    }

    public static func inputs() -> [InputDevice] {
        var a = address(kAudioHardwarePropertyDevices)
        var size: UInt32 = 0
        let sys = AudioObjectID(kAudioObjectSystemObject)
        guard AudioObjectGetPropertyDataSize(sys, &a, 0, nil, &size) == noErr else { return [] }
        var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
        guard AudioObjectGetPropertyData(sys, &a, 0, nil, &size, &ids) == noErr else { return [] }
        return ids.compactMap { id in
            let ch = inputChannels(id)
            guard ch > 0 else { return nil }
            return InputDevice(id: id, uid: string(id, kAudioDevicePropertyDeviceUID) ?? "\(id)",
                               name: string(id, kAudioObjectPropertyName) ?? "Device \(id)",
                               channels: ch, sampleRate: nominalRate(id))
        }
    }

    public static func systemDefaultInput() -> AudioObjectID? {
        var a = address(kAudioHardwarePropertyDefaultInputDevice)
        var id = AudioObjectID(0)
        var size = UInt32(MemoryLayout<AudioObjectID>.size)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size, &id) == noErr, id != 0
        else { return nil }
        return id
    }

    /// The system default input (no product names are preferred: the user's
    /// saved choice comes first, see `pick`).
    public static func defaultInput(in list: [InputDevice] = inputs(), systemDefault: AudioObjectID? = systemDefaultInput()) -> InputDevice? {
        systemDefault.flatMap { id in list.first { $0.id == id } } ?? list.first
    }

    /// Input policy: the saved choice (UID or name) when present, else the
    /// system default input.
    public static func pick(saved: String?, in list: [InputDevice], systemDefault: AudioObjectID? = systemDefaultInput()) -> InputDevice? {
        if let saved, !saved.isEmpty, let d = find(saved, in: list) { return d }
        return defaultInput(in: list, systemDefault: systemDefault)
    }

    /// Default channel selection: every channel (up to 8) when the device
    /// has more than one, so a stereo mix is sent as stereo.
    public static func defaultChannels(for d: InputDevice) -> ChannelSelection {
        d.channels >= 2 ? .all : .mono(0)
    }

    /// Exact UID, else case-insensitive name substring.
    public static func find(_ query: String, in list: [InputDevice] = inputs()) -> InputDevice? {
        list.first { $0.uid == query } ?? list.first { $0.name.localizedCaseInsensitiveContains(query) }
    }
}

public struct CaptureError: Error, CustomStringConvertible {
    public let description: String
    init(_ what: String, _ status: OSStatus) { description = "\(what) (OSStatus \(status))" }
    init(_ what: String) { description = what }
}

/// HAL IOProc capture (SPEC §4): no AVAudioEngine, no sender-side resampling.
/// The IOProc is the C function `tw_input_ioproc`; this class only does setup,
/// teardown and change notifications.
public final class CoreAudioCapture: AudioSource, @unchecked Sendable {
    public let device: InputDevice
    public let sampleRate: UInt32
    public var channelCount: Int { device.channels }
    public private(set) var bufferFrames: UInt32 = 0
    /// kAudioDevicePropertyLatency + SafetyOffset (input scope) + the first
    /// input stream's kAudioStreamPropertyLatency, as the driver reports them.
    public private(set) var deviceLatencyFrames: UInt32 = 0
    private var procID: AudioDeviceIOProcID?
    private var listeners: [(AudioObjectPropertyAddress, AudioObjectPropertyListenerBlock)] = []
    private let onInterruption: @Sendable (String) -> Void
    private let requestedFrames: UInt32

    /// `onInterruption` fires (on a background queue) when the device vanishes
    /// or changes format; the caller should stop the session and report it.
    public init(device: InputDevice, framesPerBuffer: UInt32, onInterruption: @escaping @Sendable (String) -> Void) throws {
        self.device = device
        let rate = UInt32(device.sampleRate.rounded())
        guard Wire.sampleRates.contains(rate) else {
            throw CaptureError("\(device.name) runs at \(rate) Hz; Tightwire needs 44.1, 48, 88.2 or 96 kHz")
        }
        sampleRate = rate
        requestedFrames = framesPerBuffer
        self.onInterruption = onInterruption
        try checkFloatFormat()
    }

    private func checkFloatFormat() throws {
        var a = AudioDevices.address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput)
        var size: UInt32 = 0
        AudioObjectGetPropertyDataSize(device.id, &a, 0, nil, &size)
        var streams = [AudioStreamID](repeating: 0, count: Int(size) / MemoryLayout<AudioStreamID>.size)
        AudioObjectGetPropertyData(device.id, &a, 0, nil, &size, &streams)
        for s in streams {
            var fa = AudioDevices.address(kAudioStreamPropertyVirtualFormat)
            var asbd = AudioStreamBasicDescription()
            var sz = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
            guard AudioObjectGetPropertyData(s, &fa, 0, nil, &sz, &asbd) == noErr else { continue }
            let isFloat = asbd.mFormatID == kAudioFormatLinearPCM && asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0
                && asbd.mBitsPerChannel == 32 && asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved == 0
            guard isFloat else { throw CaptureError("\(device.name): input stream is not interleaved float32") }
        }
    }

    private func u32(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope) -> UInt32 {
        var a = AudioDevices.address(sel, scope)
        var v: UInt32 = 0
        var size = UInt32(MemoryLayout<UInt32>.size)
        return AudioObjectGetPropertyData(obj, &a, 0, nil, &size, &v) == noErr ? v : 0
    }

    private func inputLatencyFrames() -> UInt32 {
        let scope = kAudioObjectPropertyScopeInput
        var total = u32(device.id, kAudioDevicePropertyLatency, scope) + u32(device.id, kAudioDevicePropertySafetyOffset, scope)
        var a = AudioDevices.address(kAudioDevicePropertyStreams, scope)
        var size: UInt32 = 0
        if AudioObjectGetPropertyDataSize(device.id, &a, 0, nil, &size) == noErr, size >= UInt32(MemoryLayout<AudioStreamID>.size) {
            var streams = [AudioStreamID](repeating: 0, count: Int(size) / MemoryLayout<AudioStreamID>.size)
            if AudioObjectGetPropertyData(device.id, &a, 0, nil, &size, &streams) == noErr, let first = streams.first {
                total += u32(first, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal)
            }
        }
        return total
    }

    public func start(sender: RTSender) throws {
        // Request the packet size as the IO buffer, clamped to what the device allows.
        var ra = AudioDevices.address(kAudioDevicePropertyBufferFrameSizeRange)
        var range = AudioValueRange()
        var size = UInt32(MemoryLayout<AudioValueRange>.size)
        var frames = requestedFrames
        if AudioObjectGetPropertyData(device.id, &ra, 0, nil, &size, &range) == noErr {
            frames = UInt32(min(max(Double(frames), range.mMinimum), range.mMaximum))
        }
        var ba = AudioDevices.address(kAudioDevicePropertyBufferFrameSize)
        AudioObjectSetPropertyData(device.id, &ba, 0, nil, UInt32(MemoryLayout<UInt32>.size), &frames)
        size = UInt32(MemoryLayout<UInt32>.size)
        AudioObjectGetPropertyData(device.id, &ba, 0, nil, &size, &bufferFrames)
        deviceLatencyFrames = inputLatencyFrames()

        var proc: AudioDeviceIOProcID?
        var st = AudioDeviceCreateIOProcID(device.id, tw_input_ioproc, UnsafeMutableRawPointer(sender.ptr), &proc)
        guard st == noErr, let proc else { throw CaptureError("could not attach to \(device.name)", st) }
        procID = proc
        st = AudioDeviceStart(device.id, proc)
        guard st == noErr else {
            AudioDeviceDestroyIOProcID(device.id, proc)
            procID = nil
            throw CaptureError("could not start \(device.name)", st)
        }
        watch(kAudioDevicePropertyDeviceIsAlive, "\(device.name) was disconnected")
        watch(kAudioDevicePropertyNominalSampleRate, "\(device.name) changed sample rate")
        watch(kAudioDevicePropertyStreamConfiguration, "\(device.name) changed its channel layout", scope: kAudioObjectPropertyScopeInput)
    }

    private func watch(_ selector: AudioObjectPropertySelector, _ message: String,
                       scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) {
        var a = AudioDevices.address(selector, scope)
        let cb = onInterruption
        let block: AudioObjectPropertyListenerBlock = { _, _ in cb(message) }
        if AudioObjectAddPropertyListenerBlock(device.id, &a, DispatchQueue.global(), block) == noErr {
            listeners.append((a, block))
        }
    }

    public func stop() {
        for (addr, block) in listeners {
            var a = addr
            AudioObjectRemovePropertyListenerBlock(device.id, &a, DispatchQueue.global(), block)
        }
        listeners.removeAll()
        if let p = procID {
            // AudioDeviceStop is synchronous: no IOProc call runs after it returns,
            // so the packetizer can be freed afterwards.
            AudioDeviceStop(device.id, p)
            AudioDeviceDestroyIOProcID(device.id, p)
            procID = nil
        }
    }
}
