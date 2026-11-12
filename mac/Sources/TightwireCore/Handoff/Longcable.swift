import Darwin
import Foundation

/// Tightwire's view of Longcable's local control interface (SPEC §9).
///
/// Mirrors the JSON of Longcable's `ControlRequest` / `ControlReply` /
/// `ServerStatus` / `DeviceStatus` / `LocalUSBDevice` (vendor/longcable,
/// Sources/LongcableCore/Models.swift and USBHardware.swift at dd0dcea), with
/// every field optional so a newer Longcable that adds or drops fields
/// degrades to "unknown" instead of a decode failure. No Longcable code is
/// linked; this is a separate implementation of its socket protocol.
public enum Longcable {
    public static let socketPath = "/var/run/longcable/control.sock"
    public static let launchdLabel = "org.sapd.longcable"
    public static let usbipPort: UInt16 = 3240

    public struct Device: Codable, Equatable, Sendable {
        /// BUSID, e.g. "1-1".
        public var id: String?
        public var name: String?
        /// "vvvv:pppp", lower-case hex.
        public var model: String?
        public var serial: String?
        public var enabled: Bool?
        public var state: String?
        public var lastError: String?
        /// USB locationID, 8 lower-case hex digits; the same value
        /// `availableDevices` reports for the device before it was added.
        public var location: String?

        public init(id: String?, name: String?, model: String?, serial: String?, enabled: Bool?, state: String?,
                    lastError: String?, location: String? = nil) {
            self.id = id
            self.name = name
            self.model = model
            self.serial = serial
            self.enabled = enabled
            self.state = state
            self.lastError = lastError
            self.location = location
        }

        /// False when Longcable remembers the device but it is unplugged.
        public var present: Bool { state != Longcable.notPluggedIn }

        /// One-line summary for the log.
        public var summary: String {
            "\(id ?? "?") \(model ?? "?") \(name ?? "") enabled=\(enabled.map(String.init) ?? "?") state=\"\(state ?? "?")\""
                + (lastError.map { " lastError=\"\($0)\"" } ?? "")
        }
    }

    /// A connected device that is not yet a share target.
    public struct LocalDevice: Codable, Equatable, Sendable {
        public var vendor: String
        public var product: String
        public var location: String
        public var name: String
        public var serial: String
        public init(vendor: String, product: String, location: String, name: String, serial: String) {
            self.vendor = vendor
            self.product = product
            self.location = location
            self.name = name
            self.serial = serial
        }
    }

    public struct Status: Codable, Equatable, Sendable {
        public var version: String?
        public var listen: String?
        public var peer: String?
        public var devices: [Device]?
        public var allowedPeers: [String]?
        public var availableDevices: [LocalDevice]?
    }

    struct Request: Codable {
        var action: String
        var busID: String?
        var peer: String?
        var device: LocalDevice?
    }

    public struct Reply: Codable, Sendable {
        public var ok: Bool
        public var message: String?
        public var status: Status?
    }

    /// Longcable's target states (Target.swift at dd0dcea). The state string
    /// alone does not say whether a device is shared: `share` flips `enabled`
    /// but leaves the state as it was ("Available on this Mac" after an
    /// unshare), and "Waiting for Windows" appears only after Longcable has
    /// read the device's descriptors, which it does lazily when a USB/IP client
    /// lists or imports devices (or at service start). So "shared" means
    /// `enabled == true` and present; the PC's import is what loads it.
    public static let notPluggedIn = "Not plugged in"
    public static let attachedStates: Set<String> = ["Connecting", "Connected to Windows"]

    /// Finds the Longcable target for a handoff device. Longcable reports a
    /// target's serial as its generated label ("USB0FD90084") until it has read
    /// the descriptors and the real serial afterwards, so the serial is not a
    /// stable key; the BUSID recorded at registration and the USB location are.
    public static func target(for d: HandoffDevice, in s: Status) -> Device? {
        let same = (s.devices ?? []).filter { $0.model?.lowercased() == d.model }
        if let id = d.busID, let t = same.first(where: { $0.id == id }) { return t }
        if let loc = d.location, let t = same.first(where: { $0.location?.lowercased() == loc.lowercased() }) { return t }
        if !d.serial.isEmpty, let t = same.first(where: { $0.serial == d.serial }) { return t }
        // Saved before BUSID/location were recorded: accept an unambiguous model.
        if d.busID == nil, d.location == nil, same.count == 1 { return same[0] }
        return nil
    }

