import Foundation

/// Quitting and relaunching the app that holds a device (the Elgato Stream
/// Deck app may hold the deck open; Longcable cannot capture a held device).
public protocol HelperAppControlling: Sendable {
    func isRunning(_ bundleID: String) -> Bool
    /// Asks the app to quit normally and waits up to `timeout`; true once it is gone.
    func quit(_ bundleID: String, timeout: TimeInterval) -> Bool
    func launch(_ bundleID: String)
}

/// The authenticated handoff messages to the receiver (SPEC §9).
public protocol HandoffTransport: Sendable {
    func offer(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem]
    func release(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem]
}

public struct HandoffTimeouts: Sendable {
    /// Until `share` shows up as enabled in Longcable's status.
    public var shareReady: TimeInterval = 3
    public var helperQuit: TimeInterval = 5
    /// Pause after quitting the helper app so macOS releases the device.
    public var afterHelperQuit: TimeInterval = 1
    public var offer: TimeInterval = 10
    public var release: TimeInterval = 2
    public var poll: TimeInterval = 0.2
    public init() {}
}

/// Runs the handover/handback sequences against injected collaborators.
/// Every step logs; every failure leaves nothing shared.
public final class HandoffSequencer: @unchecked Sendable {
    public static let defaultHelperBundleID = "com.elgato.StreamDeck"

    let longcable: LongcableControlling
    let apps: HelperAppControlling
    let transport: HandoffTransport
    let sleep: @Sendable (TimeInterval) -> Void
    let log: @Sendable (String) -> Void
    public var helperBundleID: String
    public var timeouts = HandoffTimeouts()
    /// Called when a device's Longcable BUSID/location is learned or changes,
    /// so the caller can persist it.
    public var onResolved: (@Sendable (HandoffDevice, _ busID: String, _ location: String?) -> Void)?

    /// BUSIDs this Mac currently has shared on purpose.
    public private(set) var shared: Set<String> = []
    /// Whether we quit the helper app and owe it a relaunch.
    public private(set) var helperQuit = false

    public init(longcable: LongcableControlling, apps: HelperAppControlling, transport: HandoffTransport,
                helperBundleID: String = HandoffSequencer.defaultHelperBundleID,
                sleep: @escaping @Sendable (TimeInterval) -> Void = { Thread.sleep(forTimeInterval: $0) },
                log: @escaping @Sendable (String) -> Void) {
        self.longcable = longcable
        self.apps = apps
        self.transport = transport
        self.helperBundleID = helperBundleID
        self.sleep = sleep
        self.log = log
    }

    public struct Failure: Error, CustomStringConvertible, Equatable {
        public let description: String
        public init(_ d: String) { description = d }
    }

    /// None of the handoff devices is plugged in: nothing was shared, and
    /// trying again is pointless until one appears.
    public struct NothingConnected: Error, CustomStringConvertible, Equatable {
        public var description: String { "None of the handoff devices is connected" }
        public init() {}
    }

    /// True when at least one of `devices` is plugged in, as Longcable sees
    /// it (a status read; shares and registers nothing).
    public func anyConnected(_ devices: [HandoffDevice]) -> Bool {
        guard let status = try? longcable.status() else { return false }
        return devices.contains { d in
            if let t = Longcable.target(for: d, in: status) { return t.present }
            return Longcable.local(for: d, in: status) != nil
        }
    }

    // MARK: Registration

    /// Registers a connected device with Longcable and leaves it **unshared**.
    /// Longcable shares every newly added target immediately, so the unshare
    /// follows at once and is verified through status.
    @discardableResult
    public func register(_ local: Longcable.LocalDevice) throws -> Longcable.Device {
        let model = "\(local.vendor):\(local.product)".lowercased()
        log("registering \(local.name.isEmpty ? model : local.name) (\(model) at \(local.location)) with Longcable")
        try longcable.add(local)
        let s = try longcable.status()
        guard let t = s.devices?.first(where: { $0.model?.lowercased() == model && $0.location?.lowercased() == local.location.lowercased() }),
              let id = t.id
        else { throw Failure("Longcable did not list \(local.name) after adding it") }
        log("Longcable registered it as \(id); unsharing until a handover")
        return try unshareVerified(id)
    }

