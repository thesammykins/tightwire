import Darwin
import Foundation
import TightwireCore

/// `tightwire-send handoff-check`: runs the app's real handover/handback
/// sequence (HandoffSequencer against the running Longcable service and the
/// real Stream Deck app) for one device, with this Mac standing in for the
/// PC. The stand-in imports the device over USB/IP from `--client-ip`, which
/// must be one of this Mac's own addresses that Longcable already allows
/// (e.g. its Tailscale address when the allow-list is the tailnet), so the
/// check changes no Longcable settings. Ends with everything unshared.
func runHandoffCheck(model: String, clientIP: String, hold: TimeInterval) -> Int32 {
    guard let (v, p) = splitModel(model) else { fail("--device must be VID:PID, e.g. 0fd9:0084") }
    let longcable = LongcableSocketClient()
    guard longcable.isInstalled else { fail("Longcable is not running") }
    @Sendable func say(_ s: String) { print(s); fflush(stdout) }
    func statusLine(_ id: String?) -> String {
        guard let s = try? longcable.status(), let t = s.devices?.first(where: { $0.id == id }) else { return "not listed" }
        return t.summary
    }

    let status: Longcable.Status
    do { status = try longcable.status() } catch { fail("Longcable status: \(error)") }
    let same = (status.devices ?? []).filter { $0.model == model }
    guard same.count == 1, let target = same.first, let busID = target.id else {
        fail("expected exactly one Longcable target \(model), found \(same.count); register it in Tightwire first")
    }
    let device = HandoffDevice(vendor: v, product: p, serial: "", name: target.name ?? model, location: target.location, busID: busID)
    say("target: \(target.summary)")
    say("allowed peers: \((status.allowedPeers ?? []).joined(separator: ", "))")
    guard target.enabled == false else { fail("\(busID) is shared already; unshare it first") }

    let apps = WorkspaceApps()
    let transport = LocalImportTransport(host: clientIP, hold: hold)
    let seq = HandoffSequencer(longcable: longcable, apps: apps, transport: transport) { say("  [sequencer] \($0)") }
    var code: Int32 = 0
    do {
        let attached = try seq.handOver([device], pcAddress: clientIP)
        say("handover OK: \(attached.joined(separator: ", ")) imported by \(clientIP)")
        say("during import: \(statusLine(busID))")
        say("Stream Deck app running: \(apps.isRunning(HandoffSequencer.defaultHelperBundleID))")
    } catch {
        say("handover FAILED: \(error)")
        code = 1
    }
    let still = seq.handBack([device])
    say("after handback: \(statusLine(busID))")
    if !still.isEmpty {
        say("STILL SHARED: \(still)")
        code = 1
    }
    // Last line of defence: never leave the target shared.
    if let s = try? longcable.status(), s.devices?.first(where: { $0.id == busID })?.enabled == true {
        try? longcable.unshare(busID)
        say("forced unshare of \(busID)")
        code = 1
    }
    return code
}

private func splitModel(_ m: String) -> (UInt16, UInt16)? {
    let parts = m.split(separator: ":")
    guard parts.count == 2, let v = UInt16(parts[0], radix: 16), let p = UInt16(parts[1], radix: 16) else { return nil }
    return (v, p)
}

/// Stands in for the PC: OP_REQ_IMPORT (USB/IP 1.1.1, opcode 0x8003) per
/// BUSID; keeps the connection open, as an attached client would, until
/// release closes it.
final class LocalImportTransport: HandoffTransport, @unchecked Sendable {
    let host: String
    let hold: TimeInterval
    private var sockets: [String: Int32] = [:]

    init(host: String, hold: TimeInterval) {
        self.host = host
        self.hold = hold
    }

    func offer(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
        busIDs.map { id in
            do {
                let fd = try importDevice(id, timeout: timeout)
                sockets[id] = fd
                Thread.sleep(forTimeInterval: hold)
                return Wire.HandoffItem(busID: id, result: .ok, port: 1, message: "")
            } catch {
                return Wire.HandoffItem(busID: id, result: .failed, port: 0, message: "\(error)")
            }
        }
    }

    func release(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
        busIDs.map { id in
            if let fd = sockets.removeValue(forKey: id) { close(fd) }
            return Wire.HandoffItem(busID: id, result: .ok, port: 0, message: "")
        }
    }

    private func importDevice(_ busID: String, timeout: TimeInterval) throws -> Int32 {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw CLIError(description: "socket: \(errno)") }
        var tv = timeval(tv_sec: Int(timeout), tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = Longcable.usbipPort.bigEndian
        inet_pton(AF_INET, host, &addr.sin_addr)
        let rc = withUnsafePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) } }
        guard rc == 0 else { close(fd); throw CLIError(description: "connect \(host):3240: \(String(cString: strerror(errno)))") }
        var req: [UInt8] = [0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0]
        var bus = Array(busID.utf8.prefix(31))
        bus += [UInt8](repeating: 0, count: 32 - bus.count)
        req += bus
        guard write(fd, req, req.count) == req.count else { close(fd); throw CLIError(description: "write failed") }
        func readN(_ n: Int) -> [UInt8]? {
            var out = [UInt8](repeating: 0, count: n), got = 0
            while got < n {
                let r = out.withUnsafeMutableBytes { read(fd, $0.baseAddress! + got, n - got) }
                if r <= 0 { return nil }
                got += r
            }
            return out
        }
        guard let h = readN(8) else { close(fd); throw CLIError(description: "Longcable closed the connection (peer not allowed?)") }
        let st = UInt32(h[4]) << 24 | UInt32(h[5]) << 16 | UInt32(h[6]) << 8 | UInt32(h[7])
        guard h[2] == 0x00, h[3] == 0x03, st == 0, readN(312) != nil else {
            close(fd)
            throw CLIError(description: "import refused (status \(st))")
        }
        return fd
    }
}
