import Foundation
import Security

/// The root shell command that installs Longcable's service from the copy
/// embedded in Tightwire.app (SPEC §10, security).
///
/// The bundle lives in a user-writable place, and the command runs as root,
/// so nothing is executed from the bundle in place:
/// 1. the whole app is copied into a fresh root-owned `mktemp -d` directory
///    (0700 under /private/tmp; the user can no longer change the copy);
/// 2. the copy is verified with `codesign --verify --strict --deep` against
///    the requirement `cdhash H"…"` of the *running* app's own signature, so
///    the payload must be byte-for-byte what was sealed when this app was
///    signed (a modified bundle on disk fails, even if re-signed);
/// 3. Longcable's installer runs from the copy under `env -i` with a fixed
///    PATH: `do shell script` inherits the app's environment (Apple TN2065),
///    and variables such as BASH_ENV or DYLD_* must not reach a root shell;
/// 4. the copy is removed whatever the outcome.
public enum HelperInstallCommand {
    public static let safePath = "/usr/bin:/bin:/usr/sbin:/sbin"

    public static func shellQuote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// `script`: "install-mac.sh" or "uninstall-mac.sh"; `args` already plain
    /// words (an IPv4 address and 0.0.0.0), quoted here anyway.
    public static func make(appPath: String, cdhash: String, script: String, args: [String]) -> String {
        precondition(cdhash.allSatisfy(\.isHexDigit) && !cdhash.isEmpty, "cdhash must be hex")
        let req = "cdhash H\"\(cdhash.lowercased())\""
        let scriptPath = "\"$T/Tightwire.app/Contents/Resources/longcable/scripts/\(script)\""
        let argText = args.map(shellQuote).joined(separator: " ")
        return [
            "T=$(/usr/bin/mktemp -d /private/tmp/tightwire-helper.XXXXXX) || exit 1",
            "/bin/chmod 700 \"$T\"",
            "trap '/bin/rm -rf \"$T\"' EXIT",
            "/usr/bin/ditto \(shellQuote(appPath)) \"$T/Tightwire.app\" || exit 1",
            "/usr/bin/codesign --verify --strict --deep -R=\(shellQuote(req)) \"$T/Tightwire.app\" 2>&1 || { echo 'Tightwire.app does not match the running app; not installing'; exit 1; }",
            "/usr/bin/env -i PATH=\(safePath) /bin/bash \(scriptPath)\(script == "install-mac.sh" ? " --skip-build" : "")\(argText.isEmpty ? "" : " " + argText) 2>&1",
        ].joined(separator: "; ")
    }

    /// The running process's code-directory hash (hex), from its own
    /// signature (ad-hoc or Developer ID). nil if unsigned.
    public static func runningCDHash() -> String? {
        var code: SecCode?
        guard SecCodeCopySelf([], &code) == errSecSuccess, let code else { return nil }
        var staticCode: SecStaticCode?
        guard SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode else { return nil }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess,
              let dict = info as? [String: Any], let unique = dict[kSecCodeInfoUnique as String] as? Data
        else { return nil }
        return unique.map { String(format: "%02x", $0) }.joined()
    }
}
