import AppKit
import SwiftUI
import TightwireCore

/// The configuration window: a sidebar of four panes, each a grouped Form.
struct MainWindow: View {
    static let identifier = "main"
    static let minSize = CGSize(width: 680, height: 480)

    @Bindable var model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        NavigationSplitView {
            Sidebar(model: model)
                .navigationSplitViewColumnWidth(min: 180, ideal: 200, max: 260)
        } detail: {
            Group {
                switch model.pane {
                case .stream: StreamPane(model: model)
                case .receiver: ReceiverPane(model: model)
                case .handoff: HandoffPane(model: model)
                case .setup: SetupPane(model: model)
                }
            }
            .formStyle(.grouped)
            .navigationTitle(model.pane.title)
        }
        .frame(minWidth: Self.minSize.width, minHeight: Self.minSize.height)
        .preferredColorScheme(.dark)
        .background(WindowConfigurator())
        .onAppear {
            let open = openWindow
            model.openWindowAction = { open(id: Self.identifier) }
        }
    }
}

extension AppModel.Pane {
    var title: String {
        switch self {
        case .stream: "Stream"
        case .receiver: "Receiver"
        case .handoff: "Stream Deck"
        case .setup: "Setup"
        }
    }

    var symbol: String {
        switch self {
        case .stream: "waveform"
        case .receiver: "desktopcomputer"
        case .handoff: "square.grid.3x2"
        case .setup: "checklist"
        }
    }
}

/// Remembers the window frame and shows Tightwire in the Dock and app
/// switcher while the window is open, so it activates like a normal app.
private struct WindowConfigurator: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let v = NSView()
        DispatchQueue.main.async {
            guard let w = v.window else { return }
            w.identifier = NSUserInterfaceItemIdentifier(MainWindow.identifier)
            w.setFrameAutosaveName("TightwireMainWindow")
            w.collectionBehavior.insert(.moveToActiveSpace)
            NSApp.setActivationPolicy(.regular)
            context.coordinator.observe(w)
        }
        return v
    }

    func updateNSView(_ nsView: NSView, context: Context) {}
    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        var token: NSObjectProtocol?
        func observe(_ w: NSWindow) {
            token = NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: w, queue: .main) { _ in
                // Back to a menu-bar-only app.
                MainActor.assumeIsolated { _ = NSApp.setActivationPolicy(.accessory) }
            }
        }
        deinit { token.map(NotificationCenter.default.removeObserver) }
    }
}

// MARK: Sidebar

private struct Sidebar: View {
    @Bindable var model: AppModel

    var body: some View {
        List(selection: Binding(get: { model.pane }, set: { if let p = $0 { model.pane = p } })) {
            ForEach(AppModel.Pane.allCases) { pane in
                Label {
                    HStack {
                        Text(pane.title)
                        Spacer()
                        badge(pane)
                    }
                } icon: {
                    Image(systemName: pane.symbol)
                }
                .tag(pane)
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            HStack(spacing: 8) {
                StatusDot(phase: model.phase, warning: model.pcNotPlaying)
                Text(model.statusLine)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .truncationMode(.tail)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
        }
    }

    @ViewBuilder func badge(_ pane: AppModel.Pane) -> some View {
        switch pane {
        case .stream where model.phase == .streaming:
            Circle().fill(.green).frame(width: 7, height: 7)
        case .receiver:
            if case .confirm = model.flow {
                Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
            }
        case .handoff where model.handoffConfigured:
            Text(model.handoffState.owner == .pc ? "PC" : "Mac")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.secondary)
        case .setup where !model.setup.complete:
            Text("\(model.setup.remaining)")
                .font(.caption2.weight(.semibold).monospacedDigit())
                .padding(.horizontal, 6).padding(.vertical, 1)
                .background(Capsule().fill(.orange.opacity(0.25)))
        default:
            EmptyView()
        }
    }
}

struct StatusDot: View {
    let phase: AppModel.Phase
    /// Streaming to a PC that is not playing.
    var warning = false
    var body: some View {
        Circle().fill(color).frame(width: 8, height: 8)
            .accessibilityHidden(true)
    }

    var color: Color {
        switch phase {
        case .streaming: warning ? .orange : .green
        case .searching: .yellow
        case .error: .red
        default: .secondary
        }
    }
}

// MARK: Stream

private struct StreamPane: View {
    @Bindable var model: AppModel

