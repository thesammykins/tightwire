import Foundation
import Testing
import TightwireRT
@testable import TightwireCore

/// SPEC §7.1: the Swift codec, the C SipHash and the C real-time packetizer
/// agree with the shared golden vectors byte for byte.
struct VectorTests {
    // Parsed once from JSON and never mutated; Any blocks automatic Sendable.
    struct Vector: @unchecked Sendable {
        let name: String
        let kind: String
        let bytes: [UInt8]
        let fields: [String: Any]
        let auth: [String: Any]?
        let reason: String?
    }

    static let root: URL = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent().appendingPathComponent("protocol/vectors")

    nonisolated(unsafe) static let doc: [String: Any] = {
        try! JSONSerialization.jsonObject(with: Data(contentsOf: root.appendingPathComponent("vectors.json"))) as! [String: Any]
    }()

    static let vectors: [Vector] = (doc["vectors"] as! [[String: Any]]).map {
        Vector(name: $0["name"] as! String, kind: $0["kind"] as! String,
               bytes: Hex.decode($0["hex"] as! String)!, fields: $0["fields"] as? [String: Any] ?? [:],
               auth: $0["auth"] as? [String: Any], reason: $0["reason"] as? String)
    }

    static let keys = SessionKeys(root: Hex.decode((doc["keys"] as! [String: String])["root"]!)!)

    func uint(_ v: Any?) -> UInt64 { (v as! NSNumber).uint64Value }
    func int(_ v: Any?) -> Int64 { (v as! NSNumber).int64Value }

    func key(_ auth: [String: Any]) -> ([UInt8], UInt64?) {
        let k = (auth["key"] as! String) == "m2r" ? Self.keys.m2r : Self.keys.r2m
        return (k, (auth["nonce"] as? NSNumber)?.uint64Value)
    }

    @Test func siphashReferenceVectors() throws {
        let d = try JSONSerialization.jsonObject(with: Data(contentsOf: Self.root.appendingPathComponent("siphash.json"))) as! [String: Any]
        let key = Hex.decode(d["key"] as! String)!
        for v in d["vectors"] as! [[String: String]] {
            let msg = Hex.decode(v["message"]!)!
            #expect(Hex.encode(Wire.tag(key: key, nonce: nil, body: msg)) == v["tag"]!)
            // Prefix/body split must not change the result.
            for cut in [0, msg.count / 3, msg.count] {
                var out = [UInt8](repeating: 0, count: 8)
                let a = Array(msg[..<cut]), b = Array(msg[cut...])
                a.withUnsafeBufferPointer { pa in
                    b.withUnsafeBufferPointer { pb in
                        tw_siphash24(key, pa.baseAddress, pa.count, pb.baseAddress, pb.count, &out)
                    }
                }
                #expect(Hex.encode(out) == v["tag"]!)
            }
        }
    }

    @Test func keySchedule() {
        let k = Self.doc["keys"] as! [String: String]
        #expect(Hex.encode(Self.keys.m2r) == k["m2r"])
        #expect(Hex.encode(Self.keys.r2m) == k["r2m"])
        let p = Self.doc["pairing"] as! [String: String]
        let s = PairingSecrets(shared: Hex.decode(p["shared_secret"]!)!, commit: Hex.decode(p["commit"]!)!,
                               response: Hex.decode(p["response"]!)!, reveal: Hex.decode(p["reveal"]!)!)
        #expect(Hex.encode(s.transcriptHash) == p["transcript_hash"])
        #expect(Hex.encode(s.root) == p["root"])
        #expect(Hex.encode(s.confirmKey) == p["confirm_key"])
        #expect(String(format: "%06d", s.sas) == p["sas"])
        for v in Self.vectors where v.kind == "confirm" {
            #expect(Hex.encode(s.confirmMAC(role: UInt8(uint(v.fields["role"])))) == v.fields["mac"] as! String)
        }
    }