    /// Unshares `id` and confirms through status that it is no longer enabled.
    @discardableResult
    public func unshareVerified(_ id: String) throws -> Longcable.Device {
        try longcable.unshare(id)
        guard let t = try longcable.status().devices?.first(where: { $0.id == id }) else {
            throw Failure("Longcable no longer lists \(id)")
        }
        guard t.enabled == false else { throw Failure("Longcable still shares \(id) after unshare (\(t.summary))") }
        log("unshared \(t.summary)")
        return t
    }

    /// Maps each wanted device to its BUSID, registering (unshared) connected
    /// devices Longcable does not know yet. Unplugged devices are skipped.
    func resolve(_ devices: [HandoffDevice]) throws -> [(HandoffDevice, String)] {
        var status = try longcable.status()
        var out: [(HandoffDevice, String)] = []
        for d in devices {
            if let t = Longcable.target(for: d, in: status), let id = t.id {
                if d.busID != id || (t.location != nil && d.location != t.location) { onResolved?(d, id, t.location) }
                guard t.present else {
                    log("\(d.name) (\(id)) is not plugged in; skipping")
                    continue
                }
                out.append((d, id))
            } else if let local = Longcable.local(for: d, in: status) {
                let t = try register(local)
                if let id = t.id {
                    onResolved?(d, id, t.location)
                    out.append((d, id))
                }
                status = try longcable.status()
            } else {
                log("\(d.name) (\(d.model)) is not connected; skipping")
            }
        }
        return out
    }

    private func device(_ id: String, in s: Longcable.Status?) -> Longcable.Device? {
        s?.devices?.first { $0.id == id }
    }

    // MARK: Peer

    /// The PC address this sequencer added to Longcable's allow-list; it is
    /// removed again on handback so the list is narrow while the devices are
    /// on the Mac.
    public private(set) var addedPeer: String?

    /// Makes sure Longcable lists the PC itself (/32) on TCP 3240, verified
    /// through status. A wider range that happens to contain the PC (the
    /// installer's LAN subnet or tailnet default) is not taken as permission:
    /// the PC is added explicitly and the wide entries are reported so the
    /// owner can remove them (Stream Deck pane › Allow only this PC).
    func ensurePeerAllowed(_ ip: String) throws {
        var probe = in_addr()
        guard inet_pton(AF_INET, ip, &probe) == 1 else {
            throw Failure("Longcable only accepts IPv4 peers; the PC is known as \(ip)")
        }
        let before = try longcable.status().allowedPeers ?? []
        let wide = Longcable.wideEntries(before)
        if !wide.isEmpty {
            log("Longcable also admits \(wide.joined(separator: ", ")): every machine there may import shared devices")
        }
        if Longcable.listsExactly(ip, in: before) {
            log("Longcable already lists \(ip)")
            return
        }
        try longcable.allowPeer(ip)
        guard let after = try longcable.status().allowedPeers else {
            throw Failure("Longcable does not report its allowed peers, so \(ip) cannot be verified")
        }
        guard Longcable.listsExactly(ip, in: after) else {
            throw Failure("Longcable did not add \(ip) to its allowed peers (now: \(after.joined(separator: ", ")))")
        }
        addedPeer = ip
        log("Longcable now allows \(ip) (\(after.joined(separator: ", ")))")
    }

    /// Removes a peer this sequencer added (after handback).
    func forgetAddedPeer() {
        guard let ip = addedPeer else { return }
        do {
            try longcable.forgetPeer(ip)
            addedPeer = nil
            log("Longcable no longer allows \(ip) (devices are back on the Mac)")
        } catch {
            log("could not remove \(ip) from Longcable's allowed peers: \(error)")
        }
    }