    /// A connected device that is not a Longcable target yet.
    public static func local(for d: HandoffDevice, in s: Status) -> LocalDevice? {
        let same = (s.availableDevices ?? []).filter { UInt16($0.vendor, radix: 16) == d.vendor && UInt16($0.product, radix: 16) == d.product }
        if !d.serial.isEmpty, let l = same.first(where: { $0.serial == d.serial }) { return l }
        if let loc = d.location, let l = same.first(where: { $0.location.lowercased() == loc.lowercased() }) { return l }
        if d.serial.isEmpty, d.location == nil, same.count == 1 { return same[0] }
        return nil
    }

    /// Whether Longcable's allow-list (IPv4 addresses and CIDR ranges) admits `ipv4`.
    public static func peerAllowed(_ ipv4: String, by peers: [String]) -> Bool {
        func parse(_ s: String) -> UInt32? {
            var a = in_addr()
            guard inet_pton(AF_INET, s, &a) == 1 else { return nil }
            return UInt32(bigEndian: a.s_addr)
        }
        guard let ip = parse(ipv4) else { return false }
        return peers.contains { p in
            if p == "any" { return true }
            let parts = p.split(separator: "/")
            guard let net = parse(String(parts[0])) else { return false }
            let bits = parts.count == 2 ? Int(parts[1]) ?? -1 : 32
            guard (0...32).contains(bits) else { return false }
            let mask: UInt32 = bits == 0 ? 0 : UInt32.max << (32 - bits)
            return ip & mask == net & mask
        }
    }

    /// True when `ipv4` itself is listed ("a.b.c.d" or "a.b.c.d/32").
    public static func listsExactly(_ ipv4: String, in peers: [String]) -> Bool {
        peers.contains { $0 == ipv4 || $0 == "\(ipv4)/32" }
    }

    /// Entries wider than one address ("any", or a CIDR shorter than /32):
    /// every machine in them may import shared devices.
    public static func wideEntries(_ peers: [String]) -> [String] {
        peers.filter { p in
            if p == "any" { return true }
            let parts = p.split(separator: "/")
            return parts.count == 2 && (Int(parts[1]) ?? 0) < 32
        }
    }

    /// lastError prefixes meaning another process holds the device (USBHardware.swift).
    public static func isBusy(_ error: String?) -> Bool {
        guard let e = error else { return false }
        return e.hasPrefix("In use by") || e.hasPrefix("Another app")
    }

    public static func decodeReply(_ data: Data) throws -> Reply {
        try JSONDecoder().decode(Reply.self, from: data)
    }
}

/// A device chosen for handoff. Identified by VID:PID plus the hardware
/// serial (from the IORegistry; empty when the device has none), its USB
/// location, and the Longcable BUSID once Tightwire has registered it.
public struct HandoffDevice: Codable, Equatable, Hashable, Sendable, Identifiable {
    public var vendor: UInt16
    public var product: UInt16
    /// Hardware serial, empty when the device has none.
    public var serial: String
    public var name: String
    /// Quit the configured helper app (Stream Deck) if it holds the device.
    public var quitHelperApp: Bool
    /// USB locationID as Longcable prints it ("01133100"). Optional: older
    /// saved settings lack it.
    public var location: String?
    /// Longcable's BUSID ("1-1"), recorded when Tightwire registered it.
    public var busID: String?

    public var id: String { String(format: "%04x:%04x/%@", vendor, product, serial.isEmpty ? (location ?? "") : serial) }
    public var model: String { String(format: "%04x:%04x", vendor, product) }

    public init(vendor: UInt16, product: UInt16, serial: String, name: String, quitHelperApp: Bool? = nil,
                location: String? = nil, busID: String? = nil) {
        self.vendor = vendor
        self.product = product
        self.serial = serial
        self.name = name
        self.quitHelperApp = quitHelperApp ?? (vendor == HandoffDevice.elgatoVendor)
        self.location = location
        self.busID = busID
    }

    public static let elgatoVendor: UInt16 = 0x0fd9

    /// Same physical device (ignores name and options).
    public func sameDevice(_ o: HandoffDevice) -> Bool {
        guard vendor == o.vendor, product == o.product else { return false }
        if !serial.isEmpty, !o.serial.isEmpty { return serial == o.serial }
        if let a = busID, let b = o.busID { return a == b }
        if let a = location, let b = o.location { return a == b }
        return serial == o.serial
    }
}

