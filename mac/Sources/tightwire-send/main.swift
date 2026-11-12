import Foundation
import TightwireCore

// Headless sender sharing TightwireCore with the menu bar app (SPEC §4).

let usage = """
usage:
  tightwire-send [options]              stream to the paired receiver
  tightwire-send --list-devices
  tightwire-send pair --to HOST:PORT [--name NAME]
  tightwire-send forget
  tightwire-send handoff-check --device VID:PID --client-ip IP [--hold S]
                                        run the Mac half of a Stream Deck handover with this
                                        Mac as the USB/IP client (IP: an address of this Mac
                                        that Longcable already allows); leaves it unshared

options:
  --device NAME|UID     input device (default: the system default input)
  --channel N           send mono from channel N (1-based)
  --all-channels        send every device channel (max 8; the default for 2+ channels)
  --frames N            frames per packet (default 48)
  --sine HZ             synthetic tone instead of a device (no microphone permission)
  --sine-rate HZ        tone sample rate (default 48000)
  --sine-channels N     tone channels (default 1)
  --latency-test        run the 30 s latency test once live and print its result
  --apply-tuning F,C    set the receiver's buffer floor F ms (0 = adaptive) and cap C ms once live
  --keep-awake          hold the same no-idle-sleep assertion as the app while streaming
  --duration S          stop after S seconds
  --to HOST:PORT        receiver address (default: from the pairing)
  --psk-hex HEX         TEST ONLY: 32-byte root key instead of a stored pairing
                        (visible to anyone who can list processes)
  --pairing-file PATH   pairing store (default ~/Library/Application Support/Tightwire/cli-pairing.json)
  --quiet               only print the final summary
"""

struct CLIError: Error, CustomStringConvertible { let description: String }

var args = Array(CommandLine.arguments.dropFirst())
@MainActor func take(_ flag: String) -> String? {
    guard let i = args.firstIndex(of: flag) else { return nil }
    guard i + 1 < args.count else { fail("\(flag) needs a value") }
    let v = args[i + 1]
    args.removeSubrange(i...(i + 1))
    return v
}
@MainActor func flag(_ f: String) -> Bool {
    guard let i = args.firstIndex(of: f) else { return false }
    args.remove(at: i)
    return true
}
func fail(_ msg: String) -> Never {
    FileHandle.standardError.write("error: \(msg)\n".data(using: .utf8)!)
    exit(1)
}
func log(_ s: String) { FileHandle.standardError.write((s + "\n").data(using: .utf8)!) }

let command = args.first.flatMap { ["pair", "forget", "handoff-check"].contains($0) ? $0 : nil }
if command != nil { args.removeFirst() }
if flag("--help") || flag("-h") { print(usage); exit(0) }

if command == "handoff-check" {
    guard let model = take("--device"), let ip = take("--client-ip") else { fail("handoff-check needs --device VID:PID and --client-ip IP") }
    let hold = take("--hold").flatMap(Double.init) ?? 3
    guard args.isEmpty else { fail("unknown arguments: \(args.joined(separator: " "))") }
    exit(runHandoffCheck(model: model, clientIP: ip, hold: hold))
}

let storeURL = take("--pairing-file").map { URL(fileURLWithPath: $0) }
    ?? FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/Tightwire/cli-pairing.json")
let store = FilePairingStore(url: storeURL)

if command == "forget" {
    do { try store.forget(); print("forgot pairing (\(storeURL.path))") } catch { fail("\(error)") }
    exit(0)
}

if command == "pair" {
    guard let to = take("--to") else { fail("pair needs --to HOST:PORT (the port `tightwire-recv pair` printed)") }
    let name = take("--name") ?? Host.current().localizedName ?? "Mac"
    let auto = flag("--test-auto-accept")
    guard args.isEmpty else { fail("unknown arguments: \(args.joined(separator: " "))") }
    do {
        let ep = try Endpoint.resolve(to, defaultPort: Wire.defaultPort)
        let initiator = PairingInitiator(target: ep, name: name, selfID: try store.selfID())
        var pairing = try initiator.run { code, peer in
            // Code alone on its line; the peer's (validated) name after it.
            print("code: \(code)")
            print("from: \"\(Wire.sanitize(peer, max: Wire.maxPeerNameCharacters))\"")
            if auto { return true }
            print("Does the receiver show the same code? [y/N] ", terminator: "")
            fflush(stdout)
            return ["y", "yes"].contains(readLine()?.lowercased() ?? "")
        }
        // Stream to the receiver's audio port, not the pairing port.
        pairing.lastAddress = nil
        pairing.hostName = nil
        try store.save(pairing)
        print("paired with \"\(pairing.peerName)\" (key fingerprint \(pairing.fingerprint)); saved to \(storeURL.path)")
        exit(0)
    } catch {
        fail("pairing failed: \(error)")
    }
}

if flag("--list-devices") {
    let list = AudioDevices.inputs()
    let def = AudioDevices.defaultInput(in: list)
    for d in list {
        print("\(d.id == def?.id ? "*" : " ") \(d.name)  [\(d.channels) ch, \(Int(d.sampleRate)) Hz]  uid=\(d.uid)")
    }
    exit(0)
}

