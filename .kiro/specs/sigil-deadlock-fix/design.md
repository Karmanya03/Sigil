# Sigil Deadlock Fix Design

## Overview

The Discord voice implementation suffers from a critical deadlock in the `start_mixing()` function. The function acquires and holds the sigil lock while polling `is_established()` every 100ms for up to 3 seconds, waiting for the MLS group to be established. However, the WebSocket background task needs this same lock to process DAVE opcodes (OP 25, 26, 28, 29) that would establish the MLS group, creating a circular dependency. Additionally, there's a race condition where `is_established()` returns true when the group exists, but `has_own_key()` may still be false because key export hasn't completed yet.

The fix involves refactoring the lock acquisition pattern to release the lock between polling attempts, allowing the WebSocket task to acquire it and process DAVE opcodes. We'll also ensure proper checking of both group establishment AND key availability before proceeding with the mixing loop.

## Glossary

- **Bug_Condition (C)**: The condition that triggers the deadlock - when `start_mixing()` holds the sigil lock while waiting for MLS establishment, blocking the WebSocket task from processing DAVE opcodes
- **Property (P)**: The desired behavior - `start_mixing()` should wait for MLS establishment without holding the lock, allowing the WebSocket task to process DAVE opcodes concurrently
- **Preservation**: Existing audio mixing, Opus encoding, DAVE encryption, and UDP transmission behavior that must remain unchanged by the fix
- **sigil**: An `Arc<Mutex<SigilSession>>` that manages MLS group state and DAVE encryption operations
- **start_mixing()**: The function in `sigil-voice/src/driver.rs` that initializes the audio mixing loop and waits for MLS establishment
- **WebSocket background task**: The tokio task spawned in `connect()` that handles heartbeats and processes DAVE opcodes from Discord's voice gateway
- **is_established()**: Method that returns true when the MLS group exists (but keys may not be exported yet)
- **has_own_key()**: Method that returns true when the own sender key has been exported and cached
- **DAVE opcodes**: Binary WebSocket messages (OP 25, 26, 28, 29) that establish and manage the MLS group for end-to-end encryption

## Bug Details

### Fault Condition

The deadlock manifests when `start_mixing()` is called and attempts to wait for MLS group establishment. The function acquires the sigil lock at the start of the wait loop and holds it across async sleep points, preventing the WebSocket task from acquiring the lock to process DAVE opcodes that would establish the group.

**Formal Specification:**
```
FUNCTION isBugCondition(execution_state)
  INPUT: execution_state containing lock_holder, waiting_tasks, mls_group_state
  OUTPUT: boolean
  
  RETURN execution_state.lock_holder == "start_mixing"
         AND execution_state.sigil_lock_held_duration > 0
         AND "websocket_task" IN execution_state.waiting_tasks
         AND execution_state.mls_group_state == "not_established"
         AND execution_state.websocket_task_has_pending_dave_opcodes == true
END FUNCTION
```

### Examples

- **Deadlock Scenario**: User calls `start_mixing()` → function acquires sigil lock → polls `is_established()` every 100ms → WebSocket receives OP 25 (External Sender) → WebSocket task blocks on `sigil_clone.lock().await` → MLS group never established → timeout after 3 seconds with "MLS group never established" error

- **Race Condition Scenario**: WebSocket processes OP 25 and creates group → `is_established()` returns true → `start_mixing()` proceeds → attempts to export keys but `has_own_key()` is still false → falls back to raw Opus frames instead of DAVE encryption

- **Lock Re-acquisition Issue**: Inside the wait loop, the code does `sigil_guard = self.sigil.lock().await` on each iteration, creating multiple lock acquisition points while already holding the lock across async sleep, exacerbating the deadlock

- **Expected Behavior After Fix**: `start_mixing()` releases lock between polls → WebSocket acquires lock → processes OP 25 → establishes group and exports keys → `start_mixing()` detects both `is_established()` and `has_own_key()` → proceeds with DAVE-encrypted mixing loop

## Expected Behavior

### Preservation Requirements

**Unchanged Behaviors:**
- Audio mixing from multiple PCM tracks must continue to work exactly as before
- Opus encoding of mixed PCM audio must produce valid Opus frames
- DAVE encryption using `encrypt_own_frame()` must continue to work when keys are available
- Fallback to raw Opus frames when DAVE is not ready must continue to work
- Transport encryption (AES-256-GCM) and UDP transmission must remain unchanged
- WebSocket heartbeat handling and non-DAVE opcode processing must remain unchanged
- Speaking events (OP 5) and SSRC mapping must continue to work

**Scope:**
All inputs and execution paths that do NOT involve the `start_mixing()` wait loop should be completely unaffected by this fix. This includes:
- The main mixing loop after MLS establishment (PCM mixing, Opus encoding, DAVE encryption, UDP send)
- WebSocket task handling of heartbeats, speaking events, and other non-DAVE opcodes
- DAVE opcode processing logic (OP 25, 26, 28, 29) - only the lock acquisition timing changes
- Audio track management (adding, removing, pausing, resuming tracks)

