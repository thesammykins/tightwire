import AppKit
import AVFoundation
import Foundation
import ServiceManagement
import TightwireCore

/// SPEC §10 setup checklist. Every row is detected from the system each time
/// it is shown, never from a remembered flag.
struct SetupStatus: Equatable {
    var microphone: AVAuthorizationStatus
    var helperInstalled: Bool
    var helperRunning: Bool
    var helperBundled: Bool
    var launchAtLogin: SMAppService.Status
    var paired: Bool

    var complete: Bool {
        microphone == .authorized && helperRunning && launchAtLogin == .enabled && paired
    }

    /// Before the first detection, and for previews.
    static let placeholder = SetupStatus(microphone: .notDetermined, helperInstalled: false, helperRunning: false,
                                         helperBundled: false, launchAtLogin: .notRegistered, paired: false)

    static let launchdPlist = "/Library/LaunchDaemons/\(Longcable.launchdLabel).plist"

    static func detect(paired: Bool) -> SetupStatus {
        SetupStatus(
            microphone: AVCaptureDevice.authorizationStatus(for: .audio),
            helperInstalled: FileManager.default.fileExists(atPath: launchdPlist),
            helperRunning: LongcableSocketClient().isInstalled,
            helperBundled: HelperInstaller.bundledDirectory != nil,
            launchAtLogin: SMAppService.mainApp.status,
            paired: paired)
    }
}

/// Installs Longcable's root launchd service from the copy embedded in the
/// app, using Longcable's own installer script, through the standard macOS
/// administrator password prompt.
///
/// `do shell script … with administrator privileges` is used because it works
/// for a non-notarized local build with no extra signing requirements and
/// runs the upstream installer unchanged. SMAppService.daemon would need the
/// daemon plist and binary laid out inside this bundle under our label and
/// approval in System Settings, and would bypass Longcable's own install path;
/// SMJobBless and AuthorizationExecuteWithPrivileges are deprecated.
enum HelperInstaller {
    /// Contents/Resources/longcable (bin/longcable-server, scripts/, LICENSE).
    static var bundledDirectory: URL? {
        guard let dir = Bundle.main.resourceURL?.appendingPathComponent("longcable"),
              FileManager.default.isExecutableFile(atPath: dir.appendingPathComponent("bin/longcable-server").path)
        else { return nil }
        return dir
    }

    /// Runs `command` as root after the system password prompt. Main thread.
    @MainActor
    static func runAsAdmin(_ command: String) -> Result<String, Error> {
        let escaped = command.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
        let source = "do shell script \"\(escaped)\" with prompt \"Tightwire wants to install its Stream Deck handoff helper.\" with administrator privileges"
        var error: NSDictionary?
        guard let script = NSAppleScript(source: source) else { return .failure(StoreError(description: "could not build the install script")) }
        let out = script.executeAndReturnError(&error)
        if let error {
            let msg = error[NSAppleScript.errorMessage] as? String ?? "\(error)"
            // -128 = the user cancelled the password prompt.
            if (error[NSAppleScript.errorNumber] as? Int) == -128 { return .failure(StoreError(description: "Cancelled")) }
            return .failure(StoreError(description: msg))
        }
        return .success(out.stringValue ?? "")
    }

    /// `peer`: the paired PC's IPv4, the only address allowed on TCP 3240
    /// (never Longcable's subnet/tailnet default). See HelperInstallCommand
    /// for how the root command avoids running the user-writable bundle.
    @MainActor
    static func install(peer: String) -> Result<String, Error> {
        run(script: "install-mac.sh", args: ["0.0.0.0", peer])
    }

    @MainActor
    static func uninstall() -> Result<String, Error> {
        run(script: "uninstall-mac.sh", args: [])
    }

    @MainActor
    private static func run(script: String, args: [String]) -> Result<String, Error> {
        guard bundledDirectory != nil else { return .failure(StoreError(description: "This build does not include the helper")) }
        guard let cdhash = HelperInstallCommand.runningCDHash() else {
            return .failure(StoreError(description: "Tightwire is not code-signed, so its helper cannot be verified"))
        }
        return runAsAdmin(HelperInstallCommand.make(appPath: Bundle.main.bundlePath, cdhash: cdhash, script: script, args: args))
    }
}
