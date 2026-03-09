# DAVE NoSenderKey Fix Plan

## Issues Identified:
1. MLS-Exporter Label Mismatch (v1 vs v0)
2. PrepareEpoch only exports keys at epoch 1
3. Missing error propagation in key export
4. No readiness check before mixing

## Fixes to Implement:

### 1. Fix MLS-Exporter Label (types.rs)
- [x] Change `SENDER_KEY_LABEL` from `b"Discord Secure Frames v1"` to `b"Discord Secure Frames v0"`
- [x] Updated comments in session.rs and group.rs

### 2. Improve PrepareEpoch Handling (driver.rs)
- [x] Export sender keys for ALL epochs, not just epoch 1
- [x] Add proper error handling for key export failures
- [x] Log when keys are successfully exported

### 3. Add Readiness Check (session.rs)
- [x] Add `has_own_key()` method to check if own key is available

### 4. Add Debugging
- [x] Log DAVE handshake events more comprehensively
- [x] Track when group is created/joined
- [x] Track when keys are exported
- [x] Added error handling for MlsExternalSender, MlsWelcome, MlsProposals, MlsAnnounceCommitTransition

## Status: COMPLETED

## Files Modified:
1. sigil-discord/src/types.rs - Fixed MLS-Exporter label
2. sigil-discord/src/session.rs - Added has_own_key() method
3. sigil-discord/src/mls/group.rs - Updated comments
4. sigil-voice/src/driver.rs - Improved all DAVE event handlers

