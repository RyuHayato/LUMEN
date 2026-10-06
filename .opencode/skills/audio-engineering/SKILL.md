---
name: LUMEN Audio Engineering
description: Design, implement, review, and debug LUMEN's audio playback system with a focus on correctness, low latency, gapless playback, Windows WASAPI, device handling, and high-fidelity local playback.
---

# LUMEN Audio Engineering

You are working on LUMEN, a native-first desktop music player focused on high-quality local music playback.

Audio is a core product subsystem.

Treat audio engineering as a correctness-critical area, not ordinary UI/application code.

The goal is reliable, predictable, high-quality playback without unnecessary complexity.

---

## Core Principles

1. Audio correctness comes before UI convenience.
2. Never invent audio behavior.
3. Prefer well-maintained, established libraries and platform APIs.
4. Keep audio processing isolated from UI concerns.
5. Never block the audio callback/thread with filesystem, database, network, or expensive allocation work.
6. Avoid unnecessary allocations in real-time audio paths.
7. Avoid locks in real-time audio paths when a lock-free or message-passing design is practical.
8. Keep platform-specific audio code isolated behind clear interfaces.
9. Windows is the primary platform for LUMEN.
10. Do not pretend that "bit-perfect" playback exists unless the actual implementation and output path justify that claim.
11. Do not add DSP merely because it is technically possible.
12. Every audio-related architectural decision must have a concrete reason.

---

# Architecture

Maintain a clear separation between:

UI
↓
Playback/Application Commands
↓
Playback State
↓
Audio Engine
↓
Decoder / Resampler
↓
Output Backend
↓
Operating System / Audio Device

The UI must never directly manipulate low-level audio buffers or platform audio APIs.

The audio engine must not depend on UI implementation details.

---

# Playback State

Maintain explicit playback state.

At minimum consider:

- stopped
- loading
- playing
- paused
- seeking
- buffering
- finished
- error

Playback state must have one authoritative source.

Avoid duplicated playback state across UI components.

---

# Queue

The playback engine must support:

- current track
- next track
- previous track
- queue ordering
- queue modification
- repeat modes
- shuffle state
- deterministic next-track selection
- queue persistence when appropriate

Queue operations must not directly manipulate the audio output implementation.

---

# Decoding

When adding or modifying decoders:

- Verify the supported formats.
- Verify codec/library capabilities rather than assuming them.
- Handle malformed files safely.
- Handle unsupported codecs gracefully.
- Do not crash the application because one library file is corrupt.
- Preserve metadata separately from decoded audio data.
- Do not load entire large audio files into memory unnecessarily.

Potential formats include:

- FLAC
- WAV
- MP3
- AAC/M4A where supported
- OGG/Vorbis
- Opus
- other formats only when there is a concrete product reason

Do not expand format support simply to increase a feature list.

---

# Gapless Playback

Gapless playback is a real product requirement.

When implementing it:

- Avoid unnecessary teardown/recreation between consecutive tracks.
- Understand decoder delay and padding where applicable.
- Preserve exact track boundaries where the format permits.
- Test consecutive tracks that are designed to play continuously.
- Test ordinary tracks as well.
- Do not claim gapless playback merely because tracks are queued automatically.

---

# Seeking

Seeking must be predictable.

Consider:

- accurate seek position
- decoder seek granularity
- buffering after seek
- cancellation of stale seek requests
- rapid repeated seeking
- seeking while paused
- seeking while playing
- seeking near the end of a track

Avoid race conditions between old decode operations and new seek requests.

---

# ReplayGain

ReplayGain must be implemented as a controlled playback transformation.

Support the appropriate metadata modes when justified.

Keep:

- track gain
- album gain
- preamp/settings

conceptually separate.

Do not permanently modify source files.

Do not silently alter files.

When metadata is missing, playback should continue normally.

---

# Sample Rate / Bit Depth

LUMEN should expose meaningful technical information.

Do not fabricate values.

Distinguish between:

- source file format
- decoded PCM format
- resampled format
- output device format

If resampling occurs, make that fact architecturally visible.

If Windows shared mode causes the system mixer to alter the output format, do not describe the output as identical to the source merely because the source file has a particular sample rate.

---

# Windows WASAPI

Windows is the first-class target.

When implementing WASAPI:

Understand the distinction between:

- shared mode
- exclusive mode

Consider:

- device enumeration
- default device changes
- device disconnection
- device reconnection
- unsupported sample rates
- exclusive-mode failures
- device initialization failures
- format negotiation
- buffer sizing
- latency
- stream lifecycle

Do not expose "exclusive mode" as a checkbox without implementing the actual behavior.

Do not claim bit-perfect playback without validating the complete output path.

Platform-specific code should remain isolated.

For example:

src/audio/
    engine/
    decoder/
    output/
        mod.rs
        windows/
            wasapi.rs

The exact structure may differ if the architecture has a better justified design.

---

# Threading

Audio processing must be isolated from expensive application work.

