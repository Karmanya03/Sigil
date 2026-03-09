# Part B: Fix Binary Dispatch (for Proper DAVE) - Implementation Plan

## Information Gathered

### Files Analyzed:
1. **sigil-discord/src/gateway/handler.rs** - Contains the opcode dispatch logic
2. **sigil-discord/src/gateway/opcodes.rs** - Contains opcode definitions and payload structs
3. **sigil-voice/src/driver.rs** - Contains the driver that handles DAVE events and sends messages
4. **sigil-voice/src/gateway.rs** - Contains Speaking struct definition

### Current Issues Identified:

#### High Priority (Must Fix):
1. **OP 24 (PrepareEpoch)**: Currently uses JSON deserialization, needs binary parsing
   - Format: `[seq(2)][op(1)][epoch(4 bytes LE)]`
   - Current: `serde_json::from_slice(payload)`

2. **OP 21 (PrepareTransition)**: Currently uses JSON deserialization, needs binary parsing
   - Format: `[seq(2)][op(1)][transition_id(4)][protocol_version(1)]`
   - Current: `serde_json::from_slice(payload)`

3. **OP 25 (MlsExternalSender)**: Currently skips 11 bytes, should skip only 3 bytes
   - Current: `&payload[11..]`
   - Should be: `&payload[3..]`

4. **Error Swallowing in driver.rs**: Uses `let _ =` which silently drops errors
   - Line with `set_external_sender`
   - Line with `export_sender_keys`

5. **OP 12 "Encryption Ready"**: Incorrectly being sent from client
   - OP 12 is "Client Connect" (server→client only)
   - Two occurrences in driver.rs need removal

#### Lower Priority:
6. **PrepareEpoch struct**: epoch is u64 but should be u32 for binary format

7. **Speaking struct user_id**: May serialize as `null` which Discord might reject

---

## Plan

### Step 1: Fix handler.rs - Binary Parsing for OPs 21, 24, 25
**File**: `sigil-discord/src/gateway/handler.rs`

**Changes**:
- Modify `dispatch()` function for `DaveOpcode::PrepareEpoch`:
  - Parse binary format: `[seq(2)][op(1)][epoch(4 bytes LE)]`
  - Return `PrepareEpoch` with u32 epoch

- Modify `dispatch()` function for `DaveOpcode::PrepareTransition`:
  - Parse binary format: `[seq(2)][op(1)][transition_id(4)][protocol_version(1)]`
  - Return `PrepareTransition` with parsed values

- Modify `dispatch()` function for `DaveOpcode::MlsExternalSender`:
  - Change skip from 11 to 3 bytes: `&payload[3..]`

- Also update `DaveOpcode::MlsProposals` (OP 27) to use the same 3-byte skip

### Step 2: Update PrepareEpoch struct in opcodes.rs
**File**: `sigil-discord/src/gateway/opcodes.rs`

**Changes**:
- Change `PrepareEpoch.epoch` from `u64` to `u32`

### Step 3: Fix Error Handling in driver.rs
**File**: `sigil-voice/src/driver.rs`

**Changes**:
- Replace `let _ = s.set_external_sender(&ext.credential);` with proper match block that logs errors
- Replace `let _ = s.export_sender_keys(&[uid]);` with proper error handling

### Step 4: Remove Invalid OP 12 Sends
**File**: `sigil-voice/src/driver.rs`

**Changes**:
- Remove both occurrences of OP 12 "Encryption Ready" sends (lines in MlsProposals and MlsWelcome handlers)

### Step 5: Fix Speaking struct in gateway.rs
**File**: `sigil-voice/src/gateway.rs`

**Changes**:
- Add `#[serde(skip_serializing_if = "Option::is_none")]` to the `user_id` field in `Speaking` struct

---

## Dependent Files to be Edited

1. `sigil-discord/src/gateway/handler.rs` - Main binary dispatch fixes
2. `sigil-discord/src/gateway/opcodes.rs` - PrepareEpoch epoch type fix
3. `sigil-voice/src/driver.rs` - Error handling and OP 12 removal
4. `sigil-voice/src/gateway.rs` - Speaking struct serialization fix

---

## Followup Steps

After editing the files:
1. Run `cargo check` to verify no compilation errors
2. Run `cargo build` to ensure everything compiles
3. Test the DAVE protocol handshake with Discord
4. Verify audio encryption is working properly

---

## Summary of Changes

| File | Change | Priority |
|------|--------|----------|
| handler.rs | OP 24 binary parse | High |
| handler.rs | OP 21 binary parse | High |
| handler.rs | OP 25 skip 3 not 11 | High |
| handler.rs | OP 27 skip 3 not 11 | High |
| opcodes.rs | PrepareEpoch.epoch u32 | Medium |
| driver.rs | Error handling for set_external_sender | High |
| driver.rs | Error handling for export_sender_keys | High |
| driver.rs | Remove OP 12 sends | High |
| gateway.rs | Speaking user_id skip serialization | Low |

