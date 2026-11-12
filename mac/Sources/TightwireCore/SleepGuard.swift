import Foundation
import IOKit.pwr_mgt

/// Keeps the Mac from idle-sleeping (the display may still sleep) while
/// audio streams or a USB device is on the PC: idle sleep would cut both
/// mid-use. Holds one IOPMAssertion of type PreventUserIdleSystemSleep,
/// visible in `pmset -g assertions`.
public final class SleepGuard: @unchecked Sendable {
    private var assertion: IOPMAssertionID = 0
    private var held = false
    private let lock = NSLock()
    private let log: @Sendable (String) -> Void

    public init(log: @escaping @Sendable (String) -> Void) { self.log = log }

    public var isHeld: Bool { lock.withLock { held } }

    /// Acquires or releases; `reason` is shown by pmset and logged.
    public func set(_ on: Bool, reason: String) {
        lock.withLock {
            if on, !held {
                let r = IOPMAssertionCreateWithName(kIOPMAssertPreventUserIdleSystemSleep as CFString,
                                                    IOPMAssertionLevel(kIOPMAssertionLevelOn), reason as CFString, &assertion)
                held = r == kIOReturnSuccess
                log(held ? "keeping the Mac awake: \(reason)" : "could not keep the Mac awake (IOReturn \(r))")
            } else if !on, held {
                IOPMAssertionRelease(assertion)
                held = false
                log("no longer keeping the Mac awake (\(reason))")
            }
        }
    }

    deinit { if held { IOPMAssertionRelease(assertion) } }
}
