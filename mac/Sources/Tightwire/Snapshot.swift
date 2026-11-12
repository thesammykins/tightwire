import AppKit
import SwiftUI
import TightwireCore

/// Renders the real window views with canned state to PNGs, so layout can be
/// checked without clicking through the app:
///
///     Tightwire --snapshot out/ui
///
/// Each scenario is drawn at the window's minimum size and at a large size.
/// The model is built with `live: false`, so nothing touches Core Audio, the
/// Keychain, Longcable or UserDefaults.
@MainActor
enum Snapshot {
    static var outputDirectory: String? {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--snapshot") else { return nil }
        return i + 1 < args.count ? args[i + 1] : "out/ui"
    }

    struct Scenario {
        let name: String
        let configure: (AppModel) -> Void
    }

    static let sizes: [(String, CGSize)] = [("min", MainWindow.minSize), ("large", CGSize(width: 1280, height: 860))]

    static func renderAll(to dir: String) {
        NSApp.appearance = NSAppearance(named: .darkAqua)
        let url = URL(fileURLWithPath: dir, isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        for (name, appearance, bar) in [("light", NSAppearance.Name.aqua, NSColor(white: 0.93, alpha: 1)),
                                        ("dark", NSAppearance.Name.darkAqua, NSColor(white: 0.16, alpha: 1))] {
            let file = url.appendingPathComponent("menu-icons-\(name).png")
            renderMenuIcons(appearance: NSAppearance(named: appearance)!, bar: bar, to: file)
            print("wrote \(file.path)")
        }
        for s in scenarios {
            for (label, size) in sizes {
                let model = AppModel(live: false)
                base(model)
                s.configure(model)
                let file = url.appendingPathComponent("\(s.name)-\(label).png")
                render(model, size: size, to: file)
                print("wrote \(file.path)")
            }
        }
    }

    static func render(_ model: AppModel, size: CGSize, to file: URL) {
        let window = NSWindow(contentRect: CGRect(origin: .zero, size: size),
                              styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
                              backing: .buffered, defer: false)
        window.title = "Tightwire"
        window.appearance = NSAppearance(named: .darkAqua)
        window.isReleasedWhenClosed = false
        let host = NSHostingView(rootView: MainWindow(model: model))
        window.contentView = host
        window.setContentSize(size)
        window.orderFrontRegardless()
        // Let SwiftUI lay out (split view columns settle over a few passes).
        for _ in 0..<6 {
            RunLoop.main.run(until: Date().addingTimeInterval(0.1))
            window.layoutIfNeeded()
        }
        // The theme frame includes the title bar and toolbar.
        let view = window.contentView?.superview ?? host
        guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return }
        view.cacheDisplay(in: view.bounds, to: rep)
        try? rep.representation(using: .png, properties: [:])?.write(to: file)
        window.orderOut(nil)
        window.close()
    }