    /// "Allow only this PC": drops every wide entry and the PC's old
    /// address, and lists `pc` exactly; verified through status.
    public func narrowAllowList(to pc: String, forgetting old: [String] = []) throws -> [String] {
        let before = try longcable.status().allowedPeers ?? []
        for p in Longcable.wideEntries(before) + old.filter({ $0 != pc && Longcable.listsExactly($0, in: before) }) {
            try longcable.forgetPeer(p)
            log("Longcable: removed \(p) from the allowed peers")
        }
        if !Longcable.listsExactly(pc, in: before) { try longcable.allowPeer(pc) }
        let after = try longcable.status().allowedPeers ?? []
        guard Longcable.wideEntries(after).isEmpty, Longcable.listsExactly(pc, in: after) else {
            throw Failure("Longcable's allowed peers are now \(after.joined(separator: ", ")); expected only \(pc)")
        }
        log("Longcable now allows only \(after.joined(separator: ", "))")
        return after
    }

    // MARK: Sequences

    /// Polls until every id is enabled and present, or `timeout` passes.
    private func waitShared(_ ids: [String], timeout: TimeInterval) -> (ready: Set<String>, last: Longcable.Status?) {
        var waited: TimeInterval = 0
        var last: Longcable.Status?
        while true {
            if let s = try? longcable.status() {
                last = s
                let ready = Set(ids.filter { id in device(id, in: s).map { $0.enabled == true && $0.present } ?? false })
                if ready.count == ids.count || waited >= timeout { return (ready, s) }
            } else if waited >= timeout {
                return ([], last)
            }
            sleep(timeouts.poll)
            waited += timeouts.poll
        }
    }

    /// Quits the helper app if it runs; true when it quit.
    private func quitHelper(for names: [String]) -> Bool {
        guard apps.isRunning(helperBundleID) else {
            log("\(names.joined(separator: ", ")) held by another app, but \(helperBundleID) is not running")
            return false
        }
        log("\(names.joined(separator: ", ")) held by another app; quitting \(helperBundleID)")
        guard apps.quit(helperBundleID, timeout: timeouts.helperQuit) else {
            log("\(helperBundleID) did not quit within \(Int(timeouts.helperQuit)) s")
            return false
        }
        helperQuit = true
        log("\(helperBundleID) quit")
        sleep(timeouts.afterHelperQuit)
        return true
    }

    /// Devices whose Longcable lastError says another app holds them and
    /// whose quit-helper option is on.
    private func heldByHelper(_ pairs: [(HandoffDevice, String)], _ s: Longcable.Status?) -> [(HandoffDevice, String)] {
        pairs.filter { d, id in d.quitHelperApp && Longcable.isBusy(device(id, in: s)?.lastError) }
    }

    private func unshareQuietly(_ id: String, _ why: String) {
        log("\(why); unsharing \(id)")
        do { try unshareVerified(id) } catch { log("unshare \(id) failed: \(error)") }
        shared.remove(id)
    }