## Hypothesized Root Cause

Based on the bug description and code analysis, the root causes are:

1. **Lock Held Across Async Sleep**: The `start_mixing()` function acquires the sigil lock and then calls `tokio::time::sleep()` while holding it. This blocks the WebSocket task from acquiring the lock to process DAVE opcodes during the 100ms sleep intervals.

2. **Lock Re-acquisition Inside Loop**: The code does `sigil_guard = self.sigil.lock().await` on each retry iteration, which is redundant since the lock is already held, but more importantly, it holds the lock across the sleep point between iterations.

3. **Insufficient Readiness Check**: The code only checks `is_established()` (group exists) but not `has_own_key()` (keys exported), leading to a race condition where the mixing loop starts before keys are available.

4. **Circular Dependency**: `start_mixing()` waits for MLS establishment while holding the lock → WebSocket task needs the lock to process DAVE opcodes → DAVE opcodes establish the MLS group → deadlock.

## Correctness Properties

Property 1: Fault Condition - Non-Blocking MLS Wait

_For any_ execution where `start_mixing()` is called before the MLS group is established, the fixed function SHALL release the sigil lock between polling attempts, allowing the WebSocket task to acquire the lock and process DAVE opcodes that establish the MLS group and export sender keys.

**Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.6**

Property 2: Preservation - Audio Pipeline Behavior

_For any_ execution where the MLS group is already established and keys are available, the fixed code SHALL produce exactly the same audio mixing, encoding, encryption, and transmission behavior as the original code, preserving all existing functionality for the audio pipeline.

**Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7**

## Fix Implementation

### Changes Required

Assuming our root cause analysis is correct:

**File**: `sigil-voice/src/driver.rs`

**Function**: `start_mixing()`

**Specific Changes**:

1. **Release Lock Between Polls**: Refactor the wait loop to acquire the lock only for the duration of the readiness check, then release it before sleeping. This allows the WebSocket task to acquire the lock during the sleep period.
   - Change from: `let mut sigil_guard = self.sigil.lock().await; while !sigil_guard.is_established() { sleep; sigil_guard = self.sigil.lock().await; }`
   - Change to: `while { let guard = self.sigil.lock().await; !guard.is_established() || !guard.has_own_key() } { sleep; }`

2. **Check Both Conditions**: Update the readiness check to verify both `is_established()` AND `has_own_key()` to ensure the MLS group is fully ready with exported keys before proceeding.
   - Add: `!guard.has_own_key()` to the while condition

3. **Remove Redundant Lock Acquisition**: Remove the `sigil_guard = self.sigil.lock().await` line inside the loop since we're now acquiring the lock fresh on each iteration within the while condition.

4. **Remove Redundant Key Export**: After the wait loop succeeds, we no longer need to call `export_sender_keys()` again since the WebSocket task already exported keys when processing DAVE opcodes. However, we should keep a verification check to ensure keys are present.
   - Keep the verification logic but remove the redundant export call

5. **Update Error Messages**: Update the timeout error message to reflect that we're waiting for both group establishment AND key export.
   - Change: "MLS group never established" → "MLS group not ready (group not established or keys not exported)"

### Pseudocode for Fixed Logic

```rust
pub async fn start_mixing(&self) -> Result<...> {
    // Wait for MLS group to be established AND keys to be exported
    let mut retries = 0;
    while retries < 30 {
        {
            let guard = self.sigil.lock().await;
            if guard.is_established() && guard.has_own_key() {
                info!("MLS group ready (established + keys exported)");
                break;
            }
        } // Lock released here
        
        info!("Waiting for MLS group (attempt {}/30)...", retries + 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        retries += 1;
    }

    // Verify readiness after loop
    {
        let guard = self.sigil.lock().await;
        if !guard.is_established() || !guard.has_own_key() {
            error!("MLS group not ready — cannot start mixing loop");
            return Err("MLS group not ready".into());
        }
    }

    // Continue with existing mixing loop logic...
}
```

## Testing Strategy

### Validation Approach

The testing strategy follows a two-phase approach: first, surface counterexamples that demonstrate the deadlock on unfixed code, then verify the fix works correctly and preserves existing behavior.

### Exploratory Fault Condition Checking

**Goal**: Surface counterexamples that demonstrate the deadlock BEFORE implementing the fix. Confirm or refute the root cause analysis. If we refute, we will need to re-hypothesize.

**Test Plan**: Write tests that simulate the race condition between `start_mixing()` and the WebSocket task processing DAVE opcodes. Use instrumentation to detect lock contention and measure lock hold duration. Run these tests on the UNFIXED code to observe deadlock and timeout failures.

**Test Cases**:
1. **Deadlock Detection Test**: Spawn `start_mixing()` and simulate WebSocket receiving OP 25 during the wait period. Assert that the WebSocket task blocks on lock acquisition and the timeout occurs (will fail on unfixed code with "MLS group never established" after 3 seconds).