let frames = UInt32(take("--frames") ?? "48") ?? 48
let duration = take("--duration").flatMap(Double.init)
let sine = take("--sine").flatMap(Double.init)
let sineRate = UInt32(take("--sine-rate") ?? "48000") ?? 48000
let sineChannels = Int(take("--sine-channels") ?? "1") ?? 1
let latencyTest = flag("--latency-test")
let tuningArg = take("--apply-tuning")
let keepAwake = flag("--keep-awake")
let deviceQuery = take("--device")
let to = take("--to")
let psk = take("--psk-hex")
let channelArg = take("--channel").flatMap(Int.init)
let allChannels = flag("--all-channels")
let quiet = flag("--quiet")
guard args.isEmpty else { fail("unknown arguments: \(args.joined(separator: " "))\n\(usage)") }

let pairing: Pairing
if let psk {
    guard let root = Hex.decode(psk), root.count == 32 else { fail("--psk-hex needs exactly 64 hex characters") }
    pairing = Pairing(peerName: "pre-shared key", peerID: [UInt8](repeating: 0, count: 16), rootKey: root,
                      lastAddress: to, hostName: nil)
} else {
    guard var p = try? store.load() else {
        fail("not paired: run `tightwire-recv pair` on the receiver and `tightwire-send pair --to HOST:PORT` here")
    }
    if let to { p.lastAddress = to }
    pairing = p
}
guard pairing.lastAddress != nil || pairing.hostName != nil else { fail("no receiver address: pass --to HOST:PORT") }

let source: AudioSource
let interruption = Locked<String?>(nil)
if let sine {
    source = SineSource(frequency: sine, sampleRate: sineRate, channels: sineChannels, framesPerTick: Int(frames))
} else {
    let list = AudioDevices.inputs()
    guard let dev = AudioDevices.pick(saved: deviceQuery, in: list) else {
        fail("no input device found (see --list-devices)")
    }
    log("input: \(dev.name) (\(dev.channels) ch, \(Int(dev.sampleRate)) Hz)")
    do {
        source = try CoreAudioCapture(device: dev, framesPerBuffer: frames) { interruption.value = $0 }
    } catch { fail("\(error)") }
}
let selection: ChannelSelection = allChannels ? .all
    : channelArg.map { .mono($0 - 1) } ?? (source.channelCount >= 2 ? .all : .mono(0))

let session: Session
do {
    session = try Session(config: SessionConfig(pairing: pairing, address: to, framesPerPacket: frames, channels: selection),
                          source: source)
} catch { fail("\(error)") }

signal(SIGINT, SIG_IGN)
let sigint = DispatchSource.makeSignalSource(signal: SIGINT)
let stopRequested = Locked(false)
sigint.setEventHandler { stopRequested.value = true }
sigint.resume()

let sleepGuard = SleepGuard { log("power: \($0)") }
if keepAwake { sleepGuard.set(true, reason: "tightwire-send: streaming") }
var testStarted = false
var tuningSent = false

let start = Date()
var firstLive: TimeInterval?
var lastLive: TimeInterval?
var lastPrint = Date.distantPast
while !stopRequested.value {
    usleep(50_000)
    let t = Date().timeIntervalSince(start)
    let s = session.status
    if s.phase == .streaming {
        if firstLive == nil { firstLive = t }
        lastLive = t
        // Wait for the first stats so the receiver revision and clock are known.
        if latencyTest, !testStarted, s.stats != nil, t - (firstLive ?? t) > 1 {
            testStarted = true
            session.startLatencyTest()
        }
        if let arg = tuningArg, !tuningSent, s.stats != nil {
            let parts = arg.split(separator: ",").compactMap { Double($0) }
            guard parts.count == 2 else { fail("--apply-tuning needs FLOOR_MS,CAP_MS") }
            tuningSent = true
            session.applyTuning(floorMicros: parts[0] > 0 ? UInt32(parts[0] * 1000) : nil, capMicros: UInt32(parts[1] * 1000))
        }
    }
    if latencyTest, testStarted, s.testResult != nil, duration == nil { break }
    if let msg = interruption.value { log("stopped: \(msg)"); break }
    if !quiet, Date().timeIntervalSince(lastPrint) >= 1 {
        lastPrint = Date()
        var line = "\(s.phase == .streaming ? "streaming" : "searching") → \(s.receiver ?? "?")  sent \(s.packetsSent)  gated \(s.packetsGated)  errors \(s.sendErrors)  peak \(String(format: "%.2f", s.peak))"
        if let st = s.stats {
            line += String(format: "  | rx buf %.2f ms  lost %u  late %u  drift %+d ppm  under %u",
                           Double(st.depthMicros) / 1000, st.lost, st.late, st.driftPPM, st.underruns)
        }
        log(line)
    }
    if let d = duration, t >= d { break }
}
let final = session.status
session.stop()
sleepGuard.set(false, reason: "tightwire-send finished")
print(String(format: "sender summary sent=%llu gated=%llu errors=%llu first_live_s=%.3f last_live_s=%.3f rtt_median_ms=%.3f receiver_revision=%d",
             final.packetsSent, final.packetsGated, final.sendErrors, firstLive ?? -1, lastLive ?? -1,
             final.rtt?.median ?? -1, Int(final.receiverRevision ?? 255)))
if let r = final.testResult {
    print(String(format: "latency summary markers=%d marker_median_ms=%.3f marker_unc_ms=%.3f rtt_p95_ms=%.3f underruns=%u recommend_frames=%d",
                 r.marker?.count ?? 0, r.marker?.median ?? -1, r.markerUncertaintyMillis ?? -1, r.rtt?.p95 ?? -1,
                 r.underruns, r.recommendation.framesPerPacket))
    print(r.logLine)
}
if let t = final.tuning { print("tuning result: \(t)") }
