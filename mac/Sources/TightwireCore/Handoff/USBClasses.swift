import Foundation
import IOKit

/// What a USB device does, read from its interface descriptors in the
/// IORegistry. Handing a device to the PC removes it from this Mac, so the UI
/// warns before selecting anything the Mac is likely to depend on.
public struct USBInterfaceClass: Equatable, Hashable, Sendable {
    public var cls: Int
    public var subclass: Int
    public var proto: Int
    public init(_ cls: Int, _ subclass: Int = 0, _ proto: Int = 0) {
        self.cls = cls
        self.subclass = subclass
        self.proto = proto
    }
}

public enum USBDeviceRole: String, Sendable, CaseIterable {
    case audio = "audio interface"
    case keyboard
    case mouse
    case storage = "storage device"
    case camera

    /// USB-IF base classes. HID boot protocol 1/2 marks keyboards and mice; a
    /// Stream Deck is HID with protocol 0 and so is none of these.
    public static func roles(_ interfaces: [USBInterfaceClass]) -> [USBDeviceRole] {
        var out: [USBDeviceRole] = []
        func add(_ r: USBDeviceRole) { if !out.contains(r) { out.append(r) } }
        for i in interfaces {
            switch (i.cls, i.proto) {
            case (0x01, _): add(.audio)
            case (0x03, 1): add(.keyboard)
            case (0x03, 2): add(.mouse)
            case (0x08, _): add(.storage)
            case (0x0e, _): add(.camera)
            default: break
            }
        }
        return out
    }
}

public enum USBRegistry {
    /// Interface classes of the device at `location` (Longcable's 8-hex-digit
    /// locationID), or nil if no such device is attached.
    public static func interfaces(location: String) -> [USBInterfaceClass]? {
        guard let want = UInt32(location, radix: 16) else { return nil }
        var iterator: io_iterator_t = 0
        guard IOServiceGetMatchingServices(kIOMainPortDefault, IOServiceMatching("IOUSBHostDevice"), &iterator) == KERN_SUCCESS
        else { return nil }
        defer { IOObjectRelease(iterator) }
        while case let service = IOIteratorNext(iterator), service != 0 {
            defer { IOObjectRelease(service) }
            guard number(service, "locationID") == want else { continue }
            var out: [USBInterfaceClass] = []
            var children: io_iterator_t = 0
            if IORegistryEntryGetChildIterator(service, kIOServicePlane, &children) == KERN_SUCCESS {
                while case let child = IOIteratorNext(children), child != 0 {
                    defer { IOObjectRelease(child) }
                    guard IOObjectConformsTo(child, "IOUSBHostInterface") != 0 else { continue }
                    out.append(USBInterfaceClass(Int(number(child, "bInterfaceClass") ?? 0),
                                                 Int(number(child, "bInterfaceSubClass") ?? 0),
                                                 Int(number(child, "bInterfaceProtocol") ?? 0)))
                }
                IOObjectRelease(children)
            }
            return out
        }
        return nil
    }

    private static func number(_ s: io_service_t, _ key: String) -> UInt32? {
        (IORegistryEntryCreateCFProperty(s, key as CFString, kCFAllocatorDefault, 0)?.takeRetainedValue() as? NSNumber)?.uint32Value
    }
}

/// Which USB devices may be offered for handoff, and with what caveat.
public enum HandoffPolicy {
    public enum Verdict: Equatable, Sendable {
        case ok
        /// Allowed after the owner confirms.
        case warn(String)
        /// Not selectable.
        case refuse(String)
    }

    /// True when `device` is (or feeds) the audio input Tightwire streams.
    /// USB audio inputs carry the device's serial or location in their
    /// CoreAudio UID ("AppleUSBAudioEngine:Vendor:Name:<serial|location>:n").
    /// Wave Link's virtual inputs read from Elgato USB audio hardware, so with
    /// a Wave Link input selected every Elgato audio interface counts.
    public static func feedsStream(vendor: UInt16, serial: String, location: String?, roles: [USBDeviceRole],
                                   inputUID: String, inputName: String) -> Bool {
        guard roles.contains(.audio) else { return false }
        let parts = inputUID.split(separator: ":", omittingEmptySubsequences: false).map(String.init)
        if parts.first == "AppleUSBAudioEngine", parts.count >= 4 {
            let tag = parts[3]
            if !serial.isEmpty, tag == serial { return true }
            func trimmed(_ s: String) -> String { String(s.drop { $0 == "0" }).lowercased() }
            if let location, !tag.isEmpty, trimmed(tag) == trimmed(location) { return true }
        }
        let waveLink = inputUID.localizedCaseInsensitiveContains("WaveLink") || inputName.localizedCaseInsensitiveContains("Wave Link")
        return waveLink && vendor == HandoffDevice.elgatoVendor
    }

    public static func verdict(name: String, roles: [USBDeviceRole], feedsStream: Bool) -> Verdict {
        if feedsStream {
            return .refuse("This is the audio interface Tightwire streams from. Choose a different input first.")
        }
        guard !roles.isEmpty else { return .ok }
        let what = roles.map(\.rawValue).joined(separator: ", ")
        return .warn("\(name) is a USB \(what). While it is on the PC it disappears from this Mac.")
    }

    /// Suggested by default: Elgato devices that are not audio, storage or
    /// input hardware, i.e. the Stream Deck family. Never the XLR Dock.
    public static func suggested(vendor: UInt16, roles: [USBDeviceRole]) -> Bool {
        vendor == HandoffDevice.elgatoVendor && roles.isEmpty
    }
}
