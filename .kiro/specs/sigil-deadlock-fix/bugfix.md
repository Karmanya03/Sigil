# Bugfix Requirements Document

## Introduction

The Discord voice implementation has a critical deadlock in the `start_mixing()` function that prevents audio from reaching UDP. The deadlock occurs due to lock contention between the mixing loop initialization and the WebSocket gateway task that processes DAVE protocol opcodes. The `start_mixing()` function holds the sigil lock while waiting for MLS group establishment, but the WebSocket task needs this same lock to process the DAVE opcodes that would establish the MLS group, creating a circular dependency. Additionally, there is a race condition where `is_established()` returns true when the group exists, but `has_own_key()` may still be false because key export hasn't completed yet.

## Bug Analysis

### Current Behavior (Defect)

1.1 WHEN `start_mixing()` is called THEN the system acquires the sigil lock and holds it while polling `is_established()` every 100ms for up to 3 seconds

1.2 WHEN the WebSocket gateway task receives DAVE opcodes (OP 25, 26, 28, 29) during the 3-second wait THEN the system blocks indefinitely waiting for the sigil lock that `start_mixing()` is holding

1.3 WHEN the WebSocket task is blocked THEN the system never processes the DAVE opcodes required to establish the MLS group

1.4 WHEN the MLS group is never established THEN the system times out after 3 seconds with "MLS group never established" error and the mixing loop never starts

1.5 WHEN `is_established()` returns true but `has_own_key()` is still false THEN the system falls back to sending raw Opus frames instead of DAVE-encrypted frames

1.6 WHEN the sigil lock is re-acquired on each retry iteration inside the wait loop THEN the system creates multiple opportunities for deadlock by repeatedly locking and unlocking while holding the lock across async sleep points

### Expected Behavior (Correct)

2.1 WHEN `start_mixing()` waits for MLS establishment THEN the system SHALL NOT hold the sigil lock during the wait period

2.2 WHEN the WebSocket gateway task receives DAVE opcodes THEN the system SHALL be able to acquire the sigil lock immediately to process them

2.3 WHEN DAVE opcodes are processed by the WebSocket task THEN the system SHALL establish the MLS group and export sender keys without blocking

2.4 WHEN the MLS group is established and keys are exported THEN the system SHALL allow `start_mixing()` to proceed with the mixing loop

2.5 WHEN checking for MLS readiness THEN the system SHALL verify both that the group exists AND that own sender keys have been exported

2.6 WHEN waiting for MLS establishment THEN the system SHALL release the lock between polling attempts to allow the WebSocket task to make progress

### Unchanged Behavior (Regression Prevention)

3.1 WHEN the mixing loop is running and audio is active THEN the system SHALL CONTINUE TO mix PCM audio from all active tracks

3.2 WHEN the mixing loop encodes audio to Opus THEN the system SHALL CONTINUE TO produce valid Opus frames

3.3 WHEN DAVE encryption is available (group established and own key present) THEN the system SHALL CONTINUE TO encrypt audio frames using `encrypt_own_frame()`

3.4 WHEN DAVE encryption is not yet available THEN the system SHALL CONTINUE TO fall back to sending raw Opus frames

3.5 WHEN audio frames are ready for transmission THEN the system SHALL CONTINUE TO apply transport encryption (AES-256-GCM) and send via UDP

3.6 WHEN the WebSocket task processes non-DAVE opcodes (heartbeat, speaking events) THEN the system SHALL CONTINUE TO handle them correctly

3.7 WHEN `export_sender_keys()` is called after MLS group operations THEN the system SHALL CONTINUE TO cache the own sender key for encryption
