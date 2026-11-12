import Foundation

/// Version and build of this Tightwire (the receiver's equivalent is
/// `applog::build_label`). `mac/scripts/bundle.sh` reads `version` from this
/// file for CFBundleShortVersionString and stamps the git commit into the
/// app's Info.plist (`TightwireCommit`); SwiftPM builds outside the bundle
/// (tests, `swift run`, the CLI) report "dev".
public enum BuildInfo {
    public static let version = "0.2.0"

    public static var commit: String {
        (Bundle.main.object(forInfoDictionaryKey: "TightwireCommit") as? String).flatMap { $0.isEmpty ? nil : $0 } ?? "dev"
    }

    /// "Tightwire 0.2.0 (abc1234567)".
    public static var label: String { "Tightwire \(version) (\(commit))" }
}
