// Real-time half of the sender. Everything reachable from tw_input_ioproc runs
// on the CoreAudio IO thread: no allocation, no locks, no logging, no
// Objective-C/Swift runtime. The only syscall is a non-blocking send(); if the
// socket buffer is full the packet is dropped and counted rather than waited on.

#include "tightwire_rt.h"

#include <errno.h>
#include <mach/mach_time.h>
#include <math.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>

// ---------------------------------------------------------------- SipHash-2-4

static inline uint64_t rotl(uint64_t x, int b) { return (x << b) | (x >> (64 - b)); }

static inline uint64_t load64(const uint8_t *p) {
    uint64_t v = 0;
    for (int i = 7; i >= 0; i--) v = (v << 8) | p[i];
    return v;
}

#define SIPROUND                                                                                     \
    do {                                                                                             \
        v0 += v1; v1 = rotl(v1, 13); v1 ^= v0; v0 = rotl(v0, 32);                                    \
        v2 += v3; v3 = rotl(v3, 16); v3 ^= v2;                                                       \
        v0 += v3; v3 = rotl(v3, 21); v3 ^= v0;                                                       \
        v2 += v1; v1 = rotl(v1, 17); v1 ^= v2; v2 = rotl(v2, 32);                                    \
    } while (0)

void tw_siphash24(const uint8_t *key, const uint8_t *prefix, size_t prefix_len, const uint8_t *data,
                  size_t len, uint8_t *out) {
    uint64_t k0 = load64(key), k1 = load64(key + 8);
    uint64_t v0 = k0 ^ 0x736f6d6570736575ULL, v1 = k1 ^ 0x646f72616e646f6dULL;
    uint64_t v2 = k0 ^ 0x6c7967656e657261ULL, v3 = k1 ^ 0x7465646279746573ULL;
    size_t total = prefix_len + len;
    // Treat prefix ‖ data as one stream without copying it.
    uint8_t block[8];
    size_t filled = 0;
    for (int part = 0; part < 2; part++) {
        const uint8_t *p = part == 0 ? prefix : data;
        size_t n = part == 0 ? prefix_len : len;
        if (!p) continue;
        while (n > 0) {
            if (filled == 0 && n >= 8) {
                uint64_t m = load64(p);
                v3 ^= m; SIPROUND; SIPROUND; v0 ^= m;
                p += 8; n -= 8;
                continue;
            }
            block[filled++] = *p++;
            n--;
            if (filled == 8) {
                uint64_t m = load64(block);
                v3 ^= m; SIPROUND; SIPROUND; v0 ^= m;
                filled = 0;
            }
        }
    }
    uint64_t b = (uint64_t)(total & 0xff) << 56;
    for (size_t i = 0; i < filled; i++) b |= (uint64_t)block[i] << (8 * i);
    v3 ^= b; SIPROUND; SIPROUND; v0 ^= b;
    v2 ^= 0xff;
    SIPROUND; SIPROUND; SIPROUND; SIPROUND;
    uint64_t h = v0 ^ v1 ^ v2 ^ v3;
    for (int i = 0; i < 8; i++) out[i] = (uint8_t)(h >> (8 * i));
}

// ---------------------------------------------------------------- packetizer

struct tw_sender {
    tw_sender_config config;
    // Written only by the audio thread after start.
    uint32_t sequence;
    uint64_t timestamp;
    bool discontinuity_pending;
    bool was_live;
    bool have_first_sample_time;
    double first_sample_time;
    float gather[TW_MAX_PAYLOAD / sizeof(float)];
    uint8_t packet[TW_AUDIO_HEADER_LEN + TW_MAX_PAYLOAD + TW_TAG_LEN];
    // Set by the control thread.
    _Atomic bool live;
    _Atomic uint64_t session_nonce;
    // Read by the UI thread.
    _Atomic uint32_t last_sequence;
    _Atomic uint64_t packets_sent;
    _Atomic uint64_t packets_gated;
    _Atomic uint64_t send_errors;
    _Atomic uint64_t frames;
    _Atomic int32_t last_errno;
    _Atomic uint32_t peak_bits;
    // Latency marker: requested by the control thread, published by the
    // audio thread (fields first, then seq with release).
    _Atomic bool marker_requested;
    _Atomic uint32_t marker_seq;
    _Atomic uint64_t marker_timestamp;
    _Atomic uint64_t marker_send_ns;
    _Atomic uint64_t marker_capture_ns;
    // Set at create: mach ticks → ns, and frames → ns.
    double ns_per_tick;
    double ns_per_frame;
    uint64_t buffer_host_ns;  // capture time of the current buffer's first frame (0 = unknown)
};

