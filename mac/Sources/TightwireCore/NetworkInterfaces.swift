import Darwin
import Foundation
import SystemConfiguration
import dnssd

/// An IPv4 network this Mac is attached to (address and netmask, host order).
public struct IPv4Subnet: Hashable, Sendable {
    public let address: UInt32
    public let mask: UInt32

    public init(address: UInt32, mask: UInt32) {
        self.address = address
        self.mask = mask
    }

    public func contains(_ ip: UInt32) -> Bool { ip & mask == address & mask }

    public var addressText: String {
        [24, 16, 8, 0].map { String((address >> $0) & 0xff) }.joined(separator: ".")
    }
}

/// A network interface the stream can be pinned to, such as the built-in
/// Ethernet on a direct cable to the PC while Wi-Fi carries everything else.
public struct NetworkInterface: Identifiable, Hashable, Sendable {
    /// BSD name ("en0"): what is stored and what IP_BOUND_IF takes.
    public let name: String
    /// System Settings' name ("Ethernet", "Thunderbolt Bridge"), or the BSD name.
    public let displayName: String
    public let subnets: [IPv4Subnet]
    public var id: String { name }

    public init(name: String, displayName: String, subnets: [IPv4Subnet]) {
        self.name = name
        self.displayName = displayName
        self.subnets = subnets
    }

    /// IPv4 addresses for a `.local` name as answered on this interface only.
    /// getaddrinfo hands back just the first mDNS answer, which for a PC on
    /// both a LAN and a direct cable is usually the LAN one.
    public func multicastLookup(_ host: String, port: UInt16, timeout: TimeInterval = 2) -> [Endpoint] {
        final class Answers { var found: [Endpoint] = []; var more = true }
        let answers = Answers()
        let index = if_nametoindex(name)
        guard index != 0 else { return [] }
        var ref: DNSServiceRef?
        let callback: DNSServiceGetAddrInfoReply = { _, flags, _, error, _, addr, _, ctx in
            guard let ctx else { return }
            let a = Unmanaged<Answers>.fromOpaque(ctx).takeUnretainedValue()
            a.more = flags & kDNSServiceFlagsMoreComing != 0
            guard error == kDNSServiceErr_NoError, flags & kDNSServiceFlagsAdd != 0,
                  let addr, addr.pointee.sa_family == sa_family_t(AF_INET) else { return }
            var ss = sockaddr_storage()
            memcpy(&ss, addr, MemoryLayout<sockaddr_in>.size)
            a.found.append(Endpoint(storage: ss))
        }
        let ctx = Unmanaged.passUnretained(answers).toOpaque()
        let err = DNSServiceGetAddrInfo(&ref, DNSServiceFlags(kDNSServiceFlagsTimeout), index,
                                        DNSServiceProtocol(kDNSServiceProtocol_IPv4), host, callback, ctx)
        guard err == kDNSServiceErr_NoError, let ref else { return [] }
        defer { DNSServiceRefDeallocate(ref) }
        let deadline = Date().addingTimeInterval(timeout)
        var pfd = pollfd(fd: DNSServiceRefSockFD(ref), events: Int16(POLLIN), revents: 0)
        // Stop at the first complete batch of answers, or the deadline.
        while answers.found.isEmpty || answers.more {
            let left = deadline.timeIntervalSinceNow
            guard left > 0, poll(&pfd, 1, Int32(left * 1000)) > 0,
                  DNSServiceProcessResult(ref) == kDNSServiceErr_NoError else { break }
        }
        return answers.found.map { e in
            var e = e
            withUnsafeMutableBytes(of: &e.storage) { $0.withMemoryRebound(to: sockaddr_in.self) { $0[0].sin_port = port.bigEndian } }
            return e
        }
    }

    /// True when `endpoint` is an IPv4 address on one of this interface's
    /// networks, so it is reached directly over this link and not through a router.
    public func isOnLink(_ endpoint: Endpoint) -> Bool {
        guard let ip = endpoint.ipv4 else { return false }
        return subnets.contains { $0.contains(ip) }
    }
}

public enum NetworkInterfaces {
    /// Interfaces that are up, not loopback, and have an IPv4 address.
    public static func list() -> [NetworkInterface] {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0 else { return [] }
        defer { freeifaddrs(head) }
        var order: [String] = []
        var subnets: [String: [IPv4Subnet]] = [:]
        var p = head
        while let a = p {
            p = a.pointee.ifa_next
            let flags = Int32(a.pointee.ifa_flags)
            guard flags & IFF_UP != 0, flags & IFF_LOOPBACK == 0,
                  let addr = a.pointee.ifa_addr, addr.pointee.sa_family == sa_family_t(AF_INET),
                  let mask = a.pointee.ifa_netmask else { continue }
            let name = String(cString: a.pointee.ifa_name)
            let ip = addr.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { UInt32(bigEndian: $0.pointee.sin_addr.s_addr) }
            let m = mask.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { UInt32(bigEndian: $0.pointee.sin_addr.s_addr) }
            if subnets[name] == nil { order.append(name) }
            subnets[name, default: []].append(IPv4Subnet(address: ip, mask: m))
        }
        let names = displayNames()
        return order.map { NetworkInterface(name: $0, displayName: names[$0] ?? $0, subnets: subnets[$0] ?? []) }
    }

    /// The named interface while it is up with an IPv4 address; nil when it
    /// is unplugged, unconfigured or gone.
    public static func find(_ name: String) -> NetworkInterface? {
        list().first { $0.name == name }
    }

    private static func displayNames() -> [String: String] {
        guard let all = SCNetworkInterfaceCopyAll() as? [SCNetworkInterface] else { return [:] }
        var out: [String: String] = [:]
        for i in all {
            if let bsd = SCNetworkInterfaceGetBSDName(i) as String?,
               let shown = SCNetworkInterfaceGetLocalizedDisplayName(i) as String? {
                out[bsd] = shown
            }
        }
        return out
    }
}