/// What Tightwire needs from Longcable. Faked in tests.
public protocol LongcableControlling: Sendable {
    func status() throws -> Longcable.Status
    func share(_ busID: String) throws
    func unshare(_ busID: String) throws
    func allowPeer(_ ipv4: String) throws
    /// Removes an entry (IPv4 or CIDR) from the allow-list.
    func forgetPeer(_ peer: String) throws
    /// Registers a connected device as a share target. Longcable shares a new
    /// target at once (and its control API has no way to remove one again), so
    /// callers must unshare it straight after; see `HandoffSequencer.register`.
    func add(_ device: Longcable.LocalDevice) throws
}

public struct LongcableError: Error, CustomStringConvertible, Equatable {
    public let description: String
    public init(_ d: String) { description = d }
}

/// Talks to the running service over its root-owned Unix socket:
/// 4-byte big-endian length, then JSON, in both directions.
public struct LongcableSocketClient: LongcableControlling {
    public let path: String
    public let timeout: TimeInterval

    public init(path: String = Longcable.socketPath, timeout: TimeInterval = 5) {
        self.path = path
        self.timeout = timeout
    }

    public var isInstalled: Bool {
        var st = stat()
        // Same trust check Longcable's own client makes: the socket must be root's.
        return lstat(path, &st) == 0 && st.st_uid == 0 && (st.st_mode & S_IFMT) == S_IFSOCK
    }

    func send(_ request: Longcable.Request) throws -> Longcable.Reply {
        guard isInstalled else { throw LongcableError("Longcable is not installed or not running") }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw LongcableError("socket: \(errno)") }
        defer { close(fd) }
        var tv = timeval(tv_sec: Int(timeout), tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: Array(path.utf8) + [0]) }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard rc == 0 else { throw LongcableError("Could not reach the Longcable service") }
        var uid: uid_t = .max
        var gid: gid_t = .max
        guard getpeereid(fd, &uid, &gid) == 0, uid == 0 else { throw LongcableError("Untrusted Longcable socket") }
        let body = try JSONEncoder().encode(request)
        let len = UInt32(body.count)
        let frame = [UInt8(len >> 24), UInt8(len >> 16 & 0xFF), UInt8(len >> 8 & 0xFF), UInt8(len & 0xFF)] + Array(body)
        guard frame.withUnsafeBytes({ write(fd, $0.baseAddress, $0.count) }) == frame.count else {
            throw LongcableError("Longcable did not accept the request")
        }
        func readExactly(_ n: Int) -> [UInt8]? {
            var out = [UInt8](repeating: 0, count: n)
            var got = 0
            while got < n {
                let r = out.withUnsafeMutableBytes { read(fd, $0.baseAddress! + got, n - got) }
                if r <= 0 { return nil }
                got += r
            }
            return out
        }
        guard let h = readExactly(4) else { throw LongcableError("Longcable did not respond") }
        let n = Int(h[0]) << 24 | Int(h[1]) << 16 | Int(h[2]) << 8 | Int(h[3])
        guard n > 0, n <= 65536, let payload = readExactly(n) else { throw LongcableError("Bad reply from Longcable") }
        return try Longcable.decodeReply(Data(payload))
    }

    func checked(_ r: Longcable.Request) throws -> Longcable.Reply {
        // Status is polled; log only the requests that change something.
        let what = [r.action, r.busID, r.peer, r.device.map { "\($0.vendor):\($0.product)@\($0.location)" }].compactMap { $0 }.joined(separator: " ")
        let reply: Longcable.Reply
        do {
            reply = try send(r)
        } catch {
            AppLog.shared.write("longcable", "\(what) → error: \(error)")
            throw error
        }
        if r.action != "status" {
            AppLog.shared.write("longcable", "\(what) → \(reply.ok ? "ok" : "refused"): \(reply.message ?? "")")
        }
        guard reply.ok else { throw LongcableError(reply.message ?? "Longcable refused \(r.action)") }
        return reply
    }

    public func status() throws -> Longcable.Status {
        guard let s = try checked(.init(action: "status")).status else { throw LongcableError("Longcable sent no status") }
        return s
    }

    public func share(_ busID: String) throws { _ = try checked(.init(action: "share", busID: busID)) }
    public func unshare(_ busID: String) throws { _ = try checked(.init(action: "unshare", busID: busID)) }
    public func allowPeer(_ ipv4: String) throws { _ = try checked(.init(action: "allow-peer", peer: ipv4)) }
    public func forgetPeer(_ peer: String) throws { _ = try checked(.init(action: "forget-peer", peer: peer)) }
    public func add(_ device: Longcable.LocalDevice) throws { _ = try checked(.init(action: "add", device: device)) }
}
