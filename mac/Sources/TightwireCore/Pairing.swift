import CryptoKit
import Foundation
import Security

/// The one receiver this Mac streams to (SPEC §3a).
public struct Pairing: Codable, Equatable, Sendable {
    public var peerName: String
    public var peerID: [UInt8]
    public var rootKey: [UInt8]
    /// Last address the receiver answered from ("host:port").
    public var lastAddress: String?
    /// Receiver's announced host name, re-resolved when the address stops answering.
    public var hostName: String?

    public init(peerName: String, peerID: [UInt8], rootKey: [UInt8], lastAddress: String?, hostName: String?) {
        self.peerName = peerName
        self.peerID = peerID
        self.rootKey = rootKey
        self.lastAddress = lastAddress
        self.hostName = hostName
    }

    public var keys: SessionKeys { SessionKeys(root: rootKey) }

    /// Short non-secret fingerprint for display and test comparison.
    public var fingerprint: String { Hex.encode(Array(SHA256.hash(data: rootKey).prefix(4))) }
}

public protocol PairingStore: Sendable {
    func load() throws -> Pairing?
    func save(_ p: Pairing) throws
    func forget() throws
    /// This Mac's persistent 16-byte id.
    func selfID() throws -> [UInt8]
}

public struct StoreError: Error, CustomStringConvertible {
    public let description: String
    public init(description: String) { self.description = description }
}

/// Keychain storage (generic password items, service "Tightwire pairing").
public struct KeychainPairingStore: PairingStore {
    let service = "Tightwire pairing"

    public init() {}

    private func query(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account]
    }

    private func read(_ account: String) throws -> Data? {
        var q = query(account)
        q[kSecReturnData as String] = true
        var out: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &out)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw StoreError(description: "Keychain read failed (\(status))") }
        return out as? Data
    }

    private func write(_ account: String, _ data: Data) throws {
        let status = SecItemUpdate(query(account) as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            var q = query(account)
            q[kSecValueData as String] = data
            q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            let add = SecItemAdd(q as CFDictionary, nil)
            guard add == errSecSuccess else { throw StoreError(description: "Keychain add failed (\(add))") }
        } else if status != errSecSuccess {
            throw StoreError(description: "Keychain update failed (\(status))")
        }
    }

    public func load() throws -> Pairing? {
        try read("pairing").map { try JSONDecoder().decode(Pairing.self, from: $0) }
    }

    public func save(_ p: Pairing) throws {
        try write("pairing", JSONEncoder().encode(p))
    }

    public func forget() throws {
        let status = SecItemDelete(query("pairing") as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw StoreError(description: "Keychain delete failed (\(status))")
        }
    }

    public func selfID() throws -> [UInt8] {
        if let d = try read("self-id"), d.count == Wire.idLength { return Array(d) }
        let id = secureRandom(Wire.idLength)
        try write("self-id", Data(id))
        return id
    }
}

/// File storage for the CLI and tests (owner-only permissions).
public struct FilePairingStore: PairingStore {
    public let url: URL
    public init(url: URL) { self.url = url }

    struct Contents: Codable {
        var selfID: [UInt8]
        var pairing: Pairing?
    }

    private func contents() throws -> Contents {
        guard FileManager.default.fileExists(atPath: url.path) else {
            return Contents(selfID: secureRandom(Wire.idLength), pairing: nil)
        }
        return try JSONDecoder().decode(Contents.self, from: Data(contentsOf: url))
    }

    /// The file holds the root key: it is created 0600 (O_EXCL temp file,
    /// then rename), so it is never readable by others, not even briefly.
    private func store(_ c: Contents) throws {
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let data = try JSONEncoder().encode(c)
        let tmp = url.path + ".tmp"
        unlink(tmp)
        let fd = open(tmp, O_WRONLY | O_CREAT | O_EXCL, 0o600)
        guard fd >= 0 else { throw StoreError(description: "cannot create \(tmp): \(String(cString: strerror(errno)))") }
        let written = data.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        let synced = fsync(fd) == 0
        close(fd)
        guard written == data.count, synced, rename(tmp, url.path) == 0 else {
            unlink(tmp)
            throw StoreError(description: "cannot write \(url.path)")
        }
    }

    public func load() throws -> Pairing? { try contents().pairing }
    public func save(_ p: Pairing) throws {
        var c = try contents()
        c.pairing = p
        try store(c)
    }
    public func forget() throws {
        var c = try contents()
        c.pairing = nil
        try store(c)
    }
    public func selfID() throws -> [UInt8] {
        let c = try contents()
        if !FileManager.default.fileExists(atPath: url.path) { try store(c) }
        return c.selfID
    }
}

/// Initiator half of the pairing handshake (SPEC §3a). Blocking; run it off
/// the main thread. `decide` is called once with the confirmation code and
/// must return the user's verdict (it may block while the UI asks).
public final class PairingInitiator: @unchecked Sendable {
    public enum Failure: Error, CustomStringConvertible {
        case timeout, rejectedHere, aborted(String), network(String)
        public var description: String {
            switch self {
            case .timeout: "the receiver stopped answering"
            case .rejectedHere: "cancelled"
            case .aborted(let m): m
            case .network(let m): m
            }
        }
    }

    private let target: Endpoint
    private let name: String
    private let selfID: [UInt8]
    private let timeout: TimeInterval
    private let cancelled = Locked(false)