    @Test(arguments: vectors.filter { !["invalid", "bad_tag"].contains($0.kind) }.map(\.name))
    func roundTrip(name: String) throws {
        let v = Self.vectors.first { $0.name == name }!
        let packet = try Wire.decode(v.bytes)
        let f = v.fields
        switch packet {
        case .audio(let p):
            #expect(v.kind == "audio")
            #expect(UInt64(p.streamID) == uint(f["stream_id"]))
            #expect(UInt64(p.sequence) == uint(f["sequence"]))
            #expect(p.timestamp == uint(f["timestamp"]))
            #expect(UInt64(p.sampleRate) == uint(f["sample_rate"]))
            #expect(UInt64(p.channels) == uint(f["channels"]))
            #expect(UInt64(p.flags) == uint(f["flags"]))
            #expect(p.samples.format.name == f["format"] as! String)
            let raw = f["samples"] as! [NSNumber]
            switch p.samples {
            case .f32(let s): #expect(s == raw.map(\.floatValue))
            case .s24(let s): #expect(s == raw.map(\.int32Value))
            case .s16(let s): #expect(s == raw.map(\.int16Value))
            }
        case .stats(let s):
            #expect(UInt64(s.streamID) == uint(f["stream_id"]))
            #expect(UInt64(s.depthMicros) == uint(f["depth_us"]))
            #expect(UInt64(s.received) == uint(f["received"]))
            #expect(UInt64(s.lost) == uint(f["lost"]))
            #expect(UInt64(s.late) == uint(f["late"]))
            #expect(Int64(s.driftPPM) == int(f["drift_ppm"]))
            #expect(UInt64(s.underruns) == uint(f["underruns"]))
            #expect(UInt64(s.authDropped) == uint(f["auth_dropped"]))
            #expect(UInt64(s.echoSequence) == uint(f["echo_seq"]))
            #expect(s.sessionNonce == uint(f["session_nonce"]))
            if let x = f["ext"] as? [String: Any] {
                let e = try #require(s.ext)
                #expect(UInt64(e.outputMode.rawValue) == uint(x["output_mode"]))
                #expect(UInt64(e.streamChannels) == uint(x["stream_channels"]))
                #expect(UInt64(e.outputChannels) == uint(x["output_channels"]))
                #expect(UInt64(e.targetMicros) == uint(x["target_us"]))
                #expect(UInt64(e.jitterMeanMicros) == uint(x["jitter_mean_us"]))
                #expect(UInt64(e.jitterPeakMicros) == uint(x["jitter_peak_us"]))
                #expect(UInt64(e.minTargetMicros) == uint(x["min_target_us"]))
                #expect(UInt64(e.maxTargetMicros) == uint(x["max_target_us"]))
                #expect(UInt64(e.outputPeriodFrames) == uint(x["output_period_frames"]))
                #expect(UInt64(e.outputRate) == uint(x["output_rate"]))
                #expect(UInt64(e.streamRate) == uint(x["stream_rate"]))
                #expect(Int64(e.driftPPB) == int(x["drift_ppb"]))
                #expect(UInt64(e.duplicates) == uint(x["duplicates"]))
                #expect(UInt64(e.concealedFrames) == uint(x["concealed_frames"]))
                #expect(UInt64(e.resets) == uint(x["resets"]))
                #expect(UInt64(e.ringFill) == uint(x["ring_fill"]))
                #expect(UInt64(e.ringCapacity) == uint(x["ring_capacity"]))
                #expect(UInt64(e.ringFull) == uint(x["ring_full"]))
                #expect(e.markerTimestamp == uint(x["marker_timestamp"]))
                #expect(e.markerOutputMicros == uint(x["marker_output_us"]))
                #expect(e.receiverNowMicros == uint(x["receiver_now_us"]))
                #expect(e.receiverIPv4.map(String.init).joined(separator: ".") == x["receiver_ipv4"] as! String)
                if let y = x["health"] as? [String: Any] {
                    let h = try #require(e.health)
                    #expect(UInt64(h.flags) == uint(y["flags"]))
                    #expect(UInt64(h.deviceQueueMicros) == uint(y["device_queue_us"]))
                    #expect(Int64(h.deviceRatePPB) == int(y["device_rate_ppb"]))
                    #expect(UInt64(h.deviceLeadMicros) == uint(y["device_lead_us"]))
                    #expect(UInt64(h.trims) == uint(y["trims"]))
                    #expect(UInt64(h.skips) == uint(y["skips"]))
                    #expect(UInt64(h.inserts) == uint(y["inserts"]))
                } else {
                    #expect(e.health == nil)
                }
            } else {
                #expect(s.ext == nil)
            }
        case .hello(let nonce, let echo, let ext):
            #expect(nonce == uint(f["session_nonce"]))
            #expect(echo == uint(f["echo"]))
            if let x = f["ext"] as? [String: Any] {
                let e = try #require(ext)
                #expect(UInt64(e.revision) == uint(x["revision"]))
                #expect(e.receiverIPv4.map(String.init).joined(separator: ".") == x["receiver_ipv4"] as! String)
                #expect(e.receiverTimeMicros == uint(x["receiver_time_us"]))
            } else {
                #expect(ext == nil)
            }
        case .tuningSet(let id, let adaptive, let minUs, let maxUs):
            #expect(UInt64(id) == uint(f["request_id"]))
            #expect(adaptive == f["adaptive"] as! Bool)
            #expect(UInt64(minUs) == uint(f["min_target_us"]))
            #expect(UInt64(maxUs) == uint(f["max_target_us"]))
        case .tuningAck(let a):
            #expect(UInt64(a.requestID) == uint(f["request_id"]))
            #expect(UInt64(a.result) == uint(f["result"]))
            #expect(UInt64(a.minTargetMicros) == uint(f["min_target_us"]))
            #expect(UInt64(a.maxTargetMicros) == uint(f["max_target_us"]))
            #expect(a.message == f["message"] as! String)
        case .probe(let nonce):
            #expect(nonce == uint(f["nonce"]))
        case .beacon(let b):
            #expect(UInt64(b.port) == uint(f["port"]))
            #expect(Hex.encode(b.receiverID) == f["receiver_id"] as! String)
            #expect(b.host == f["host"] as! String)
        case .commit(let id, let commitment, let name):
            #expect(Hex.encode(id) == f["id"] as! String)
            #expect(Hex.encode(commitment) == f["commitment"] as! String)
            #expect(name == f["name"] as! String)
        case .response(let id, let pk, let name):
            #expect(Hex.encode(id) == f["id"] as! String)
            #expect(Hex.encode(pk) == f["public_key"] as! String)
            #expect(name == f["name"] as! String)
        case .reveal(let pk):
            #expect(Hex.encode(pk) == f["public_key"] as! String)
        case .confirm(let role, let mac):
            #expect(UInt64(role) == uint(f["role"]))
            #expect(Hex.encode(mac) == f["mac"] as! String)
        case .abort(let reason):
            #expect(UInt64(reason) == uint(f["reason"]))
        case .handoffOffer(let id, let port, let host, let ids):
            #expect(UInt64(id) == uint(f["request_id"]))
            #expect(UInt64(port) == uint(f["usbip_port"]))
            #expect(host == f["host"] as! String)
            #expect(ids == f["bus_ids"] as! [String])
        case .handoffAck(let id, let op, let items):
            #expect(UInt64(id) == uint(f["request_id"]))
            #expect(UInt64(op) == uint(f["op"]))
            let want = f["items"] as! [[String: Any]]
            #expect(items.count == want.count)
            for (i, w) in zip(items, want) {
                #expect(i.busID == w["bus_id"] as! String)
                #expect(UInt64(i.result.rawValue) == uint(w["result"]))
                #expect(UInt64(i.port) == uint(w["port"]))
                #expect(i.message == w["message"] as! String)
            }
        case .handoffRelease(let id, let ids):
            #expect(UInt64(id) == uint(f["request_id"]))
            #expect(ids == f["bus_ids"] as! [String])
        case .deckRequest(let id, let ms, let target):
            #expect(UInt64(id) == uint(f["request_id"]))
            #expect(ms == uint(f["sent_at_ms"]))
            #expect(UInt64(target.rawValue) == uint(f["target"]))
        case .deckAck(let id, let accepted, let pc, let message):
            #expect(UInt64(id) == uint(f["request_id"]))
            #expect(accepted == f["accepted"] as! Bool)
            #expect((pc ? 1 : 0) == uint(f["owner"]))
            #expect(message == f["message"] as! String)
        }
        var re = Wire.encode(packet)
        if let auth = v.auth {
            let (k, n) = key(auth)
            #expect(Wire.verify(v.bytes, key: k, nonce: n))
            re = Wire.seal(re, key: k, nonce: n)
        }
        #expect(re == v.bytes)
    }

