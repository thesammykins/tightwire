import Darwin
import Foundation

func cString(_ buf: [CChar]) -> String {
    String(decoding: buf.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
}

public struct SocketError: Error, CustomStringConvertible, Sendable {
    public let description: String
    init(_ what: String, _ code: Int32 = errno) {
        description = "\(what): \(String(cString: strerror(code)))"
    }
}

/// An IPv4/IPv6 UDP endpoint.
public struct Endpoint: Equatable, Hashable, Sendable, CustomStringConvertible {
    var storage: sockaddr_storage
    public var length: socklen_t { storage.ss_family == sa_family_t(AF_INET6) ? socklen_t(MemoryLayout<sockaddr_in6>.size) : socklen_t(MemoryLayout<sockaddr_in>.size) }

    init(storage: sockaddr_storage) { self.storage = storage }

    public static func == (a: Endpoint, b: Endpoint) -> Bool { a.description == b.description }
    public func hash(into h: inout Hasher) { h.combine(description) }

    public var description: String {
        var s = storage
        var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
        var serv = [CChar](repeating: 0, count: Int(NI_MAXSERV))
        _ = withUnsafePointer(to: &s) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                getnameinfo($0, length, &host, socklen_t(host.count), &serv, socklen_t(serv.count), NI_NUMERICHOST | NI_NUMERICSERV)
            }
        }
        let h = cString(host)
        return h.contains(":") ? "[\(h)]:\(cString(serv))" : "\(h):\(cString(serv))"
    }

    public var host: String {
        let d = description
        guard let i = d.lastIndex(of: ":") else { return d }
        return String(d[..<i]).trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
    }

    public var port: UInt16 {
        UInt16(description.split(separator: ":").last.flatMap { UInt16($0) } ?? 0)
    }

    /// Resolves "host" or "host:port" (blocking; may hit DNS/mDNS). IPv4 preferred.
    public static func resolve(_ text: String, defaultPort: UInt16) throws -> Endpoint {
        var host = text.trimmingCharacters(in: .whitespaces)
        var port = defaultPort
        if host.hasPrefix("["), let close = host.firstIndex(of: "]") {
            let rest = host[host.index(after: close)...]
            if rest.hasPrefix(":"), let p = UInt16(rest.dropFirst()) { port = p }
            host = String(host[host.index(after: host.startIndex)..<close])
        } else if host.filter({ $0 == ":" }).count == 1, let i = host.lastIndex(of: ":") {
            guard let p = UInt16(host[host.index(after: i)...]) else { throw SocketError("bad port in \(text)", EINVAL) }
            port = p
            host = String(host[..<i])
        }
        var hints = addrinfo(ai_flags: AI_ADDRCONFIG, ai_family: AF_UNSPEC, ai_socktype: SOCK_DGRAM,
                             ai_protocol: IPPROTO_UDP, ai_addrlen: 0, ai_canonname: nil, ai_addr: nil, ai_next: nil)
        var res: UnsafeMutablePointer<addrinfo>?
        let rc = getaddrinfo(host, String(port), &hints, &res)
        guard rc == 0, let first = res else {
            throw SocketError("cannot resolve \(host): \(String(cString: gai_strerror(rc)))", 0)
        }
        defer { freeaddrinfo(res) }
        var pick = first
        var p: UnsafeMutablePointer<addrinfo>? = first
        while let a = p {
            if a.pointee.ai_family == AF_INET { pick = a; break }
            p = a.pointee.ai_next
        }
        var ss = sockaddr_storage()
        memcpy(&ss, pick.pointee.ai_addr, Int(pick.pointee.ai_addrlen))
        return Endpoint(storage: ss)
    }

    var family: Int32 { Int32(storage.ss_family) }

    /// This Mac's IPv4 address on the route to `self` (UDP connect only
    /// consults the routing table; nothing is sent). nil for IPv6 or no route.
    public var localIPv4Toward: String? {
        guard family == AF_INET else { return nil }
        let fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard fd >= 0 else { return nil }
        defer { close(fd) }
        var s = storage
        let ok = withUnsafePointer(to: &s) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, length) }
        } == 0
        guard ok else { return nil }
        var local = sockaddr_in()
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        let got = withUnsafeMutablePointer(to: &local) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &len) }
        } == 0
        guard got, local.sin_addr.s_addr != 0 else { return nil }
        var buf = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
        inet_ntop(AF_INET, &local.sin_addr, &buf, socklen_t(buf.count))
        return cString(buf)
    }
}