    public init(target: Endpoint, name: String, selfID: [UInt8], timeout: TimeInterval = 120) {
        self.target = target
        self.name = Wire.outgoingName(name)
        self.selfID = selfID
        self.timeout = timeout
    }

    public func cancel() { cancelled.value = true }

    public func run(decide: @escaping @Sendable (_ code: String, _ peerName: String) -> Bool) throws -> Pairing {
        let sock = try UDPSocket(family: target.family, preferredPort: 0)
        let secret = Curve25519.KeyAgreement.PrivateKey()
        let pub = Array(secret.publicKey.rawRepresentation)
        let commit = Wire.encode(.commit(id: selfID, commitment: Array(SHA256.hash(data: pub)), name: name))
        let deadline = Date().addingTimeInterval(timeout)

        func exchange(_ out: [UInt8], until accept: ([UInt8]) -> Bool) throws {
            var lastSend = Date.distantPast
            while Date() < deadline {
                if cancelled.value { throw Failure.rejectedHere }
                if Date().timeIntervalSince(lastSend) >= 0.5 {
                    sock.send(out, to: target)
                    lastSend = Date()
                }
                guard let (data, from) = sock.receive(timeout: 0.1), from == target else { continue }
                if case .abort(let r) = try? Wire.decode(data) {
                    throw Failure.aborted(r == 2 ? "receiver reported a key mismatch" : "cancelled on the receiver")
                }
                if accept(data) { return }
            }
            throw Failure.timeout
        }

        var response: [UInt8] = []
        var peer: (id: [UInt8], key: [UInt8], name: String)?
        try exchange(commit) { d in
            guard case .response(let id, let key, let name) = try? Wire.decode(d) else { return false }
            response = d
            peer = (id, key, name)
            return true
        }
        guard let peer else { throw Failure.timeout }
        let reveal = Wire.encode(.reveal(publicKey: pub))
        let shared: [UInt8]
        do {
            let peerKey = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: peer.key)
            shared = try secret.sharedSecretFromKeyAgreement(with: peerKey).withUnsafeBytes { Array($0) }
        } catch {
            throw Failure.aborted("receiver sent an invalid key")
        }
        guard shared.contains(where: { $0 != 0 }) else { throw Failure.aborted("receiver sent a degenerate key") }
        let secrets = PairingSecrets(shared: shared, commit: commit, response: response, reveal: reveal)
        // The receiver only needs the reveal to show its code; send it a few
        // times, then ask our user while the confirm loop keeps it fresh.
        for _ in 0..<3 { sock.send(reveal, to: target) }

        let verdict = Locked<Bool?>(nil)
        let code = secrets.sasDisplay
        DispatchQueue.global().async { verdict.value = decide(code, peer.name) }

        let myConfirm = Wire.encode(.confirm(role: 0, mac: secrets.confirmMAC(role: 0)))
        let theirs = secrets.confirmMAC(role: 1)
        var peerConfirmed = false
        var lastSend = Date.distantPast
        while Date() < deadline {
            if cancelled.value || verdict.value == false {
                sock.send(Wire.encode(.abort(reason: 1)), to: target)
                throw Failure.rejectedHere
            }
            let accepted = verdict.value == true
            if Date().timeIntervalSince(lastSend) >= 0.5 {
                // Until our user decides, keep resending the reveal (cheap,
                // idempotent) in case it was lost; afterwards our confirm.
                sock.send(accepted ? myConfirm : reveal, to: target)
                lastSend = Date()
            }
            if accepted && peerConfirmed { break }
            guard let (data, from) = sock.receive(timeout: 0.1), from == target else { continue }
            switch try? Wire.decode(data) {
            case .confirm(1, let mac) where Wire.constantTimeEqual(mac, theirs):
                peerConfirmed = true
                if accepted { sock.send(myConfirm, to: target) }
            case .abort(let r):
                throw Failure.aborted(r == 2 ? "receiver reported a key mismatch" : "codes rejected on the receiver")
            default: break
            }
        }
        guard verdict.value == true, peerConfirmed else { throw Failure.timeout }
        // The receiver only finishes once it has our confirm, and nothing acks
        // it. When the receiver's user answers first, the loop above can exit
        // inside the 0.5 s resend window without ever having sent it, leaving
        // this side paired and the receiver not. So always send it now, and
        // keep answering the receiver's confirm resends for a moment in case
        // ours is lost.
        sock.send(myConfirm, to: target)
        let lingerEnd = Date().addingTimeInterval(1.0)
        while Date() < lingerEnd {
            guard let (data, from) = sock.receive(timeout: 0.1), from == target else { continue }
            if case .confirm(1, let mac) = try? Wire.decode(data), Wire.constantTimeEqual(mac, theirs) {
                sock.send(myConfirm, to: target)
            }
        }
        return Pairing(peerName: peer.name, peerID: peer.id, rootKey: secrets.root,
                       lastAddress: Endpoint(storage: target.storage).description, hostName: nil)
    }
}

/// Small lock-protected box for state shared with background threads.
public final class Locked<T>: @unchecked Sendable {
    private var v: T
    private let lock = NSLock()
    public init(_ v: T) { self.v = v }
    public var value: T {
        get { lock.withLock { v } }
        set { lock.withLock { v = newValue } }
    }
    @discardableResult
    public func update<R>(_ f: (inout T) -> R) -> R { lock.withLock { f(&v) } }
}
