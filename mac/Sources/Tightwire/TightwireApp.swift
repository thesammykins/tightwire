import SwiftUI
import TightwireCore

/// Handles tightwire:// URLs (Stream Deck "Open URL" buttons), the snapshot
/// harness and app quit. An Apple Event handler works whether or not any
/// window is open.
final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor static var model: AppModel?

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSAppleEventManager.shared().setEventHandler(self, andSelector: #selector(handleURL(_:reply:)),
                                                     forEventClass: AEEventClass(kInternetEventClass),
                                                     andEventID: AEEventID(kAEGetURL))
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        MainActor.assumeIsolated {
            if let dir = Snapshot.outputDirectory {
                Snapshot.renderAll(to: dir)
                exit(0)
            }
            // First run, or something still to set up: show the window.
            if let m = Self.model, !m.setup.complete { m.showWindow(.setup) }
        }
    }

    @objc func handleURL(_ event: NSAppleEventDescriptor, reply: NSAppleEventDescriptor) {
        guard let s = event.paramDescriptor(forKeyword: AEKeyword(keyDirectObject))?.stringValue, let url = URL(string: s) else { return }
        MainActor.assumeIsolated { Self.model?.handleURL(url) }
    }

    func applicationWillTerminate(_ notification: Notification) {
        // Never leave a device shared with nobody attached.
        MainActor.assumeIsolated { Self.model?.shutdown() }
    }

    /// Opening Tightwire again (Finder, Spotlight, `open`) while it runs shows the window.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        MainActor.assumeIsolated { Self.model?.showWindow() }
        return false
    }

    /// Closing the window must not quit a menu bar app.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

@main
struct TightwireApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var model: AppModel

    init() {
        let m = AppModel(live: Snapshot.outputDirectory == nil)
        _model = State(initialValue: m)
        AppDelegate.model = m
    }

    var body: some Scene {
        MenuBarExtra {
            StatusMenu(model: model)
        } label: {
            Image(nsImage: MenuIcon.image(model.menuIcon))
        }
        .menuBarExtraStyle(.menu)

        Window("Tightwire", id: MainWindow.identifier) {
            MainWindow(model: model)
        }
        .defaultSize(width: 860, height: 620)
        .windowResizability(.contentMinSize)
        .suppressedAtLaunch()
    }
}

extension Scene {
    /// The window opens from the menu (or for setup), never by itself at login.
    func suppressedAtLaunch() -> some Scene {
        if #available(macOS 15, *) {
            return defaultLaunchBehavior(.suppressed)
        } else {
            return self
        }
    }
}

/// The native menu: one status line, the stream and deck actions, the window
/// and the log. Everything else lives in the window.
struct StatusMenu: View {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        // Evaluated when SwiftUI builds the menu, which gives the model a way
        // to open the window later (pairing code ready, first launch).
        let _ = register()
        Text(model.statusLine)
        Button(model.isRunning ? "Stop Streaming" : "Start Streaming") { model.toggle() }
            .disabled(model.pairing == nil || model.flow != .none)
        if model.handoffConfigured {
            Divider()
            Text("Stream Deck: \(model.deckOwnerText)")
            if model.handoffState.owner == .pc {
                Button("Back to Mac") { model.handoffRequest(.mac) }
                    .disabled(model.handoffState.busy != nil)
            } else {
                Button("Send to PC") { model.handoffRequest(.pc) }
                    .disabled(model.handoffState.busy != nil)
            }
        }
        Divider()
        Button("Open Tightwire…") { model.showWindow() }
            .keyboardShortcut(",")
        Button("Open Log") { model.openLog() }
        Divider()
        Button("Quit Tightwire") {
            model.shutdown()
            NSApplication.shared.terminate(nil)
        }
        .keyboardShortcut("q")
    }

    private func register() {
        guard model.openWindowAction == nil else { return }
        let open = openWindow
        DispatchQueue.main.async { model.openWindowAction = { open(id: MainWindow.identifier) } }
    }
}