/// Thin wrapper over a non-blocking UDP socket. Not used on the audio thread
/// (the C packetizer uses the raw fd).
public final class UDPSocket: @unchecked Sendable {
    public let fd: Int32

    /// Binds to `port` on all interfaces, falling back to an ephemeral port
    /// if it is taken (another Tightwire instance).
    /// `shared` lets several sockets bind the same port and receive broadcasts
    /// (beacon listeners).
    public init(family: Int32 = AF_INET, preferredPort: UInt16, fallbackToEphemeral: Bool = true, shared: Bool = false) throws {
        fd = socket(family, SOCK_DGRAM, IPPROTO_UDP)
        guard fd >= 0 else { throw SocketError("socket") }
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        if shared {
            setsockopt(fd, SOL_SOCKET, SO_BROADCAST, &one, socklen_t(MemoryLayout<Int32>.size))
            setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, socklen_t(MemoryLayout<Int32>.size))
            setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one, socklen_t(MemoryLayout<Int32>.size))
        }
        // Expedited forwarding class for voice (DSCP EF); see SPEC deviations.
        var svc = Int32(NET_SERVICE_TYPE_VO)
        setsockopt(fd, SOL_SOCKET, SO_NET_SERVICE_TYPE, &svc, socklen_t(MemoryLayout<Int32>.size))
        let flags = fcntl(fd, F_GETFL)
        _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK)
        if try !bind(family: family, port: preferredPort) {
            guard fallbackToEphemeral, try bind(family: family, port: 0) else {
                close(fd)
                throw SocketError("bind \(preferredPort)")
            }
        }
    }

    private func bind(family: Int32, port: UInt16) throws -> Bool {
        if family == AF_INET6 {
            var a = sockaddr_in6()
            a.sin6_family = sa_family_t(AF_INET6)
            a.sin6_port = port.bigEndian
            a.sin6_addr = in6addr_any
            return withUnsafePointer(to: &a) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in6>.size)) == 0 }
            }
        }
        var a = sockaddr_in()
        a.sin_family = sa_family_t(AF_INET)
        a.sin_port = port.bigEndian
        a.sin_addr = in_addr(s_addr: INADDR_ANY)
        return withUnsafePointer(to: &a) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0 }
        }
    }

    deinit { close(fd) }

    public var localPort: UInt16 {
        var ss = sockaddr_storage()
        var len = socklen_t(MemoryLayout<sockaddr_storage>.size)
        _ = withUnsafeMutablePointer(to: &ss) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &len) } }
        return Endpoint(storage: ss).port
    }

    /// Restricts the socket to one peer (send() goes there; others are filtered by the kernel).
    public func connect(_ to: Endpoint) throws {
        var s = to.storage
        let rc = withUnsafePointer(to: &s) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, to.length) }
        }
        guard rc == 0 else { throw SocketError("connect \(to)") }
    }

    @discardableResult
    public func send(_ data: [UInt8]) -> Bool {
        data.withUnsafeBytes { Darwin.send(fd, $0.baseAddress, $0.count, 0) } == data.count
    }

    @discardableResult
    public func send(_ data: [UInt8], to: Endpoint) -> Bool {
        var s = to.storage
        return data.withUnsafeBytes { b in
            withUnsafePointer(to: &s) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { sendto(fd, b.baseAddress, b.count, 0, $0, to.length) }
            }
        } == data.count
    }

    /// Waits up to `timeout` for one datagram.
    public func receive(timeout: TimeInterval) -> (data: [UInt8], from: Endpoint)? {
        var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
        guard poll(&pfd, 1, Int32(timeout * 1000)) > 0 else { return nil }
        var buf = [UInt8](repeating: 0, count: 2048)
        var ss = sockaddr_storage()
        var len = socklen_t(MemoryLayout<sockaddr_storage>.size)
        let n = withUnsafeMutablePointer(to: &ss) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { recvfrom(fd, &buf, buf.count, 0, $0, &len) }
        }
        guard n > 0 else { return nil }
        return (Array(buf[..<n]), Endpoint(storage: ss))
    }
}
