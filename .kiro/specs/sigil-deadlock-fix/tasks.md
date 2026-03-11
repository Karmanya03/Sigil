# Implementation Plan

- [ ] 1. Write bug condition exploration test
  - **Property 1: Fault Condition** - Non-Blocking MLS Wait
  - **CRITICAL**: This test MUST FAIL on unfixed code - failure confirms the bug exists
  - **DO NOT attempt to fix the test or the code when it fails**
  - **NOTE**: This test encodes the expected behavior - it will validate the fix when it passes after implementation
  - **GOAL**: Surface counterexamples that demonstrate the deadlock exists
  - **Scoped PBT Approach**: Scope the property to concrete failing cases: `start_mixing()` called before MLS group establishment with WebSocket receiving DAVE opcodes during wait
  - Test that when `start_mixing()` is called and WebSocket receives OP 25 during the wait period, the WebSocket task can acquire the lock within 100ms and process the opcode
  - Test that lock hold duration per poll is < 10ms (not ~3000ms)
  - Test that MLS group becomes established and keys are exported without timeout
  - Run test on UNFIXED code
  - **EXPECTED OUTCOME**: Test FAILS (deadlock occurs, timeout after 3 seconds, or lock held for entire wait period)
  - Document counterexamples found:
    - `start_mixing()` times out with "MLS group never established"
    - WebSocket task blocks waiting for sigil lock
    - Lock contention metrics show lock held for entire 3-second period
  - Mark task complete when test is written, run, and failure is documented
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.6_

- [ ] 2. Write preservation property tests (BEFORE implementing fix)
  - **Property 2: Preservation** - Audio Pipeline Behavior
  - **IMPORTANT**: Follow observation-first methodology
  - Observe behavior on UNFIXED code for non-buggy inputs (MLS already established before `start_mixing()` called)
  - Write property-based tests capturing observed behavior patterns:
    - When MLS group is pre-established, `start_mixing()` proceeds immediately
    - Audio mixing produces identical PCM output for same input tracks
    - Opus encoding produces identical frames for same PCM input
    - DAVE encryption produces valid encrypted frames when keys available
    - Fallback to raw Opus works when DAVE not ready
    - WebSocket processes heartbeats and non-DAVE opcodes identically
  - Property-based testing generates many test cases for stronger guarantees
  - Run tests on UNFIXED code
  - **EXPECTED OUTCOME**: Tests PASS (confirms baseline behavior to preserve)
  - Mark task complete when tests are written, run, and passing on unfixed code
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7_

- [x] 3. Fix for sigil deadlock in start_mixing()

  - [x] 3.1 Refactor wait loop to release lock between polls
    - Change wait loop structure to acquire lock only for readiness check
    - Release lock before sleeping to allow WebSocket task to acquire it
    - Pattern: `while { let guard = self.sigil.lock().await; !guard.is_established() || !guard.has_own_key() } { sleep; }`
    - Remove redundant `sigil_guard = self.sigil.lock().await` inside loop
    - _Bug_Condition: isBugCondition(execution_state) where lock_holder == "start_mixing" AND sigil_lock_held_duration > 0 AND "websocket_task" IN waiting_tasks AND mls_group_state == "not_established" AND websocket_task_has_pending_dave_opcodes == true_
    - _Expected_Behavior: Lock released between polls, WebSocket can acquire lock within 100ms, lock hold duration per poll < 10ms_
    - _Preservation: Audio mixing, Opus encoding, DAVE encryption, UDP transmission, WebSocket non-DAVE handling remain unchanged_
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.6, 2.1, 2.2, 2.6_

  - [x] 3.2 Update readiness check to verify both conditions
    - Add `has_own_key()` check to while condition alongside `is_established()`
    - Ensure both group establishment AND key export are verified before proceeding
    - Update info log message: "MLS group ready (established + keys exported)"
    - _Bug_Condition: Race condition where is_established() returns true but has_own_key() is false_
    - _Expected_Behavior: start_mixing() waits until both is_established() AND has_own_key() are true_
    - _Preservation: Existing DAVE encryption and fallback logic unchanged_
    - _Requirements: 1.5, 2.3, 2.4, 2.5_

  - [x] 3.3 Update error messages and verification
    - Update timeout error message: "MLS group not ready (group not established or keys not exported)"
    - Update verification check after loop to test both conditions
    - Update log messages to reflect both conditions being checked
    - _Expected_Behavior: Clear error messages indicating which condition failed_
    - _Preservation: Error handling behavior unchanged, only message clarity improved_
    - _Requirements: 2.4, 2.5_

  - [x] 3.4 Verify bug condition exploration test now passes
    - **Property 1: Expected Behavior** - Non-Blocking MLS Wait
    - **IMPORTANT**: Re-run the SAME test from task 1 - do NOT write a new test
    - The test from task 1 encodes the expected behavior
    - When this test passes, it confirms the expected behavior is satisfied
    - Run bug condition exploration test from step 1
    - **EXPECTED OUTCOME**: Test PASSES (confirms deadlock is fixed)
    - Verify WebSocket task acquires lock within 100ms
    - Verify lock hold duration per poll is < 10ms
    - Verify MLS group establishment completes without timeout
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.6_

  - [x] 3.5 Verify preservation tests still pass
    - **Property 2: Preservation** - Audio Pipeline Behavior
    - **IMPORTANT**: Re-run the SAME tests from task 2 - do NOT write new tests
    - Run preservation property tests from step 2
    - **EXPECTED OUTCOME**: Tests PASS (confirms no regressions)
    - Confirm all tests still pass after fix (no regressions)
    - Verify audio mixing, encoding, encryption, and transmission unchanged
    - Verify WebSocket non-DAVE opcode handling unchanged
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7_

- [x] 4. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise
  - Verify no deadlock occurs in any scenario
  - Verify lock hold duration is minimal (< 10ms per poll)
  - Verify audio pipeline behavior is preserved
  - Verify WebSocket task can process DAVE opcodes concurrently with start_mixing() wait