Never perform these operations directly in a real-time audio callback:

- database queries
- filesystem operations
- network requests
- metadata extraction
- artwork extraction
- large allocations
- blocking mutex acquisition
- UI operations
- logging that may block

Use appropriate channels, queues, ring buffers, worker threads, or other mechanisms where necessary.

Do not introduce concurrency complexity unless the problem actually requires it.

---

# Device Changes

The application must eventually handle:

- headphones unplugged
- USB DAC disconnected
- Bluetooth device disconnected
- default Windows output changed
- device becoming unavailable
- device returning
- device format becoming unavailable

Playback should fail gracefully.

Never allow a device failure to crash the entire application.

---

# Errors

Audio errors must be explicit and actionable.

Examples:

- unsupported format
- decoder failure
- file disappeared
- output device unavailable
- WASAPI initialization failure
- exclusive mode unavailable
- unsupported sample rate
- device disconnected
- corrupted audio data

Do not swallow errors silently.

Do not expose raw low-level errors directly to users when a clearer explanation is possible.

Preserve detailed technical diagnostics for logs.

---

# Performance

When optimizing audio:

Measure before optimizing.

Prioritize:

1. glitch-free playback
2. predictable latency
3. low CPU usage
4. reasonable memory usage
5. large-library responsiveness

Do not optimize UI rendering at the expense of audio stability.

Audio glitches are higher priority than minor UI frame drops.

---

# Testing

Audio changes should include tests appropriate to the subsystem.

Where practical test:

- decoder initialization
- supported formats
- malformed files
- missing files
- seeking
- pause/resume
- queue transitions
- end-of-track handling
- gapless transitions
- replay gain
- output-device errors
- device disconnection
- repeated play/pause
- repeated seek
- rapid track changes

Separate deterministic unit tests from hardware-dependent integration tests.

Never pretend hardware-dependent behavior was tested if the environment cannot test it.

---

# Diagnostics

LUMEN should eventually expose useful diagnostics.

Useful information may include:

- current track
- codec
- container
- sample rate
- bit depth
- channel count
- decoded format
- output device
- output mode
- shared/exclusive state
- resampling status
- playback position
- buffer/underrun information where available

Diagnostics are for understanding the actual playback pipeline.

Do not display meaningless technical numbers merely to make the product look "audiophile."

---

# Code Review Rules

When reviewing audio code, actively look for:

- blocking operations in audio paths
- unnecessary allocations
- lock contention
- race conditions
- stale seek operations
- device lifecycle bugs
- resource leaks
- incorrect sample-rate assumptions
- incorrect channel assumptions
- incorrect format conversions
- gapless playback regressions
- swallowed errors
- platform-specific code leaking into generic modules
- misleading "bit-perfect" claims
- unnecessary abstractions
- unnecessary dependencies

If you find a real problem, fix it rather than merely documenting it when the current task permits the fix.

---

# Dependency Rules

Before adding an audio dependency:

1. Determine whether the project already has an appropriate dependency.
2. Verify that the proposed dependency actually solves the problem.
3. Prefer mature and actively maintained libraries.
4. Avoid adding multiple libraries that overlap substantially.
5. Consider Windows compatibility.
6. Consider licensing.
7. Consider future macOS/Linux compatibility where practical.
8. Document important dependency decisions.

Do not introduce a dependency merely because it is convenient for a small helper.

---

# Platform Abstraction

Generic application code should not contain Windows-specific audio implementation details.

Prefer an interface similar in spirit to:

AudioEngine
    ↓
OutputBackend
    ↓
Windows WASAPI

Future implementations may include:

AudioEngine
    ↓
OutputBackend
    ├── Windows
    ├── macOS
    └── Linux

Do not implement future platforms until they are actually required.

The abstraction should exist to isolate platform concerns, not to create speculative code.

---

# Engineering Decision Rule

When multiple technically valid approaches exist:

Prefer the solution that is:

- correct
- understandable
- maintainable
- testable
- performant enough
- compatible with the current product goals

Do not choose the most complicated solution simply because it is more sophisticated.

---

# Important Restrictions

Never:

- copy proprietary implementation from another music player
- reproduce proprietary code
- depend on undocumented/private service APIs without explicit authorization
- bypass licensing restrictions
- claim audio behavior that has not been verified
- add cloud services without a concrete product requirement
- introduce unnecessary DSP
- sacrifice playback reliability for visual effects

LUMEN is an original product.

---

# When This Skill Should Be Used

Use this skill whenever a task involves:

- audio playback
- audio decoding
- audio formats
- playback queue
- gapless playback
- seeking
- ReplayGain
- sample rates
- bit depth
- resampling
- output devices
- WASAPI
- exclusive mode
- audio buffering
- audio threading
- playback errors
- audio diagnostics
- audio performance
- device disconnection/reconnection
- reviewing audio-related architecture or code

For unrelated UI, database, documentation, or general application tasks, do not unnecessarily load this skill.