    /// Mac → PC. Returns the BUSIDs the receiver attached.
    public func handOver(_ devices: [HandoffDevice], pcAddress: String) throws -> [String] {
        do {
            log("handover to \(pcAddress): \(devices.map(\.name).joined(separator: ", "))")
            let resolved = try resolve(devices)
            guard !resolved.isEmpty else { throw NothingConnected() }
            try ensurePeerAllowed(pcAddress)
            for (d, id) in resolved {
                try longcable.share(id)
                shared.insert(id)
                log("share \(id) (\(d.name))")
            }
            let ids = resolved.map(\.1)
            let (ready, status) = waitShared(ids, timeout: timeouts.shareReady)
            for id in ids where !ready.contains(id) {
                unshareQuietly(id, "\(id) is not shared after \(Int(timeouts.shareReady)) s (\(device(id, in: status)?.summary ?? "not listed"))")
            }
            guard !ready.isEmpty else { throw Failure("Longcable did not share the devices") }
            for id in ids where ready.contains(id) { if let t = device(id, in: status) { log("shared \(t.summary)") } }
            var offered = resolved.filter { ready.contains($0.1) }

            // A previous attempt may already have recorded that an app holds it.
            let heldBefore = heldByHelper(offered, status)
            if !heldBefore.isEmpty { _ = quitHelper(for: heldBefore.map(\.0.name)) }

            log("offering \(offered.map(\.1).joined(separator: ", ")) to the PC")
            var items = try transport.offer(busIDs: offered.map(\.1), timeout: timeouts.offer)
            // The PC's import is what makes Longcable open the device; if an app
            // holds it, that is when "In use by …" appears. Quit and retry once.
            let failed = offered.filter { _, id in items.first(where: { $0.busID == id })?.result != .ok }
            if !failed.isEmpty, !helperQuit {
                let s = try? longcable.status()
                for (_, id) in failed { if let t = device(id, in: s) { log("after failed attach: \(t.summary)") } }
                let held = heldByHelper(failed, s)
                if !held.isEmpty, quitHelper(for: held.map(\.0.name)) {
                    log("offering \(held.map(\.1).joined(separator: ", ")) again")
                    let retry = try transport.offer(busIDs: held.map(\.1), timeout: timeouts.offer)
                    items = items.filter { i in !retry.contains { $0.busID == i.busID } } + retry
                }
            }
            offered = offered.filter { _, id in shared.contains(id) }

            var attached: [String] = []
            for (_, id) in offered {
                if let item = items.first(where: { $0.busID == id }), item.result == .ok {
                    attached.append(id)
                    log("PC attached \(id) on port \(item.port)")
                } else {
                    let msg = items.first(where: { $0.busID == id })?.message ?? "no answer"
                    unshareQuietly(id, "PC did not attach \(id): \(msg)")
                }
            }
            guard !attached.isEmpty else {
                let reason = items.first(where: { $0.result != .ok })?.message ?? "no device attached"
                throw Failure("The PC could not attach: \(reason)")
            }
            return attached
        } catch {
            log("handover failed: \(error); reclaiming")
            handBack(devices, notifyPC: true)
            throw (error as? Failure) ?? Failure("\(error)")
        }
    }

    /// PC → Mac. Never throws: asks the PC to let go, then unshares regardless,
    /// and verifies through status. Returns BUSIDs that are still shared.
    /// `extraBusIDs`: other targets Tightwire registered earlier that must
    /// not stay shared (start-up reconcile).
    @discardableResult
    public func handBack(_ devices: [HandoffDevice], notifyPC: Bool = true, extraBusIDs: Set<String> = []) -> [String] {
        var ids = shared
        let status = try? longcable.status()
        // Also catch handoff devices shared by someone else (e.g. Longcable
        // re-enabling them after a restart, or a target it shared on `add`).
        for d in devices {
            if let t = status.flatMap({ Longcable.target(for: d, in: $0) }), t.enabled == true, let id = t.id { ids.insert(id) }
        }
        for id in extraBusIDs where device(id, in: status)?.enabled == true { ids.insert(id) }
        let list = ids.sorted()
        if notifyPC, !list.isEmpty {
            do {
                let items = try transport.release(busIDs: list, timeout: timeouts.release)
                for i in items where i.result != .ok { log("PC could not detach \(i.busID): \(i.message)") }
            } catch {
                log("PC did not confirm release (\(error)); unsharing anyway")
            }
        }
        var still: [String] = []
        for id in list {
            do {
                try unshareVerified(id)
            } catch {
                log("unshare \(id) failed: \(error)")
                still.append(id)
            }
        }
        shared.removeAll()
        if still.isEmpty { forgetAddedPeer() }
        if helperQuit {
            apps.launch(helperBundleID)
            log("relaunched \(helperBundleID)")
            helperQuit = false
        }
        return still
    }

    /// The owner unticked `d`: give it back (asking the PC first if we had
    /// shared it) and leave it unshared. Longcable's control API cannot remove
    /// a target, so it stays registered, unshared.
    public func deselect(_ d: HandoffDevice) {
        guard let s = try? longcable.status(), let t = Longcable.target(for: d, in: s), let id = t.id else {
            log("\(d.name) removed from handoff (not registered with Longcable)")
            return
        }
        if shared.contains(id) {
            _ = try? transport.release(busIDs: [id], timeout: timeouts.release)
        }
        if t.enabled == true || shared.contains(id) {
            do { try unshareVerified(id) } catch { log("unshare \(id) failed: \(error)") }
        }
        shared.remove(id)
        log("\(d.name) removed from handoff; \(id) stays registered with Longcable, unshared")
    }
}

