import CryptoKit
import Foundation

/// Directional SipHash keys derived from a pairing's 32-byte root key (SPEC §3a).
public struct SessionKeys: Equatable, Sendable {
    /// Mac → receiver (audio, probe, control).
    public let m2r: [UInt8]
    /// Receiver → Mac (stats, hello).
    public let r2m: [UInt8]

    public init(root: [UInt8]) {
        precondition(root.count == 32, "root key must be 32 bytes")
        m2r = Self.expand(root, salt: [], info: "tightwire v2 m2r", count: 16)
        r2m = Self.expand(root, salt: [], info: "tightwire v2 r2m", count: 16)
    }

    static func expand(_ ikm: [UInt8], salt: [UInt8], info: String, count: Int) -> [UInt8] {
        let key = HKDF<SHA256>.deriveKey(inputKeyMaterial: SymmetricKey(data: ikm), salt: salt,
                                         info: Array(info.utf8), outputByteCount: count)
        return key.withUnsafeBytes { Array($0) }
    }
}

/// Outputs of the pairing key agreement.
public struct PairingSecrets: Sendable {
    public let root: [UInt8]
    public let confirmKey: [UInt8]
    public let sas: UInt32
    public let transcriptHash: [UInt8]

    /// `shared` is the X25519 output; the transcript is commit, response and
    /// reveal exactly as sent.
    public init(shared: [UInt8], commit: [UInt8], response: [UInt8], reveal: [UInt8]) {
        var h = SHA256()
        h.update(data: commit)
        h.update(data: response)
        h.update(data: reveal)
        let th = Array(h.finalize())
        let okm = SessionKeys.expand(shared, salt: th, info: "tightwire v2 pairing", count: 64)
        let sasBytes = SessionKeys.expand(shared, salt: th, info: "tightwire v2 sas", count: 4)
        root = Array(okm[0..<32])
        confirmKey = Array(okm[32..<64])
        sas = sasBytes.reduce(0) { $0 << 8 | UInt32($1) } % 1_000_000
        transcriptHash = th
    }

    public func confirmMAC(role: UInt8) -> [UInt8] {
        var m = HMAC<SHA256>(key: SymmetricKey(data: confirmKey))
        m.update(data: Array("tightwire confirm".utf8))
        m.update(data: [role])
        m.update(data: transcriptHash)
        return Array(Array(m.finalize()).prefix(16))
    }

    /// "123 456"
    public var sasDisplay: String {
        String(format: "%03d %03d", sas / 1000, sas % 1000)
    }
}

public enum Hex {
    public static func decode(_ s: String) -> [UInt8]? {
        let c = Array(s.trimmingCharacters(in: .whitespaces).utf8)
        guard c.count % 2 == 0 else { return nil }
        var out: [UInt8] = []
        out.reserveCapacity(c.count / 2)
        for i in stride(from: 0, to: c.count, by: 2) {
            guard let v = UInt8(String(decoding: c[i..<i + 2], as: UTF8.self), radix: 16) else { return nil }
            out.append(v)
        }
        return out
    }

    public static func encode(_ b: [UInt8]) -> String {
        b.map { String(format: "%02x", $0) }.joined()
    }
}

public func secureRandom(_ count: Int) -> [UInt8] {
    var out = [UInt8](repeating: 0, count: count)
    let status = SecRandomCopyBytes(kSecRandomDefault, count, &out)
    precondition(status == errSecSuccess, "SecRandomCopyBytes failed")
    return out
}

public func secureRandomU64() -> UInt64 {
    secureRandom(8).reduce(0) { $0 << 8 | UInt64($1) }
}
