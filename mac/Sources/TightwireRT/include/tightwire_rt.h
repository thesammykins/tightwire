#ifndef TIGHTWIRE_RT_H
#define TIGHTWIRE_RT_H

#include <CoreAudio/CoreAudio.h>
#include <stdbool.h>
#include <stdint.h>

#pragma clang assume_nonnull begin

#define TW_AUDIO_HEADER_LEN 24
#define TW_MAX_PAYLOAD 1400
#define TW_MAX_CHANNELS 8
#define TW_TAG_LEN 8
#define TW_PROTOCOL_VERSION 2

/// SipHash-2-4 (64-bit output, written little-endian to `out`) over
/// prefix ‖ data. `prefix` may be NULL. Allocation-free; safe on the audio thread.
void tw_siphash24(const uint8_t *key, const uint8_t *_Nullable prefix, size_t prefix_len, const uint8_t *_Nullable data,
                  size_t len, uint8_t *out);

/// Real-time packetizer state. Created and destroyed off the audio thread; the
/// process functions touch only this struct, the socket and atomics.
typedef struct tw_sender tw_sender;

typedef struct {
    int fd;                      ///< connected, non-blocking UDP socket (not owned)
    uint16_t stream_id;
    uint8_t rate_code;           ///< SPEC §3 sample-rate code
    uint8_t channels;            ///< channels on the wire (1-8)
    int32_t source_channel;      ///< -1: send device channels 0..channels-1; else mono from this channel
    uint32_t frames_per_packet;  ///< upper bound; clamped so the payload fits TW_MAX_PAYLOAD
    uint8_t key[16];             ///< SipHash key, Mac → receiver direction
} tw_sender_config;

typedef struct {
    uint64_t packets_sent;
    uint64_t packets_gated;      ///< packets not sent because the receiver was not live
    uint64_t send_errors;
    uint64_t frames;
    int32_t last_errno;
    float peak;                  ///< max |sample| since the previous read (reset on read)
} tw_sender_counters;

tw_sender *_Nullable tw_sender_create(const tw_sender_config *config);
void tw_sender_destroy(tw_sender *sender);
uint32_t tw_sender_frames_per_packet(const tw_sender *sender);
void tw_sender_read_counters(tw_sender *sender, tw_sender_counters *out);

/// Liveness gate (SPEC §3a): audio leaves only while open. `session_nonce` is
/// the receiver's current nonce, mixed into every tag.
void tw_sender_set_live(tw_sender *sender, bool live, uint64_t session_nonce);

/// Sequence number of the most recently built packet (for stats freshness).
uint32_t tw_sender_last_sequence(const tw_sender *sender);

/// Packetizes `frames` frames from a CoreAudio buffer list (each buffer holds
/// interleaved float32 for its own channels). `sample_time` < 0 means unknown;
/// otherwise jumps in device sample time become timestamp gaps. `host_time`
/// is the mach_absolute_time at which the buffer's first frame was captured
/// (0 = unknown); it only feeds latency markers.
void tw_sender_process_abl(tw_sender *sender, const AudioBufferList *abl, uint32_t frames, double sample_time,
                           uint64_t host_time);

/// Packetizes interleaved float32 frames with `channels` channels (synthetic sources).
void tw_sender_process_interleaved(tw_sender *sender, const float *data, uint32_t channels, uint32_t frames);

/// Latency marker (SPEC §3, flag 0x02): the next packet sent carries the
/// marker flag, and its timestamp and times are published for `read_marker`.
typedef struct {
    uint32_t seq;            ///< increments per marker sent; 0 = none yet
    uint64_t timestamp;      ///< stream timestamp of the marked packet's first frame
    uint64_t send_ns;        ///< CLOCK_UPTIME_RAW ns when the packet was sent
    uint64_t capture_ns;     ///< CLOCK_UPTIME_RAW ns its first frame was captured (0 = unknown)
} tw_marker;

void tw_sender_request_marker(tw_sender *sender);
void tw_sender_read_marker(tw_sender *sender, tw_marker *out);

/// AudioDeviceIOProc that forwards input to the tw_sender passed as client data.
OSStatus tw_input_ioproc(AudioObjectID device, const AudioTimeStamp *now, const AudioBufferList *input,
                         const AudioTimeStamp *input_time, AudioBufferList *output,
                         const AudioTimeStamp *output_time, void *_Nullable client_data);

/// Builds one tagged audio packet into `out` (capacity >= TW_AUDIO_HEADER_LEN +
/// TW_MAX_PAYLOAD + TW_TAG_LEN) and returns its length. Exposed so tests can
/// compare against the golden vectors.
size_t tw_build_audio_packet(uint8_t *out, const uint8_t *key, uint64_t session_nonce, uint16_t stream_id,
                             uint32_t sequence, uint64_t timestamp, uint8_t rate_code, uint8_t channels,
                             uint8_t flags, const float *samples, uint32_t sample_count);

#pragma clang assume_nonnull end

#endif