    /// Every menu bar icon state on a menu-bar-coloured strip, at 1× and
    /// enlarged 4×, drawn the way the status item draws them: templates are
    /// tinted with the appearance's label colour, the others drawn as is.
    static func renderMenuIcons(appearance: NSAppearance, bar: NSColor, to file: URL) {
        let states = MenuIcon.State.allCases
        let cell: CGFloat = 26, scale: CGFloat = 4
        let size = NSSize(width: CGFloat(states.count) * cell * scale + 20, height: cell * scale + cell + 30)
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size.width) * 2, pixelsHigh: Int(size.height) * 2,
                                   bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                   colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        rep.size = size
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        appearance.performAsCurrentDrawingAppearance {
            bar.setFill()
            NSRect(origin: .zero, size: size).fill()
            for (i, st) in states.enumerated() {
                let img = MenuIcon.image(st)
                let draw: (NSRect) -> Void = { r in
                    if img.isTemplate {
                        let tinted = NSImage(size: img.size, flipped: false) { b in
                            img.draw(in: b)
                            NSColor.labelColor.set()
                            b.fill(using: .sourceAtop)
                            return true
                        }
                        tinted.draw(in: r)
                    } else {
                        img.draw(in: r)
                    }
                }
                let x = 10 + CGFloat(i) * cell * scale
                draw(NSRect(x: x, y: 18, width: img.size.width * scale, height: img.size.height * scale))
                draw(NSRect(x: x, y: cell * scale + 16, width: img.size.width, height: img.size.height))
                ("\(st)" as NSString).draw(at: NSPoint(x: x + 30, y: cell * scale + 16),
                                           withAttributes: [.font: NSFont.systemFont(ofSize: 11), .foregroundColor: NSColor.labelColor])
            }
        }
        NSGraphicsContext.restoreGraphicsState()
        try? rep.representation(using: .png, properties: [:])?.write(to: file)
    }

    // MARK: Canned state

    static let longName = "Elgato Systems Stream Deck + Studio Edition with an Extremely Long Product Name (Desk Left, Port 3)"

    static let inputs = [
        InputDevice(id: 1, uid: "ElgatoWaveLinkPersonalMix", name: "Elgato Wave Link Personal Mix", channels: 2, sampleRate: 48000),
        InputDevice(id: 2, uid: "AppleUSBAudioEngine:Elgato Systems:Elgato XLR Dock:0000XLRDOCK001:2,1", name: "Elgato XLR Dock", channels: 1, sampleRate: 48000),
        InputDevice(id: 3, uid: "long", name: "Universal Audio Apollo Twin X Quad Heritage Edition Thunderbolt 3 (Main Inputs 1–2)", channels: 8, sampleRate: 48000),
    ]

    static let pairing = Pairing(peerName: "GAMING-PC", peerID: Array(repeating: 1, count: 16), rootKey: Array(repeating: 2, count: 32),
                                 lastAddress: "192.168.50.20:4810", hostName: "GAMING-PC")

    static func base(_ m: AppModel) {
        m.devices = inputs
        m.deviceUID = inputs[0].uid
        m.setup = SetupStatus(microphone: .authorized, helperInstalled: true, helperRunning: true, helperBundled: true,
                              launchAtLogin: .enabled, paired: true)
    }

    static var stats: Wire.Stats {
        var x = Wire.StatsExt()
        x.outputMode = .wasapiSharedLowLatency
        x.streamChannels = 2
        x.outputChannels = 2
        x.targetMicros = 11_980
        x.jitterMeanMicros = 90
        x.jitterPeakMicros = 610
        x.maxTargetMicros = 20_000
        x.outputPeriodFrames = 480
        x.outputRate = 48_000
        x.streamRate = 48_000
        x.driftPPB = -31_200
        x.ringFill = 1
        x.ringCapacity = 512
        x.receiverIPv4 = [192, 168, 50, 20]
        return Wire.Stats(streamID: 7, depthMicros: 12_020, received: 120_334, lost: 0, late: 0, driftPPM: -31,
                          underruns: 0, authDropped: 0, echoSequence: 120_330, sessionNonce: 9, ext: x)
    }

    static var session: SessionStatus {
        var st = SessionStatus()
        st.phase = .streaming
        st.stats = stats
        st.rtt = Distribution([0.31, 0.35, 0.42, 0.38, 0.33])
        st.receiverRevision = 1
        st.stream = (48_000, 2, 48, 48, 96)
        return st
    }

    static func paired(_ m: AppModel) {
        m.pairing = pairing
        m.phase = .idle
    }

    static let scenarios: [Scenario] = [
        Scenario(name: "1-unpaired") { m in
            m.pane = .receiver
            m.phase = .unpaired
            m.setup.paired = false
        },
        Scenario(name: "2-pairing-browsing") { m in
            m.pane = .receiver
            m.phase = .unpaired
            m.setup.paired = false
            m.flow = .browsing
            m.candidates = [
                .init(id: "a", host: "GAMING-PC", address: "192.168.50.20"),
                .init(id: "b", host: "STUDIO-WORKSTATION-WITH-A-VERY-LONG-NETBIOS-AND-DNS-NAME.local", address: "192.168.50.143"),
            ]
        },
        Scenario(name: "3-pairing-confirm") { m in
            m.pane = .receiver
            m.phase = .unpaired
            m.setup.paired = false
            m.flow = .confirm(code: "482 913", peer: "GAMING-PC")
        },
        Scenario(name: "4-streaming") { m in
            paired(m)
            m.pane = .stream
            m.phase = .streaming
            m.level = 0.32
            m.statsFresh = true
            m.stats = Self.stats
            m.sessionStatus = Self.session
            m.keepingAwake = true
        },
        Scenario(name: "4b-latency-test") { m in
            paired(m)
            m.pane = .stream
            m.phase = .streaming
            m.level = 0.28
            m.statsFresh = true
            m.stats = Self.stats
            m.sessionStatus = Self.session
            m.keepingAwake = true
            m.testResult = LatencyTestResult(
                seconds: 30, rtt: Distribution([0.31, 0.35, 0.42, 0.38, 0.9]), depth: Distribution([11.9, 12.0, 12.1]),
                target: Distribution([11.9, 12.4]), jitterPeakMaxMillis: 0.62, jitterMeanMillis: 0.09, lost: 0, late: 0,
                underruns: 0, driftPPM: Distribution([-31, -30, -32]), marker: Distribution([13.6, 13.9, 14.1, 13.8]),
                markerUncertaintyMillis: 0.24, markerFromCapture: true,
                recommendation: .init(framesPerPacket: 48, floorMicros: nil, capMicros: 20000,
                                      reason: "arrivals within 2 ms; the adaptive buffer kept up"),
                receiverOutdated: false)
            m.tuning = .applied(floorMicros: 0, capMicros: 20000)
        },
        Scenario(name: "4c-testing-old-pc") { m in
            paired(m)
            m.pane = .stream
            m.phase = .streaming
            m.statsFresh = true
            m.stats = Wire.Stats(streamID: 7, depthMicros: 2650, received: 120_334, lost: 2, late: 0, driftPPM: -31,
                                 underruns: 1, authDropped: 0, echoSequence: 120_330, sessionNonce: 9)
            var st = Self.session
            st.receiverRevision = 0
            st.stats = m.stats
            m.sessionStatus = st
            m.testProgress = 0.4
        },
        Scenario(name: "5-error") { m in
            paired(m)
            m.pane = .stream
            m.deviceUID = inputs[2].uid
            m.phase = .error("Elgato XLR Dock was disconnected while streaming. Reconnect it or choose another input, then start again.")
        },
        Scenario(name: "6-handoff") { m in
            paired(m)
            m.pane = .handoff
            m.helperRunning = true
            m.allowedPeers = ["100.64.0.0/10", "192.168.50.20"]
            let deck = HandoffDevice(vendor: 0x0fd9, product: 0x0084, serial: "EL00A0A00001", name: "Stream Deck Plus",
                                     location: "01133100", busID: "1-1")
            m.handoffDevices = [deck]
            m.handoffState = HandoffState(owner: .pc, mode: .auto, receiverAlive: true, log: [
                "19:41:02 handover to 192.168.50.20: Stream Deck Plus",
                "19:41:02 Longcable now allows 192.168.50.20 (100.64.0.0/10, 192.168.50.20)",
                "19:41:02 share 1-1 (Stream Deck Plus)",
                "19:41:03 offering 1-1 to the PC",
                "19:41:04 PC attached 1-1 on port 1",
                "19:41:04 devices on the PC: 1-1",
            ])
            func row(_ d: HandoffDevice, registered: Bool, shared: Bool, roles: [USBDeviceRole], verdict: HandoffPolicy.Verdict) -> AppModel.HandoffRow {
                .init(device: d, registered: registered, shared: shared, state: "", roles: roles, verdict: verdict,
                      suggested: HandoffPolicy.suggested(vendor: d.vendor, roles: roles))
            }
            let long = HandoffDevice(vendor: 0x0fd9, product: 0x0090, serial: "CL55K2B00001", name: longName, location: "01134100")
            m.handoffRows = [
                row(deck, registered: true, shared: true, roles: [], verdict: .ok),
                row(long, registered: false, shared: false, roles: [], verdict: .ok),
                row(HandoffDevice(vendor: 0x0fd9, product: 0x00a6, serial: "", name: "Elgato XLR Dock", location: "01133200", busID: "1-2"),
                    registered: true, shared: false, roles: [.audio],
                    verdict: HandoffPolicy.verdict(name: "Elgato XLR Dock", roles: [.audio], feedsStream: true)),
                row(HandoffDevice(vendor: 0x1b1c, product: 0x2b0c, serial: "0000", name: "Example Mechanical Keyboard", location: "01131100"),
                    registered: false, shared: false, roles: [.keyboard],
                    verdict: HandoffPolicy.verdict(name: "Example Keyboard Display", roles: [.keyboard], feedsStream: false)),
                row(HandoffDevice(vendor: 0x05e3, product: 0x0764, serial: "0001", name: "USB Storage", location: "00243000"),
                    registered: false, shared: false, roles: [.storage],
                    verdict: HandoffPolicy.verdict(name: "USB Storage", roles: [.storage], feedsStream: false)),
            ]
            m.registering = [long.id]
        },
        Scenario(name: "7-setup-incomplete") { m in
            m.pane = .setup
            m.phase = .unpaired
            m.setup = SetupStatus(microphone: .notDetermined, helperInstalled: false, helperRunning: false, helperBundled: true,
                                  launchAtLogin: .requiresApproval, paired: false)
        },
    ]
}