2. **Lock Hold Duration Test**: Measure how long `start_mixing()` holds the sigil lock during the wait loop. Assert that it holds the lock for the entire 3-second timeout period (will fail on unfixed code, showing lock held for ~3000ms instead of brief acquisitions).

3. **Race Condition Test**: Simulate WebSocket processing OP 25 (group creation) but delay key export. Assert that `start_mixing()` proceeds even though `has_own_key()` is false (will fail on unfixed code, showing fallback to raw Opus instead of DAVE encryption).

4. **Concurrent Access Test**: Spawn multiple tasks that need the sigil lock (start_mixing, WebSocket DAVE processing, manual encryption calls). Assert that all tasks make progress without blocking (will fail on unfixed code with some tasks timing out).

**Expected Counterexamples**:
- `start_mixing()` times out after 3 seconds with "MLS group never established"
- WebSocket task logs show DAVE opcodes received but not processed during the timeout period
- Lock contention metrics show WebSocket task waiting for lock held by `start_mixing()`
- Possible causes: lock held across async sleep, lock not released between polls, insufficient readiness check

### Fix Checking

**Goal**: Verify that for all inputs where the bug condition holds, the fixed function produces the expected behavior.

**Pseudocode:**
```
FOR ALL execution_state WHERE isBugCondition(execution_state) DO
  result := start_mixing_fixed(execution_state)
  ASSERT result.websocket_task_processed_dave_opcodes == true
  ASSERT result.mls_group_established == true
  ASSERT result.own_key_exported == true
  ASSERT result.mixing_loop_started == true
  ASSERT result.lock_hold_duration_per_poll < 10ms
END FOR
```

**Test Cases**:
1. **Non-Blocking Wait Test**: Spawn `start_mixing()` and simulate WebSocket receiving OP 25. Assert that WebSocket acquires lock within 100ms and processes the opcode, and `start_mixing()` proceeds after group establishment.

2. **Key Availability Test**: Verify that `start_mixing()` waits until both `is_established()` and `has_own_key()` are true before proceeding. Assert that DAVE encryption is used immediately without fallback.

3. **Lock Release Test**: Instrument lock acquisition/release and verify that the lock is released between each polling attempt. Assert that lock hold duration per poll is < 10ms.

### Preservation Checking

**Goal**: Verify that for all inputs where the bug condition does NOT hold, the fixed function produces the same result as the original function.

**Pseudocode:**
```
FOR ALL execution_state WHERE NOT isBugCondition(execution_state) DO
  ASSERT start_mixing_original(execution_state) = start_mixing_fixed(execution_state)
END FOR
```

**Testing Approach**: Property-based testing is recommended for preservation checking because:
- It generates many test cases automatically across the input domain (different timing scenarios, different DAVE opcode sequences)
- It catches edge cases that manual unit tests might miss (e.g., group already established before `start_mixing()` is called)
- It provides strong guarantees that behavior is unchanged for all non-buggy inputs

**Test Plan**: Observe behavior on UNFIXED code first for scenarios where MLS is already established, then write property-based tests capturing that behavior.

**Test Cases**:
1. **Pre-Established Group Preservation**: Start with MLS group already established and keys exported. Call `start_mixing()` and verify it proceeds immediately without waiting. Compare audio output between original and fixed versions.

2. **Audio Pipeline Preservation**: Run the full mixing loop (PCM mixing, Opus encoding, DAVE encryption, UDP send) and verify byte-for-byte identical output for the same input audio tracks.

3. **WebSocket Non-DAVE Preservation**: Process heartbeats, speaking events, and other non-DAVE opcodes. Verify identical behavior between original and fixed versions.

4. **Fallback Behavior Preservation**: Simulate scenarios where DAVE encryption fails. Verify that fallback to raw Opus frames works identically in both versions.

### Unit Tests

- Test lock acquisition and release pattern in the wait loop (verify lock is released between polls)
- Test readiness check with various combinations of `is_established()` and `has_own_key()` states
- Test timeout behavior when MLS group never becomes ready
- Test immediate proceed when group is already established before `start_mixing()` is called
- Test error handling when key export fails

### Property-Based Tests

- Generate random timing scenarios (DAVE opcodes arriving at different times during the wait loop) and verify no deadlock occurs
- Generate random sequences of DAVE opcodes and verify MLS group establishment always succeeds
- Generate random audio track configurations and verify mixing output is identical between original and fixed versions
- Test that lock hold duration is always < 10ms per poll across many random scenarios

### Integration Tests

- Test full connection flow: connect → receive DAVE opcodes → establish group → start mixing → send encrypted audio
- Test concurrent operations: multiple tracks playing, WebSocket processing opcodes, mixing loop running
- Test recovery scenarios: connection drops and reconnects, MLS group re-establishment
- Test visual feedback: verify log messages show correct progression (group established, keys exported, mixing started)