    var body: some View {
        Form {
            Section {
                HStack(alignment: .center, spacing: 12) {
                    StatusDot(phase: model.phase, warning: model.pcNotPlaying)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(headline).font(.headline).lineLimit(1)
                        Text(subline).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                    }
                    Spacer(minLength: 8)
                    Button(model.isRunning ? "Stop" : "Start") { model.toggle() }
                        .buttonStyle(.borderedProminent)
                        .tint(model.isRunning ? .red : .accentColor)
                        .controlSize(.large)
                        .disabled(model.pairing == nil || model.flow != .none)
                }
                if case .error(let msg) = model.phase {
                    Label(msg, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                        .font(.callout)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            if model.isRunning {
                levelSection
                receiverSection
                latencyAndPowerSections
                inputSection
            } else {
                inputSection
                levelSection
                receiverSection
                latencyAndPowerSections
            }
        }
    }

    @ViewBuilder var inputSection: some View {
        Section {
            Picker("Device", selection: $model.deviceUID) {
                Text("System default (\(AudioDevices.defaultInput(in: model.devices, systemDefault: model.live ? AudioDevices.systemDefaultInput() : model.devices.first?.id)?.name ?? "none"))").tag("")
                ForEach(model.devices) { d in Text(d.name).tag(d.uid) }
                if model.savedInputMissing { Text("\(model.deviceUID) (not connected)").tag(model.deviceUID) }
            }
            .truncationMode(.middle)
            if model.savedInputMissing {
                Label("The saved input is not connected; using the system default until it is back.", systemImage: "exclamationmark.triangle")
                    .font(.caption).foregroundStyle(.orange)
            }
            if let d = model.selectedDevice {
                Picker("Channel", selection: $model.channel) {
                    if d.channels > 1 { Text(d.channels == 2 ? "Stereo (both channels)" : "All \(min(d.channels, 8)) channels").tag(AppModel.allChannels) }
                    ForEach(0..<d.channels, id: \.self) { c in Text("Mono · channel \(c + 1)").tag(c) }
                }
                LabeledContent("Format", value: "\(Self.rate(d.sampleRate)) · \(d.channels) ch · 32-bit float")
            }
            Picker("Packet size", selection: $model.framesPerPacket) {
                ForEach(AppModel.packetSizes, id: \.self) { Text("\($0)").tag($0) }
            }
            .pickerStyle(.segmented)
        } header: {
            Text("Input")
        } footer: {
            Text(model.isRunning ? "Stop streaming to change the input." : packetHint)
                .font(.caption).foregroundStyle(.secondary)
        }
        .disabled(model.isRunning)

    }

    @ViewBuilder var levelSection: some View {
        Section("Level") {
            LevelRow(model: model)
        }

    }

    @ViewBuilder var receiverSection: some View {
        Section("Receiver") {
            if model.receiverOutdated {
                Label("The PC runs an older Tightwire. Update Tightwire on the PC to 0.2.0 for buffer details, latency measurement and secure Stream Deck handoff.",
                      systemImage: "arrow.up.circle")
                    .font(.callout).foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if model.pcNotPlaying {
                Label {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(AppModel.pcNotPlayingText).font(.callout.weight(.semibold))
                        Text("Audio reaches \(model.peerName), but no output device is open there, so it is discarded. On the PC: Tightwire tray icon › Output device.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                } icon: {
                    Image(systemName: "speaker.slash.fill").foregroundStyle(.yellow)
                }
                .fixedSize(horizontal: false, vertical: true)
            } else if let note = model.receiverHealthNote {
                Label(note, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout).foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let s = model.stats, model.statsFresh {
                Grid(alignment: .leading, horizontalSpacing: 24, verticalSpacing: 8) {
                    GridRow {
                        stat("Buffer", String(format: "%.1f ms", Double(s.depthMicros) / 1000))
                        stat("Target", s.ext.map { String(format: "%.1f ms", Double($0.targetMicros) / 1000) } ?? "—")
                        stat("Jitter", s.ext.map { String(format: "%.2f / %.2f", Double($0.jitterMeanMicros) / 1000, Double($0.jitterPeakMicros) / 1000) } ?? "—")
                        stat("Drift", s.ext.map { String(format: "%+.1f ppm", Double($0.driftPPB) / 1000) } ?? "\(s.driftPPM > 0 ? "+" : "")\(s.driftPPM) ppm")
                    }
                    GridRow {
                        stat("Lost", "\(s.lost)")
                        stat("Late", "\(s.late)")
                        stat("Underruns", "\(s.underruns)")
                        stat("Rejected", "\(s.authDropped)")
                    }
                }
                .padding(.vertical, 2)
                if let x = s.ext {
                    Text("\(x.outputMode.name) · \(x.outputPeriodFrames)-frame period · stream \(Self.rate(Double(x.streamRate))) \(x.streamChannels) ch → device \(Self.rate(Double(x.outputRate))) \(x.outputChannels) ch · ring \(x.ringFill)/\(x.ringCapacity)\(x.minTargetMicros > 0 ? String(format: " · floor %.1f ms", Double(x.minTargetMicros) / 1000) : "") · cap \(String(format: "%.0f", Double(x.maxTargetMicros) / 1000)) ms")
                        .font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let h = x.health, h.outputOpen {
                        Text(String(format: "PC output: %.1f ms queued · device %+.0f ppm · %u skips, %u inserts, %u trims",
                                    Double(h.deviceQueueMicros) / 1000, Double(h.deviceRatePPB) / 1000, h.skips, h.inserts, h.trims))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            } else {
                Text(model.phase == .streaming ? "Waiting for the PC's first report…"
                     : model.isRunning ? "No answer from \(model.peerName) yet."
                     : "Start streaming to see the PC's buffer, loss and drift.")
                    .foregroundStyle(.secondary)
            }
        }

    }

    @ViewBuilder var latencyAndPowerSections: some View {
        if model.isRunning || model.testResult != nil {
            LatencySection(model: model)
        }

        if model.keepingAwake {
            Section {
                Label("Keeping this Mac awake while streaming or while a device is on the PC (the display can still sleep).", systemImage: "cup.and.saucer")
                    .font(.callout).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    var headline: String {
        switch model.phase {
        case .unpaired: "Not paired"
        case .idle: "Ready"
        case .searching: "Looking for \(model.peerName)…"
        case .streaming: model.pcNotPlaying ? "Streaming — PC is not playing" : "Streaming to \(model.peerName)"
        case .error: "Stopped"
        }
    }

    var subline: String {
        switch model.phase {
        case .unpaired: "Pair with a PC in Receiver first."
        case .idle: "Sends \(model.selectedDevice?.name ?? "the input") to \(model.peerName)."
        case .searching: "Audio is held back until the PC answers."
        case .streaming: model.pcNotPlaying ? "Choose an output on the PC: tray icon › Output device." : model.selectedDevice?.name ?? ""
        case .error: "Fix the problem below, then start again."
        }
    }

    var packetHint: String {
        let rate = model.selectedDevice?.sampleRate ?? 48000
        let ms = Double(model.framesPerPacket) / rate * 1000
        return String(format: "%d frames = %.2f ms per packet. Smaller is lower latency; 128–256 suits busy networks.", model.framesPerPacket, ms)
    }

    static func rate(_ r: Double) -> String {
        r.truncatingRemainder(dividingBy: 1000) == 0 ? "\(Int(r / 1000)) kHz" : String(format: "%.1f kHz", r / 1000)
    }

    func stat(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label).font(.caption).foregroundStyle(.secondary)
            Text(value).font(.title3.monospacedDigit())
        }
        .frame(minWidth: 90, alignment: .leading)
    }
}

private struct LatencySection: View {
    let model: AppModel

    var body: some View {
        Section {
            if !model.latencyTerms.isEmpty {
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 6) {
                    ForEach(model.latencyTerms) { term in
                        GridRow {
                            Text(term.label)
                            Text(term.millis.map { String(format: "%.2f ms", $0) } ?? "—")
                                .monospacedDigit()
                                .gridColumnAlignment(.trailing)
                            Text(term.kind.rawValue)
                                .font(.caption2.weight(.semibold))
                                .foregroundStyle(term.kind == .measured ? .green : term.kind == .estimate ? .orange : .secondary)
                            Text(term.detail).font(.caption).foregroundStyle(.secondary).lineLimit(3)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                    Divider().gridCellUnsizedAxes(.horizontal)
                    GridRow {
                        Text("Estimated total").fontWeight(.semibold)
                        Text(model.latencyTotal.map { String(format: "%.1f ms", $0) } ?? "—")
                            .monospacedDigit().fontWeight(.semibold)
                        Text("sum").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
                        Text(model.stats?.ext?.health != nil ? "mic to the PC's output device, including what its output endpoint has queued" : "mic to the PC's output device, including one output period").font(.caption).foregroundStyle(.secondary)
                    }
                    if let m = model.testResult?.marker, let u = model.testResult?.markerUncertaintyMillis {
                        GridRow {
                            Text("Measured").fontWeight(.semibold)
                            Text(String(format: "%.1f ms", m.median)).monospacedDigit().fontWeight(.semibold)
                            Text("measured").font(.caption2.weight(.semibold)).foregroundStyle(.green)
                            Text(String(format: "%@ → handed to the PC's output device (buffer, network, jitter buffer, resampler; not the driver latencies or output period); median of %d markers, ± %.1f ms",
                                        model.testResult?.markerFromCapture == true ? "Core Audio capture time" : "packet send", m.count, u))
                                .font(.caption).foregroundStyle(.secondary).lineLimit(3)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
            }
            HStack(spacing: 10) {
                if let p = model.testProgress {
                    ProgressView(value: p).frame(maxWidth: 160)
                    Text("Measuring… \(Int(p * 30)) of 30 s").font(.caption).foregroundStyle(.secondary)
                } else {
                    Button("Run Latency Test") { model.runLatencyTest() }
                        .disabled(model.phase != .streaming)
                }
                Spacer(minLength: 0)
            }
            if let r = model.testResult {
                VStack(alignment: .leading, spacing: 6) {
                    Text(r.summary).font(.callout).fixedSize(horizontal: false, vertical: true)
                    Text("Recommended: \(r.recommendation.framesPerPacket)-frame packets, buffer floor \(r.recommendation.floorMicros.map { String(format: "%.1f ms", Double($0) / 1000) } ?? "adaptive"), cap \(String(format: "%.0f", Double(r.recommendation.capMicros) / 1000)) ms. \(r.recommendation.reason.prefix(1).uppercased() + r.recommendation.reason.dropFirst()).")
                        .font(.callout).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack {
                        Button("Apply") { model.applyRecommendation() }
                            .disabled(!model.isRunning || r.receiverOutdated)
                        if let t = model.tuning { Text(Self.describe(t)).font(.caption).foregroundStyle(.secondary) }
                    }
                }
            }
        } header: {
            Text("Latency")
        } footer: {
            Text("Measured: network and buffer (and the end-to-end markers of a test). Reported/fixed: device and configuration figures. What the PC's audio engine, Wave Link's mixer and apps add after the output device cannot be measured from here. Apply restarts the stream if the packet size changes.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    static func describe(_ t: TuningState) -> String {
        switch t {
        case .sending: "Sending to the PC…"
        case .applied(let f, let c): "PC applied: floor \(f == 0 ? "adaptive" : String(format: "%.1f ms", Double(f) / 1000)), cap \(String(format: "%.0f", Double(c) / 1000)) ms (saved on the PC)"
        case .rejected(let m): "PC refused: \(m)"
        case .noAnswer: "No answer from the PC (it needs Tightwire 0.2.0)"
        }
    }
}

/// Reads `model.level` in its own body, so the meter's 10 Hz updates
/// re-evaluate this row and not the whole Stream pane.
struct LevelRow: View {
    let model: AppModel

    var body: some View {
        LevelMeter(level: model.level, active: model.isRunning)
    }
}

struct LevelMeter: View {
    let level: Float
    let active: Bool

    var db: Float { 20 * log10(max(level, 1e-5)) }

    var body: some View {
        HStack(spacing: 10) {
            GeometryReader { g in
                let frac = CGFloat(max(0, min(1, (db + 60) / 60)))
                ZStack(alignment: .leading) {
                    Capsule().fill(.quaternary)
                    Capsule()
                        .fill(LinearGradient(colors: [.green, .green, .yellow, .red],
                                             startPoint: .leading, endPoint: .trailing))
                        .mask(alignment: .leading) { Rectangle().frame(width: g.size.width * frac) }
                }
            }
            .frame(height: 8)
            Text(active ? String(format: "%5.0f dB", max(db, -60)) : "—")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 56, alignment: .trailing)
        }
        .accessibilityElement()
        .accessibilityLabel("Input level")
        .accessibilityValue(active ? String(format: "%.0f dB", db) : "idle")
    }
}

// MARK: Receiver

private struct ReceiverPane: View {
    @Bindable var model: AppModel
    @State private var confirmForget = false

    var body: some View {
        Form {
            if model.flow != .none {
                PairingSection(model: model)
            }
            if model.pairing != nil || model.flow == .none {
                pairedSection
            }
        }
        .confirmationDialog("Forget \(model.peerName)?", isPresented: $confirmForget) {
            Button("Forget", role: .destructive) { model.forget() }
        } message: {
            Text("Streaming and Stream Deck handoff stop until you pair again.")
        }
    }

    var pairedSection: some View {
            Section("Paired PC") {
                if let p = model.pairing {
                    LabeledContent("Name") { Text(p.peerName).lineLimit(1).truncationMode(.middle) }
                    LabeledContent("Last address", value: p.lastAddress ?? p.hostName ?? "Not seen yet")
                    TextField("Address override", text: $model.addressOverride, prompt: Text("Automatic"))
                        .disabled(model.isRunning)
                    HStack {
                        Button("Pair Another PC…") { model.beginPairing() }
                            .disabled(model.isRunning || model.flow != .none)
                        Spacer()
                        Button("Forget…", role: .destructive) { confirmForget = true }
                    }
                } else {
                    Text("On the PC, right-click the Tightwire tray icon and choose Pair…, then click Pair with a PC here.")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Button("Pair with a PC…") { model.beginPairing() }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.flow != .none)
                }
            }
    }
}

private struct PairingSection: View {
    let model: AppModel

    var body: some View {
        Section {
            switch model.flow {
            case .none:
                EmptyView()
            case .browsing:
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Looking for PCs in pairing mode. On the PC, right-click the Tightwire tray icon and choose Pair…")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                ForEach(model.candidates) { c in
                    HStack {
                        Image(systemName: c.conflict ? "exclamationmark.triangle.fill" : "desktopcomputer")
                            .foregroundStyle(c.conflict ? .orange : .primary)
                        Text(c.host).lineLimit(1).truncationMode(.middle)
                        if c.conflict {
                            Text("same id from two addresses").font(.caption).foregroundStyle(.orange)
                        }
                        Spacer(minLength: 8)
                        Text(c.address).font(.caption.monospaced()).foregroundStyle(.secondary)
                        Button("Pair") { model.pair(with: c) }
                    }
                }
                cancelRow("Cancel")
            case .connecting(let host):
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Contacting \(host)…")
                }
                cancelRow("Cancel")
            case .confirm(let code, let peer):
                VStack(spacing: 10) {
                    Text("Confirm the code").font(.title2.weight(.semibold))
                    Text(code)
                        .font(.system(size: 56, weight: .bold, design: .monospaced))
                        .tracking(4)
                        .foregroundStyle(.primary)
                        .textSelection(.enabled)
                        .padding(.vertical, 6)
                        .padding(.horizontal, 24)
                        .background(RoundedRectangle(cornerRadius: 12).fill(.orange.opacity(0.18)))
                        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(.orange.opacity(0.7), lineWidth: 1.5))
                    Text("Does the PC show exactly these six digits? Answer on both computers. (Request from “\(peer)”.)")
                        .multilineTextAlignment(.center)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 12) {
                        Button("Codes Differ") { model.answer(false) }
                            .controlSize(.large)
                        Button("Codes Match") { model.answer(true) }
                            .buttonStyle(.borderedProminent)
                            .controlSize(.large)
                            .keyboardShortcut(.defaultAction)
                    }
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
            case .failed(let msg):
                Label(msg, systemImage: "xmark.octagon.fill")
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                HStack {
                    Spacer()
                    Button("Close") { model.cancelPairing() }
                    Button("Try Again") { model.cancelPairing(); model.beginPairing() }
                        .buttonStyle(.borderedProminent)
                }
            }
        } header: {
            Text("Pairing")
        }
    }

    func cancelRow(_ title: String) -> some View {
        HStack {
            Spacer()
            Button(title) { model.cancelPairing() }
        }
    }
}

// MARK: Stream Deck handoff

private struct HandoffPane: View {
    @Bindable var model: AppModel

    var body: some View {
        Form {
            Section {
                LabeledContent("Helper") {
                    if model.longcableInstalled {
                        Label("Longcable running", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                    } else {
                        HStack {
                            Text("Not installed").foregroundStyle(.secondary)
                            if model.setup.helperBundled {
                                Button("Install…") { model.installHelper() }
                                    .disabled(model.pcIPv4 == nil)
                                    .help(model.pcIPv4 == nil ? "Pair with the PC first: the helper admits only the paired PC" : "")
                            }
                        }
                    }
                }
                if model.longcableInstalled {
                    LabeledContent("Allowed PCs") {
                        Text(model.allowedPeers.isEmpty ? "None" : model.allowedPeers.joined(separator: ", "))
                            .font(.callout.monospaced())
                            .lineLimit(2)
                            .truncationMode(.middle)
                            .textSelection(.enabled)
                    }
                }
                if let e = model.longcableError {
                    Label(e, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red).font(.callout)
                }
                if model.longcableInstalled, !model.wideAllowEntries.isEmpty {
                    VStack(alignment: .leading, spacing: 6) {
                        Label("Longcable also admits \(model.wideAllowEntries.joined(separator: ", ")): every machine there can import a device while it is shared.",
                              systemImage: "exclamationmark.shield")
                            .font(.callout).foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                        HStack {
                            Button("Allow Only \(model.pcIPv4 ?? "the PC")") { model.narrowAllowList() }
                                .disabled(model.pcIPv4 == nil)
                            if let m = model.allowListMessage { Text(m).font(.caption).foregroundStyle(.secondary) }
                        }
                    }
                } else if let m = model.allowListMessage {
                    Text(m).font(.caption).foregroundStyle(.secondary)
                }
            } header: {
                Text("Handoff helper")
            } footer: {
                if model.longcableInstalled, let ip = model.pcIPv4 {
                    Text(Longcable.listsExactly(ip, in: model.allowedPeers)
                         ? "The PC (\(ip)) may connect to Longcable."
                         : "Tightwire adds the PC (\(ip)) on each handover and removes it again when the devices come back.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }

            if model.longcableInstalled {
                Section("Owner") {
                    HStack(spacing: 10) {
                        Image(systemName: model.handoffState.owner == .pc ? "desktopcomputer" : "laptopcomputer")
                            .font(.title2)
                            .foregroundStyle(.secondary)
                            .frame(width: 30)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(model.handoffDevices.isEmpty ? "No device chosen" : model.deckOwnerText).font(.headline)
                            Text(ownerDetail).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                        }
                        Spacer(minLength: 8)
                        if model.handoffState.busy != nil { ProgressView().controlSize(.small) }
                        Button("Back to Mac") { model.handoffRequest(.mac) }
                            .disabled(!model.handoffConfigured || model.handoffState.busy != nil || model.handoffState.owner == .mac)
                        Button("Send to PC") { model.handoffRequest(.pc) }
                            .buttonStyle(.borderedProminent)
                            .disabled(!model.handoffConfigured || model.handoffState.busy != nil || model.handoffState.owner == .pc)
                    }
                    if let e = model.handoffState.lastError {
                        Label(e, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.red).font(.callout)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }

                Section {
                    if model.handoffRows.isEmpty {
                        Text("Longcable reports no USB devices.").foregroundStyle(.secondary)
                    }
                    ForEach(model.handoffRows) { row in
                        DeviceRow(model: model, row: row)
                    }
                } header: {
                    HStack {
                        Text("Devices")
                        Spacer()
                        Button("Refresh") { model.refreshLongcable() }
                            .buttonStyle(.borderless)
                            .font(.caption)
                    }
                } footer: {
                    Text("A ticked device is registered with Longcable but stays on the Mac until it is sent to the PC.")
                        .font(.caption).foregroundStyle(.secondary)
                }

                Section {
                    Toggle("Allow URL buttons (tightwire://deck/…)", isOn: $model.allowURLTriggers)
                    ForEach(["pc", "mac", "toggle"], id: \.self) { action in
                        LabeledContent(action == "pc" ? "Send to PC" : action == "mac" ? "Back to Mac" : "Toggle") {
                            HStack(spacing: 6) {
                                Text("tightwire://deck/\(action)").font(.callout.monospaced()).textSelection(.enabled)
                                Button {
                                    NSPasteboard.general.clearContents()
                                    NSPasteboard.general.setString("tightwire://deck/\(action)", forType: .string)
                                } label: {
                                    Image(systemName: "doc.on.doc")
                                }
                                .buttonStyle(.borderless)
                                .help("Copy")
                            }
                        }
                    }
                } header: {
                    Text("Stream Deck buttons")
                } footer: {
                    Text("Use with the Stream Deck “Website” action on the Mac. Off by default: any app or web page can open these URLs.").font(.caption).foregroundStyle(.secondary)
                }

                Section {
                    if model.handoffState.log.isEmpty {
                        Text("Nothing yet.").foregroundStyle(.secondary)
                    } else {
                        VStack(alignment: .leading, spacing: 3) {
                            ForEach(Array(model.handoffState.log.suffix(8).enumerated()), id: \.offset) { _, line in
                                Text(line)
                                    .font(.caption.monospaced())
                                    .foregroundStyle(.secondary)
                                    .lineLimit(2)
                                    .truncationMode(.middle)
                                    .textSelection(.enabled)
                            }
                        }
                    }
                } header: {
                    HStack {
                        Text("Activity")
                        Spacer()
                        Button("Open Log") { model.openLog() }.buttonStyle(.borderless).font(.caption)
                    }
                }
            }
        }
        .confirmationDialog(warningTitle, isPresented: Binding(get: { model.pendingWarning != nil },
                                                                set: { if !$0 { model.confirmWarning(false) } })) {
            Button("Use for Handoff") { model.confirmWarning(true) }
            Button("Cancel", role: .cancel) { model.confirmWarning(false) }
        } message: {
            if case .warn(let msg) = model.pendingWarning?.verdict { Text(msg) }
        }
    }

    var warningTitle: String { "Hand \(model.pendingWarning?.device.name ?? "this device") to the PC?" }

    var ownerDetail: String {
        let st = model.handoffState
        if model.pairing == nil { return "Pair with a PC first." }
        if model.handoffDevices.isEmpty { return "Tick a device below." }
        let mode = st.mode == .heldOnMac ? "Held on the Mac until you send it" : "Follows the PC automatically"
        return "\(mode) · PC \(st.receiverAlive ? "answering" : "not answering")"
    }
}

private struct DeviceRow: View {
    let model: AppModel
    let row: AppModel.HandoffRow

    var chosen: HandoffDevice? { model.selected(row) }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Toggle(isOn: Binding(get: { chosen != nil }, set: { model.setHandoff(row, $0) })) { EmptyView() }
                    .toggleStyle(.checkbox)
                    .labelsHidden()
                    .disabled(isRefused && chosen == nil)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(row.device.name)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .help(row.device.name)
                            .layoutPriority(1)
                        if row.suggested { tag("Suggested", .blue) }
                        if row.shared { tag("Shared", .orange) }
                    }
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 4)
                if model.registering.contains(row.id) {
                    ProgressView().controlSize(.small)
                }
            }
            if let msg = model.rowMessages[row.id] ?? refusal {
                Label(msg, systemImage: "nosign")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 28)
            } else if case .warn = row.verdict {
                Label("Uses: \(row.roles.map(\.rawValue).joined(separator: ", ")). You’ll be asked before it is used.", systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 28)
            }
            if let c = chosen, c.vendor == HandoffDevice.elgatoVendor || c.quitHelperApp {
                Toggle("Quit the Stream Deck app if it holds this device", isOn: Binding(
                    get: { c.quitHelperApp }, set: { model.setQuitHelper(c, $0) }))
                    .font(.caption)
                    .controlSize(.small)
                    .padding(.leading, 28)
            }
        }
        .padding(.vertical, 2)
    }

    var isRefused: Bool { if case .refuse = row.verdict { true } else { false } }
    var refusal: String? { if case .refuse(let m) = row.verdict { m } else { nil } }

    var detail: String {
        var parts = [row.device.model]
        if let b = row.device.busID, row.registered { parts.append("Longcable \(b)") }
        parts.append(row.registered ? (row.shared ? "shared" : "not shared") : "not registered")
        if let loc = row.device.location { parts.append("port \(loc)") }
        return parts.joined(separator: " · ")
    }

    func tag(_ text: String, _ color: Color) -> some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(color)
            .padding(.horizontal, 5).padding(.vertical, 1)
            .background(Capsule().fill(color.opacity(0.18)))
            .fixedSize()
    }
}

// MARK: Setup

private struct SetupPane: View {
    @Bindable var model: AppModel

    var body: some View {
        let s = model.setup
        Form {
            Section {
                row(ok: s.microphone == .authorized, title: "Microphone access",
                    detail: s.microphone == .authorized ? "Allowed" : s.microphone == .notDetermined ? "Needed to stream an input" : "Off in System Settings") {
                    if s.microphone != .authorized {
                        Button(s.microphone == .notDetermined ? "Allow…" : "Open Settings") { model.requestMicrophone() }
                    }
                }
                row(ok: s.paired, title: "Paired with a PC",
                    detail: s.paired ? model.pairing?.peerName ?? "" : "On the PC: Tightwire tray icon › Pair…") {
                    if !s.paired { Button("Pair…") { model.pane = .receiver; model.beginPairing() } }
                }
                row(ok: s.helperRunning, title: "Stream Deck handoff helper",
                    detail: s.helperRunning ? "Longcable is running" : s.helperInstalled ? "Installed but not running"
                        : model.pcIPv4 == nil ? "Optional. Pair first: it will admit only the paired PC." : "Optional. Needs your administrator password.") {
                    if s.helperBundled {
                        if s.helperRunning || s.helperInstalled {
                            Button("Reinstall…") { model.installHelper() }
                            Button("Uninstall…") { model.uninstallHelper() }
                        } else {
                            Button("Install…") { model.installHelper() }
                                .disabled(model.pcIPv4 == nil)
                        }
                    }
                }
                row(ok: s.launchAtLogin == .enabled, title: "Open at login",
                    detail: s.launchAtLogin == .requiresApproval ? "Approve in System Settings › General › Login Items" : s.launchAtLogin == .enabled ? "On" : "Off") {
                    Toggle("Open at login", isOn: Binding(get: { s.launchAtLogin == .enabled }, set: { model.setLaunchAtLogin($0) }))
                        .labelsHidden()
                        .toggleStyle(.switch)
                }
            } header: {
                Text(s.complete ? "Everything is set up" : "\(s.remaining) step\(s.remaining == 1 ? "" : "s") left")
            } footer: {
                if let m = model.setupMessage {
                    Text(m).font(.caption).foregroundStyle(.secondary)
                }
            }

            Section("Diagnostics") {
                LabeledContent("Version", value: BuildInfo.label)
                LabeledContent("Sleep") {
                    Text(model.keepingAwake ? "Keeping this Mac awake while streaming or while a device is on the PC"
                         : "Keeps this Mac awake while streaming or while a device is on the PC")
                        .font(.callout).foregroundStyle(.secondary)
                }
                LabeledContent("Log") {
                    HStack {
                        Text(AppLog.shared.fileURL.path.replacingOccurrences(of: NSHomeDirectory(), with: "~"))
                            .font(.caption.monospaced())
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .textSelection(.enabled)
                        Button("Open Log") { model.openLog() }
                    }
                }
            }
        }
        .onAppear { model.refreshSetup() }
    }

    func row<A: View>(ok: Bool, title: String, detail: String, @ViewBuilder action: () -> A) -> some View {
        HStack(alignment: .center, spacing: 10) {
            Image(systemName: ok ? "checkmark.circle.fill" : "circle.dashed")
                .font(.title3)
                .foregroundStyle(ok ? .green : .orange)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                Text(detail).font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            action()
        }
    }
}

extension SetupStatus {
    var remaining: Int {
        [microphone == .authorized, helperRunning, launchAtLogin == .enabled, paired].filter { !$0 }.count
    }
}