static const double rates[] = {44100.0, 48000.0, 88200.0, 96000.0};

static inline uint64_t now_ns(const tw_sender *s) { return (uint64_t)((double)mach_absolute_time() * s->ns_per_tick); }

static inline void put16(uint8_t *p, uint16_t v) {
    p[0] = (uint8_t)v;
    p[1] = (uint8_t)(v >> 8);
}

static inline void put32(uint8_t *p, uint32_t v) {
    put16(p, (uint16_t)v);
    put16(p + 2, (uint16_t)(v >> 16));
}

static inline void put64(uint8_t *p, uint64_t v) {
    put32(p, (uint32_t)v);
    put32(p + 4, (uint32_t)(v >> 32));
}

size_t tw_build_audio_packet(uint8_t *out, const uint8_t *key, uint64_t session_nonce, uint16_t stream_id,
                             uint32_t sequence, uint64_t timestamp, uint8_t rate_code, uint8_t channels,
                             uint8_t flags, const float *samples, uint32_t sample_count) {
    memcpy(out, "TWIR", 4);
    out[4] = TW_PROTOCOL_VERSION;
    out[5] = 1;  // type: audio
    put16(out + 6, stream_id);
    put32(out + 8, sequence);
    put64(out + 12, timestamp);
    out[20] = rate_code;
    out[21] = channels;
    out[22] = 0;  // f32: senders emit float only
    out[23] = flags;
    // arm64 and x86_64 are little-endian, so float bits copy straight through.
    size_t len = TW_AUDIO_HEADER_LEN + sample_count * sizeof(float);
    memcpy(out + TW_AUDIO_HEADER_LEN, samples, sample_count * sizeof(float));
    uint8_t nonce[8];
    put64(nonce, session_nonce);
    tw_siphash24(key, nonce, 8, out, len, out + len);
    return len + TW_TAG_LEN;
}

tw_sender *tw_sender_create(const tw_sender_config *config) {
    if (config->channels < 1 || config->channels > TW_MAX_CHANNELS) return NULL;
    tw_sender *s = calloc(1, sizeof(tw_sender));
    if (!s) return NULL;
    s->config = *config;
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    s->ns_per_tick = (double)tb.numer / (double)tb.denom;
    s->ns_per_frame = 1e9 / rates[config->rate_code < 4 ? config->rate_code : 1];
    uint32_t max_frames = TW_MAX_PAYLOAD / (sizeof(float) * config->channels);
    if (s->config.frames_per_packet == 0 || s->config.frames_per_packet > max_frames)
        s->config.frames_per_packet = max_frames;
    // A fresh stream id plus this flag tells the receiver to flush and resync.
    s->discontinuity_pending = true;
    return s;
}

void tw_sender_destroy(tw_sender *sender) { free(sender); }

uint32_t tw_sender_frames_per_packet(const tw_sender *sender) { return sender->config.frames_per_packet; }

void tw_sender_set_live(tw_sender *s, bool live, uint64_t session_nonce) {
    atomic_store_explicit(&s->session_nonce, session_nonce, memory_order_relaxed);
    // Release: the audio thread sees the nonce no later than the gate opening.
    atomic_store_explicit(&s->live, live, memory_order_release);
}

uint32_t tw_sender_last_sequence(const tw_sender *s) {
    return atomic_load_explicit(&((tw_sender *)s)->last_sequence, memory_order_relaxed);
}