/// Decides who should own the handoff devices (SPEC §9). Pure state, driven
/// by liveness, manual commands and sequence results.
public struct HandoffMachine: Equatable, Sendable {
    public enum Owner: String, Sendable { case mac, pc }
    public enum Mode: String, Sendable { case auto, heldOnMac }
    public enum Command: Equatable, Sendable { case handOver, handBack }

    public static let lossGrace: TimeInterval = 5
    /// After a failed automatic handover, wait this long before trying again.
    public static let retryAfterFailure: TimeInterval = 30

    public private(set) var owner: Owner = .mac
    public private(set) var mode: Mode = .auto
    public private(set) var running: Command?
    public private(set) var lastAlive: TimeInterval?
    private var wentAwayWhileHeld = false
    private var retryAt: TimeInterval = 0
    /// A handover found none of the devices plugged in: no automatic retry
    /// (no 30 s loop, no sleep-assertion churn) until one appears
    /// (`devicesAppeared`) or the owner asks again (`request`).
    public private(set) var waitingForDevice = false

    public init(mode: Mode = .auto) {
        self.mode = mode
    }

    func lost(_ now: TimeInterval) -> Bool {
        guard let t = lastAlive else { return true }
        return now - t >= Self.lossGrace
    }

    /// Feed liveness; returns a command to start, if any.
    public mutating func update(alive: Bool, now: TimeInterval) -> Command? {
        if alive {
            lastAlive = now
            if mode == .heldOnMac && wentAwayWhileHeld {
                // "Held" lasts until the PC next reconnects after going away.
                mode = .auto
                wentAwayWhileHeld = false
            }
        } else if lastAlive != nil, lost(now), mode == .heldOnMac {
            // Only a PC that was seen and then lost counts as "went away"; a
            // restored hold must survive the start-up period before first contact.
            wentAwayWhileHeld = true
        }
        return next(alive: alive, now: now)
    }

    public mutating func request(_ target: Wire.DeckTarget, alive: Bool, now: TimeInterval) -> Command? {
        let effective: Wire.DeckTarget = switch target {
        case .toggle: (owner == .pc || running == .handOver) ? .mac : .pc
        default: target
        }
        switch effective {
        case .mac:
            mode = .heldOnMac
            wentAwayWhileHeld = false
        default:
            mode = .auto
            retryAt = 0
            waitingForDevice = false
        }
        if alive { lastAlive = now }
        return next(alive: alive, now: now)
    }

    /// A handover ended because nothing to hand over is plugged in.
    public mutating func finishedNothingConnected() {
        running = nil
        owner = .mac
        waitingForDevice = true
    }

    /// A handoff device is plugged in again: automatic handover may resume.
    public mutating func devicesAppeared(alive: Bool, now: TimeInterval) -> Command? {
        guard waitingForDevice else { return nil }
        waitingForDevice = false
        retryAt = 0
        return next(alive: alive, now: now)
    }

    public mutating func finished(_ command: Command, success: Bool, now: TimeInterval) {
        running = nil
        switch command {
        case .handOver:
            owner = success ? .pc : .mac
            if !success { retryAt = now + Self.retryAfterFailure }
        case .handBack:
            owner = .mac
        }
    }

    private mutating func next(alive: Bool, now: TimeInterval) -> Command? {
        guard running == nil else { return nil }
        let wantPC = mode == .auto && alive && !lost(now)
        let wantMac = mode == .heldOnMac || lost(now)
        if owner == .mac, wantPC, now >= retryAt, !waitingForDevice {
            running = .handOver
            return .handOver
        }
        if owner == .pc, wantMac {
            running = .handBack
            return .handBack
        }
        return nil
    }
}

/// For Longcable work that involves no PC (registering or unsharing a device
/// while no receiver is paired). Any PC message fails.
public struct NoPCTransport: HandoffTransport {
    public init() {}
    public func offer(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
        throw HandoffSequencer.Failure("No PC is paired")
    }
    public func release(busIDs: [String], timeout: TimeInterval) throws -> [Wire.HandoffItem] {
        throw HandoffSequencer.Failure("No PC is paired")
    }
}
