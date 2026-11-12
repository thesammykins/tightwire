import Foundation

/// The Mac app's diagnostic log: `~/Library/Logs/Tightwire/tightwire.log`,
/// rotated at 1 MB with three old files kept (`.1` newest), the same policy
/// as the Windows tray log. One line per event, local time, a short category.
///
/// Never call this from the Core Audio callback: it takes a lock and does
/// file I/O. Writes are rare (state changes, handoff steps), so the file is
/// opened per write; that keeps rotation and external deletion trivial.
public final class AppLog: @unchecked Sendable {
    public static let shared = AppLog(directory: FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/Tightwire"))

    public let fileURL: URL
    let maxBytes: Int
    let keep: Int
    private let lock = NSLock()
    private let formatter: DateFormatter = {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd HH:mm:ss.SSS"
        return f
    }()

    public init(directory: URL, maxBytes: Int = 1 << 20, keep: Int = 3) {
        fileURL = directory.appendingPathComponent("tightwire.log")
        self.maxBytes = maxBytes
        self.keep = keep
    }

    public func write(_ category: String, _ message: String) {
        lock.lock()
        defer { lock.unlock() }
        let line = "\(formatter.string(from: Date())) [\(category)] \(message)\n"
        let fm = FileManager.default
        try? fm.createDirectory(at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        if let size = (try? fm.attributesOfItem(atPath: fileURL.path))?[.size] as? Int, size + line.utf8.count > maxBytes {
            rotate()
        }
        if !fm.fileExists(atPath: fileURL.path) {
            fm.createFile(atPath: fileURL.path, contents: nil, attributes: [.posixPermissions: 0o600])
        }
        guard let h = try? FileHandle(forWritingTo: fileURL) else { return }
        defer { try? h.close() }
        _ = try? h.seekToEnd()
        try? h.write(contentsOf: Data(line.utf8))
    }

    private func rotate() {
        let fm = FileManager.default
        func old(_ n: Int) -> String { fileURL.path + ".\(n)" }
        try? fm.removeItem(atPath: old(keep))
        if keep > 1 {
            for n in stride(from: keep - 1, through: 1, by: -1) where fm.fileExists(atPath: old(n)) {
                try? fm.moveItem(atPath: old(n), toPath: old(n + 1))
            }
        }
        try? fm.moveItem(atPath: fileURL.path, toPath: old(1))
    }
}