void tw_sender_read_counters(tw_sender *s, tw_sender_counters *out) {
    out->packets_sent = atomic_load_explicit(&s->packets_sent, memory_order_relaxed);
    out->packets_gated = atomic_load_explicit(&s->packets_gated, memory_order_relaxed);
    out->send_errors = atomic_load_explicit(&s->send_errors, memory_order_relaxed);
    out->frames = atomic_load_explicit(&s->frames, memory_order_relaxed);
    out->last_errno = atomic_load_explicit(&s->last_errno, memory_order_relaxed);
    uint32_t bits = atomic_exchange_explicit(&s->peak_bits, 0, memory_order_relaxed);
    memcpy(&out->peak, &bits, sizeof bits);
}

static void raise_peak(tw_sender *s, float peak) {
    uint32_t bits;
    memcpy(&bits, &peak, sizeof bits);
    // Non-negative floats order the same as their bit patterns, so an integer
    // CAS-max works. Lock-free on every Apple platform.
    uint32_t cur = atomic_load_explicit(&s->peak_bits, memory_order_relaxed);
    while (bits > cur &&
           !atomic_compare_exchange_weak_explicit(&s->peak_bits, &cur, bits, memory_order_relaxed,
                                                  memory_order_relaxed)) {
    }
}

void tw_sender_request_marker(tw_sender *s) {
    atomic_store_explicit(&s->marker_requested, true, memory_order_relaxed);
}

void tw_sender_read_marker(tw_sender *s, tw_marker *out) {
    out->seq = atomic_load_explicit(&s->marker_seq, memory_order_acquire);
    out->timestamp = atomic_load_explicit(&s->marker_timestamp, memory_order_relaxed);
    out->send_ns = atomic_load_explicit(&s->marker_send_ns, memory_order_relaxed);
    out->capture_ns = atomic_load_explicit(&s->marker_capture_ns, memory_order_relaxed);
}

static void send_packet(tw_sender *s, uint32_t frames, bool live, uint64_t nonce, uint32_t offset) {
    if (!live) {
        // Time still passes while gated, so the stream resumes at the right
        // timestamp; sequence numbers are only spent on packets actually sent.
        s->timestamp += frames;
        atomic_fetch_add_explicit(&s->packets_gated, 1, memory_order_relaxed);
        return;
    }
    uint8_t flags = s->discontinuity_pending ? 0x01 : 0x00;
    bool marker = atomic_load_explicit(&s->marker_requested, memory_order_relaxed);
    if (marker) flags |= 0x02;
    size_t len = tw_build_audio_packet(s->packet, s->config.key, nonce, s->config.stream_id, s->sequence,
                                       s->timestamp, s->config.rate_code, s->config.channels, flags, s->gather,
                                       frames * s->config.channels);
    atomic_store_explicit(&s->last_sequence, s->sequence, memory_order_relaxed);
    s->sequence++;
    s->timestamp += frames;
    uint64_t ts = s->timestamp - frames;
    if (send(s->config.fd, s->packet, len, 0) == (ssize_t)len) {
        s->discontinuity_pending = false;
        atomic_fetch_add_explicit(&s->packets_sent, 1, memory_order_relaxed);
        if (marker) {
            atomic_store_explicit(&s->marker_requested, false, memory_order_relaxed);
            atomic_store_explicit(&s->marker_timestamp, ts, memory_order_relaxed);
            atomic_store_explicit(&s->marker_send_ns, now_ns(s), memory_order_relaxed);
            uint64_t cap = s->buffer_host_ns ? s->buffer_host_ns + (uint64_t)(offset * s->ns_per_frame) : 0;
            atomic_store_explicit(&s->marker_capture_ns, cap, memory_order_relaxed);
            atomic_fetch_add_explicit(&s->marker_seq, 1, memory_order_release);
        }
    } else {
        // Keep the flag set until a packet actually leaves, so the receiver
        // still sees the stream start even if the first sends fail.
        atomic_fetch_add_explicit(&s->send_errors, 1, memory_order_relaxed);
        atomic_store_explicit(&s->last_errno, errno, memory_order_relaxed);
    }
}

// Copies one wire channel from a source of `stride` interleaved floats.
static inline float copy_channel(float *dst, uint32_t dst_stride, const float *src, uint32_t src_stride,
                                 uint32_t frames, float peak) {
    for (uint32_t i = 0; i < frames; i++) {
        float v = src[i * src_stride];
        dst[i * dst_stride] = v;
        float a = fabsf(v);
        if (a > peak) peak = a;
    }
    return peak;
}