    @Test(arguments: vectors.filter { $0.kind == "invalid" }.map(\.name))
    func rejectsInvalid(name: String) {
        let v = Self.vectors.first { $0.name == name }!
        let expected: Wire.DecodeError = switch v.reason! {
        case "magic": .magic
        case "version": .version
        case "type": .type
        case "length": .length
        case "channels": .channels
        case "sample_rate": .sampleRate
        case "format": .format
        case "field": .field
        default: .utf8
        }
        #expect(throws: expected) { try Wire.decode(v.bytes) }
    }

    @Test(arguments: vectors.filter { $0.kind == "bad_tag" }.map(\.name))
    func rejectsBadTags(name: String) throws {
        let v = Self.vectors.first { $0.name == name }!
        _ = try Wire.decode(v.bytes)
        let (k, n) = key(v.auth!)
        #expect(!Wire.verify(v.bytes, key: k, nonce: n))
    }

    /// The C packetizer used on the audio thread must produce the vector bytes, tag included.
    @Test(arguments: vectors.filter { $0.kind == "audio" && ($0.fields["format"] as! String) == "f32" }.map(\.name))
    func realtimePacketizerMatches(name: String) throws {
        let v = Self.vectors.first { $0.name == name }!
        guard case .audio(let p) = try Wire.decode(v.bytes), case .f32(let samples) = p.samples else {
            Issue.record("not f32 audio"); return
        }
        let (k, nonce) = key(v.auth!)
        var out = [UInt8](repeating: 0, count: Int(TW_AUDIO_HEADER_LEN + TW_MAX_PAYLOAD + TW_TAG_LEN))
        let len = samples.withUnsafeBufferPointer { s in
            tw_build_audio_packet(&out, k, nonce!, p.streamID, p.sequence, p.timestamp,
                                  UInt8(Wire.sampleRates.firstIndex(of: p.sampleRate)!), p.channels,
                                  p.flags, s.baseAddress!, UInt32(s.count))
        }
        #expect(Array(out[..<len]) == v.bytes)
    }
}
