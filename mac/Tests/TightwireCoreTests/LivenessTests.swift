import Testing
@testable import TightwireCore

/// SPEC §3a liveness gate, driven with a fake clock.
struct LivenessTests {
    @Test func startsGatedAndProbes() {
        let l = Liveness()
        #expect(!l.isLive(at: 0))
        #expect(l.shouldProbe(at: 0))
    }

    @Test func echoedHelloOpensGateForThreeSeconds() {
        var l = Liveness()
        l.probeSent(nonce: 42, at: 0)
        let r1 = l.hello(sessionNonce: 7, echo: 42, at: 0.01)
        #expect(r1)
        #expect(l.isLive(at: 2.9))
        #expect(l.sessionNonce == 7)
        #expect(!l.isLive(at: 3.02), "gate must close 3 s after the last proof")
    }

    @Test func unsolicitedOrReplayedHelloDoesNotOpenGate() {
        var l = Liveness()
        let r2 = l.hello(sessionNonce: 7, echo: 0, at: 0)
        #expect(!r2, "start-up hello only prompts a probe")
        #expect(l.shouldProbe(at: 0.01))
        l.probeSent(nonce: 42, at: 0)
        let r3 = l.hello(sessionNonce: 7, echo: 41, at: 0.1)
        #expect(!r3, "echo of a nonce we never sent")
        let r4 = l.hello(sessionNonce: 7, echo: 42, at: 0.1)
        #expect(r4)
        let r5 = l.hello(sessionNonce: 7, echo: 42, at: 4.0)
        #expect(!r5, "same echo replayed later")
        #expect(!l.isLive(at: 4.0))
    }

    @Test func probeEchoExpires() {
        var l = Liveness()
        l.probeSent(nonce: 42, at: 0)
        let r6 = l.hello(sessionNonce: 7, echo: 42, at: 3.5)
        #expect(!r6, "answer to a stale probe is not proof")
    }

    @Test func freshStatsKeepGateOpen() {
        var l = Liveness()
        l.probeSent(nonce: 1, at: 0)
        l.hello(sessionNonce: 9, echo: 1, at: 0)
        let r7 = l.stats(echoSequence: 990, sessionNonce: 9, lastSentSequence: 1000, packetsPerWindow: 3008, at: 2.5)
        #expect(r7)
        #expect(l.isLive(at: 5.0))
        #expect(!l.shouldProbe(at: 3.0), "fresh stats mean no probe needed yet")
    }

    @Test func staleOrForeignStatsRejected() {
        var l = Liveness()
        l.probeSent(nonce: 1, at: 0)
        l.hello(sessionNonce: 9, echo: 1, at: 0)
        let replayed = l.stats(echoSequence: 10, sessionNonce: 9, lastSentSequence: 10_000, packetsPerWindow: 3008, at: 1)
        #expect(!replayed, "echo too far behind: replayed stats")
        let foreign = l.stats(echoSequence: 999, sessionNonce: 8, lastSentSequence: 1000, packetsPerWindow: 3008, at: 1)
        #expect(!foreign, "different receiver session")
        #expect(!l.isLive(at: 3.1))
    }

    @Test func refreshesBeforeExpiry() {
        var l = Liveness()
        l.probeSent(nonce: 1, at: 0)
        l.hello(sessionNonce: 9, echo: 1, at: 0)
        #expect(!l.shouldProbe(at: 1.0))
        #expect(l.shouldProbe(at: 1.6), "probe once evidence is 1.5 s old so streaming never gates")
    }
}