void tw_sender_process_abl(tw_sender *s, const AudioBufferList *abl, uint32_t frames, double sample_time,
                           uint64_t host_time) {
    s->buffer_host_ns = host_time ? (uint64_t)((double)host_time * s->ns_per_tick) : 0;
    if (sample_time >= 0) {
        if (!s->have_first_sample_time) {
            s->first_sample_time = sample_time;
            s->have_first_sample_time = true;
        }
        // Device overloads skip IO cycles; carrying the device clock into the
        // timestamp turns them into gaps the receiver can fade over.
        double rel = sample_time - s->first_sample_time;
        if (rel > (double)s->timestamp) s->timestamp = (uint64_t)rel;
    }
    bool live = atomic_load_explicit(&s->live, memory_order_acquire);
    uint64_t nonce = atomic_load_explicit(&s->session_nonce, memory_order_relaxed);
    if (live && !s->was_live) s->discontinuity_pending = true;  // resync after a gated spell
    s->was_live = live;

    uint32_t wire_ch = s->config.channels;
    uint32_t fpp = s->config.frames_per_packet;
    float peak = 0;
    for (uint32_t done = 0; done < frames;) {
        uint32_t n = frames - done < fpp ? frames - done : fpp;
        memset(s->gather, 0, n * wire_ch * sizeof(float));
        // Map wire channel k to device channel (source_channel or k), walking
        // the per-stream buffers to find which buffer holds it.
        for (uint32_t k = 0; k < wire_ch; k++) {
            uint32_t want = s->config.source_channel >= 0 ? (uint32_t)s->config.source_channel : k;
            uint32_t base = 0;
            for (uint32_t b = 0; b < abl->mNumberBuffers; b++) {
                const AudioBuffer *buf = &abl->mBuffers[b];
                uint32_t nch = buf->mNumberChannels;
                if (want < base + nch) {
                    if (buf->mData && buf->mDataByteSize >= (done + n) * nch * sizeof(float)) {
                        const float *src = (const float *)buf->mData + done * nch + (want - base);
                        peak = copy_channel(s->gather + k, wire_ch, src, nch, n, peak);
                    }
                    break;
                }
                base += nch;
            }
        }
        send_packet(s, n, live, nonce, done);
        done += n;
    }
    raise_peak(s, peak);
    atomic_fetch_add_explicit(&s->frames, frames, memory_order_relaxed);
}

void tw_sender_process_interleaved(tw_sender *s, const float *data, uint32_t channels, uint32_t frames) {
    AudioBufferList abl;
    abl.mNumberBuffers = 1;
    abl.mBuffers[0].mNumberChannels = channels;
    abl.mBuffers[0].mDataByteSize = frames * channels * sizeof(float);
    abl.mBuffers[0].mData = (void *)data;
    tw_sender_process_abl(s, &abl, frames, -1.0, 0);
}

OSStatus tw_input_ioproc(AudioObjectID device, const AudioTimeStamp *now, const AudioBufferList *input,
                         const AudioTimeStamp *input_time, AudioBufferList *output,
                         const AudioTimeStamp *output_time, void *client_data) {
    (void)device;
    (void)now;
    (void)output;
    (void)output_time;
    if (!input || input->mNumberBuffers == 0 || !client_data) return noErr;
    const AudioBuffer *first = &input->mBuffers[0];
    if (first->mNumberChannels == 0) return noErr;
    uint32_t frames = first->mDataByteSize / (first->mNumberChannels * sizeof(float));
    double st = (input_time && (input_time->mFlags & kAudioTimeStampSampleTimeValid)) ? input_time->mSampleTime : -1.0;
    // inInputTime is when the buffer's first frame was captured (HAL docs).
    uint64_t ht = (input_time && (input_time->mFlags & kAudioTimeStampHostTimeValid)) ? input_time->mHostTime : 0;
    tw_sender_process_abl((tw_sender *)client_data, input, frames, st, ht);
    return noErr;
}
