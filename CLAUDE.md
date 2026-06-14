# pc_sink — Project Instructions & Agent Documentation

> **pc_sink** is a **new, greenfield** Rust crate: a BLE **central** that discovers,
> drains, stores, and (later) plots data from many **HyfindTag** sensor devices.
> As of today the crate has **no dependencies and no implementation** — `Cargo.toml` is
> bare and there is no source beyond the scaffold. We are building it from scratch through
> GitHub issues.
>
> The HyfindTag **firmware** is documented in the tag repo's `CLAUDE.md` and is the
> **source of truth** for every wire format. When pc_sink and the firmware must agree,
> pc_sink follows the firmware.
>
> **`tag-tester` is a different, older repo — NOT pc_sink.** It is an eframe/egui
> single-device app, structurally unrelated to pc_sink. It is useful **only as a
> reference** for how the system used to work (single connection → subscribe → append
> CSV). Do **not** treat its files, dependencies, workflows, or layout as pc_sink's
> baseline, and do not "refactor" it into pc_sink. See §A.8.

Part A is domain & architecture knowledge. Part B is the engineering conventions every PR
must follow. Read both before implementing an issue.

---

# PART A — Domain & Architecture

## A.1 What pc_sink does

pc_sink connects to Hyfind sensor tags over BLE, pulls the data each tag has buffered,
stores it locally in a queryable way, and (in a later phase) plots all tags in real time
on a shared wall-clock axis. It is a **tag tester / data sink**, not a gateway.

Target scale: on the order of **15–20+ distinct tags** over a session, but **not all
connected simultaneously** — tags are serviced opportunistically as they advertise.

> **Frontend / plotting is out of scope for now.** The current phase is the data path:
> BLE acquisition → decode → local store. UI comes later; design the core so a UI can sit
> on top of it without rework, but do not build it yet.

## A.2 The store-and-forward model (read this before touching the BLE layer)

A tag is **not** a continuous streamer. Each tag:

1. Stays **disconnected** while it acquires and buffers samples.
2. Periodically enters **connectable advertising**.
3. When a central connects, it expects to be **time-synced**, then **indicates** all
   buffered packets until empty.
4. Then disconnects and goes back to acquiring.

pc_sink's BLE layer is therefore a **continuous, fully-automatic loop**:

```
scan ──▶ tag advertising? ──▶ connect ──▶ write time-sync ──▶ drain all packets ──▶ disconnect ──▶ (repeat)
```

Consequences for design:
- **No persistent connections.** Do not model a tag as "the connected device". Model the
  session as a stream of `(TagId, BlePacket)` batches arriving from many short-lived
  connections.
- Because only a few links are held at any instant, desktop-OS BLE connection limits
  (Windows/WinRT, BlueZ often cap concurrent central links well below 20) are **not** a
  hard blocker — but treat the max-concurrent-connections count as a **parameter**, never
  assume an arbitrary N succeeds at once.
- Identity matters: all data from one physical tag across many connect/drain cycles must
  be tied to a **stable tag id** (BLE address; name as a human label).

## A.3 BLE contract (mirror of firmware — verify against tag `CLAUDE.md`)

UUID base `…-1212-efde-1523-785feabcd123`:

| Role     | UUID (16-bit slot) | Properties   | Payload |
|----------|--------------------|--------------|---------|
| Service  | `00001522-…`       | primary      | —       |
| DATA     | `00001523-…`       | **INDICATE** | `BlePacket`, 100 B |
| STIMULI  | `00001524-…`       | WRITE        | 1 byte (out of scope for now) |
| COMMAND  | `00001525-…`       | WRITE        | time-sync cmd, 9 B |

Critical quirks:
- **DATA is _indicate_, not notify** — subscribe to indications (confirmed).
- **Tags do NOT advertise the service UUID.** Match by **name prefix `HyfindTag`** from the
  scan response. Never filter on advertised service UUIDs.
- Link negotiates PHY 2M / DLE 251 / MTU 247 right after connect; tolerate that brief
  window before indications begin.

## A.4 Uplink packet — `BlePacket` (100 bytes, packed, little-endian)

Firmware `ble_packet` (`models.h`). Canonical Python cross-check:
`struct.Struct("<q" + ("HHhh"*10) + "H" + ("B"*10))`.

| Offset | Size | Field | Type | Meaning |
|--------|------|-------|------|---------|
| 0  | 8  | `time`        | i64 LE | wall-clock ms of **first** sample in batch |
| 8  | 80 | `dp[0..10]`   | 10 × {u16 temp, u16 hum, i16 adc1, i16 adc2} | samples |
| 88 | 2  | `dt`          | u16 LE | ms between consecutive samples |
| 90 | 10 | `stimuli[..]` | u8 × 10 | stimulus byte active for each sample |

**Per-sample wall-clock time:** `sample_time_ms[i] = time + i * dt`. This is the X-axis
value for the eventual plot and the key for cross-tag alignment.

### Decode chain (confirmed)

For each of the 10 samples in a packet, pc_sink must derive a fully-decoded row. The
stimulus is **always present in the packet** and must be parsed even though it is currently
a constant `0x88` on the tag.

1. **Timestamp:** `sample_time_ms[i] = time + i * dt`.
2. **Temperature / humidity:** the tag sends u16 values already converted by the Zephyr
   sensor kernel, expressed in **hundredths** (two implied decimals):
   - `temperature_C = raw_temp / 100.0`   (e.g. `2567` → `25.67 °C`)
   - `humidity_%    = raw_hum / 100.0`
   (The old tag-tester `-45 + 175*raw/65535` SHTC formula is **wrong** for this firmware.
   Note `temperature` is `u16`, so negative temperatures are not representable.)
3. **Stimulus → mV per channel:** the per-sample `stimuli[i]` byte splits into two nibbles
   — **low nibble = channel A**, **high nibble = channel B** — each translated through the
   `VoltageStimuli` table (§A.5) to a discrete mV value.
4. **ADC:** `adc1`/`adc2` are signed 16-bit values in **mV**.
5. **Current per channel** (exactly as tag-tester's `parse_current`, with `R9 = 3000` Ω):

   ```rust
   const R9: f64 = 3000.0; // ohms

   fn parse_current(adc_mv: i16, stim_mv: u16) -> f64 {
       (adc_mv as f64 - stim_mv as f64) / R9
   }
   ```

   i.e. `current_ch_A = (adc1 - stim_A_mv) / 3000`, `current_ch_B = (adc2 - stim_B_mv) / 3000`,
   where `stim_A_mv`/`stim_B_mv` are the translated mV from step 3 for that sample's
   stimulus byte.

   > ⚠️ **Unit caveat (flag, do not silently change):** tag-tester labels this output
   > "µA" while dividing mV by `3000`. mV ÷ 3000 Ω = mA, not µA (mV ÷ 3 kΩ would be µA).
   > Reproduce the formula exactly as given (`/ 3000.0`); surface the unit-label question
   > to the maintainer rather than "correcting" the constant or the label unilaterally.

## A.5 Stimulus byte — translation table (decode required now)

The packet's per-sample `stimuli[i]` is **one byte, two nibbles**:
**low nibble = channel A (ch0), high nibble = channel B (ch1)**, i.e.
`byte = (chB_code << 4) | chA_code`. pc_sink **must parse and translate this now** — it is
needed for the current calculation (§A.4) — even though the tag currently always sends
`0x88` (both channels = `mV304` = 304 mV).

Each nibble is a `VoltageStimuli` resistor-switch code mapping to a discrete mV value
(from firmware `hytag_stimuli.h`; the base bits are `mV131=0x1`, `mV161=0x2`, `mV211=0x4`,
`mV304=0x8`, others are bit-combinations):

| Code | Enum   | mV  | | Code | Enum   | mV  |
|------|--------|-----|-|------|--------|-----|
| 0x0  | OFF    | 0   | | 0x8  | mV304  | 304 |
| 0x1  | mV131  | 131 | | 0x9  | mV408  | 408 |
| 0x2  | mV161  | 161 | | 0xA  | mV431  | 431 |
| 0x3  | mV277  | 277 | | 0xB  | mV524  | 524 |
| 0x4  | mV211  | 211 | | 0xC  | mV470  | 470 |
| 0x5  | mV322  | 322 | | 0xD  | mV561  | 561 |
| 0x6  | mV347  | 347 | | 0xE  | mV582  | 582 |
| 0x7  | mV448  | 448 | | 0xF  | mV663  | 663 |

Model this as a Rust enum mirroring `VoltageStimuli` with a `u8` ↔ enum ↔ mV mapping
(exhaustive over the 16 codes — every nibble value is defined). `0x88` → ch A = ch B =
304 mV.

**PC→tag stimulus _control_ (writing the STIMULI characteristic) is deferred.** Only
*decoding* the reported stimulus is in scope now. When write-control is added later, reuse
the same enum to compose the byte; do not introduce a free-range mV integer (tag-tester's
input model does not match this firmware).

## A.6 Downlink — time-sync (write on connect, before draining)

COMMAND characteristic, packed `hyfind_downlink_cmd { u8 type; i64 time_ms }` = **9 bytes
LE**, `type = HYFIND_CMD_TIME = 0`. Wire: `[0x00][i64 LE epoch_ms]`. The tag stores
`offset = time_ms - uptime` and stamps `BlePacket.time` in real wall-clock ms thereafter.
**pc_sink must write this on every connect, before draining**, with the current epoch ms.
Handler requires exact len 9 / offset 0.

## A.7 Local storage

Target: **SQLite**, one DB file per session, queryable, with **CSV export on demand**.
Data is keyed by stable tag id so a session can be reopened and tags compared after the
fact. Keep storage behind a trait so logic stays testable without touching a real DB/file
(see Part B: "Separate I/O from logic", "No test touches real fs/clock").

## A.8 Relationship to `tag-tester` (reference only)

`tag-tester` is a **separate, pre-existing** eframe/egui application. It is **not** an
earlier version of pc_sink and shares no code with it. Use it only to understand prior
behavior:
- It connected to a **single** device, subscribed to the data characteristic, and appended
  rows to one CSV.
- Its `src/models.rs` parses an **older, incompatible** packet format (SHTC + DAC + N×ADC,
  with `i32` ADCs). **That format is obsolete** — pc_sink uses `BlePacket` (§A.4).
- It ships a WASM / GitHub-Pages build (`pages.yml`, `wasm32-unknown-unknown`). That is
  **tag-tester's** concern, not pc_sink's. BLE and SQLite are native-only; pc_sink targets
  the **native binary**. Do not import tag-tester's wasm targets or Pages workflow.
- Its extensive clippy lint table is tag-tester's. pc_sink starts clean; adopting a
  similar lint set is fine but is a deliberate choice, not inherited state.

When something here disagrees with tag-tester's code, **this document and the firmware
win.**

## A.9 Likely module shape for pc_sink (suggested, not yet built)

Greenfield — nothing exists yet beyond the crate scaffold. A natural separation that keeps
I/O at the edges:

| Module (suggested) | Responsibility |
|--------------------|----------------|
| `packet` / `models` | decode `BlePacket` from 100 raw bytes; derive per-sample row `(timestamp, temp_C, hum_%, adc1_mv, adc2_mv, stim_A_mv, stim_B_mv, current_A, current_B)`. Includes the `VoltageStimuli` enum (nibble→mV) and `parse_current`. Pure, no I/O. |
| `command` | encode the 9-byte time-sync command. Pure. |
| `ble` | scan/connect/time-sync/drain/disconnect loop over many tags; emits `(TagId, BlePacket)`. I/O at the edge. |
| `store` | SQLite session store behind a trait; insert batches, query, CSV export. |
| (`ui` later) | plotting/visibility toggles — **deferred**. |

The reference Python central (`hyfind_gateway_emulator.py`, in the firmware project)
documents the scan/connect/drain flow and the canonical packet `struct` — consult it for
protocol behavior, not for Rust structure.

---

# PART B — Engineering Conventions

## Workflow

This project is developed through GitHub issues. When assigned an issue:

1. Read the issue carefully, including any linked issues or referenced context.
2. Ask clarifying questions as comments on the issue **before** starting if anything is ambiguous.
3. Create a feature branch named `issue-<number>-<short-slug>`.
4. Implement the work according to the guidelines below.
5. Open a PR that references the issue (`Closes #N`), with a brief summary of what changed and why.

Do not start implementation if the acceptance criteria are unclear. A wrong implementation is worse than a delayed one.

## Language & Toolchain

- **Rust stable** (track the current stable release; no nightly unless explicitly required).
- `Cargo.toml` must declare exact dependency versions (`=x.y.z`) for direct dependencies unless a range is explicitly justified. (The crate currently has **no** dependencies — add them deliberately, justifying each in the PR.)

### Definition of done (every PR / every issue)

No work is "done" — and no PR may be opened or merged — until **all** of the following
pass clean on the workspace:

```bash
cargo fmt --all -- --check          # code is formatted (no diffs)
cargo check --workspace             # type-checks
cargo build --workspace             # compiles
cargo clippy --workspace --all-targets --all-features -- -D warnings   # zero warnings
cargo test --workspace              # all tests green
```

Rules:
- The agent must **run these itself** and report the output before opening a PR — never
  push code it has not verified compiles, lints, formats, and tests clean.
- `cargo fmt` must be **applied** (not just checked) so the committed code is formatted;
  `--check` is the CI gate.
- `-D warnings` means clippy warnings are **errors**. Do not silence them with
  `#[allow(...)]` without a comment justifying why the lint is inapplicable.
- If a transitional/intermediate commit cannot satisfy all five at once, say so explicitly
  in the PR; the **final** state of the branch must satisfy all five.

## Idiomatic Rust

- **Ownership first.** Prefer owned types when the cost is negligible; borrow when ownership transfer is unnecessary.
- **No unnecessary clones.** Every `.clone()` requires a comment explaining why borrowing was not sufficient.
- **Use iterators and combinators** rather than explicit `for` loops where it improves clarity. Never mutate inside an iterator chain.
- **Error handling via `Result` and `?`.** Never use `.unwrap()`/`.expect()` outside tests or `main` startup. Use `thiserror` for library errors and `anyhow` for application-level errors; stay consistent once chosen.
- **No `panic!` in library code.** Panics only in `main`, CLI parsing, or documented invariants.
- Prefer `Option`/`Result` combinators over `match` for simple transformations.

## Type System

- **Newtype pattern** for domain primitives (e.g. `struct TagId(String)` not bare `String`). Implement `Display`/`Debug`/`From` as relevant.
- **Enum over bool flags** for semantic state.
- **Phantom / type-state** where validity depends on prior operations.
- **Sealed traits** where the implementor set must be controlled.
- **Non-empty collections** get a wrapper type, not scattered runtime checks.
- Avoid `Any`/downcasting/`unsafe` unless unavoidable; every `unsafe` needs a `// SAFETY:` comment.
- Domain-specific: model the **stimulus codes as an enum** mirroring firmware `VoltageStimuli` (when that work arrives); decode/encode the 100-byte packet and 9-byte command through a **typed layer**, not raw buffers passed around.

## Functional Style

- Functions **pure where possible**.
- **Separate I/O from logic** — core logic must not perform filesystem/network/time/randomness directly. Pass dependencies in. (Critical here: BLE, SQLite, and the clock must be behind seams so decoding/aggregation is unit-testable.)
- Prefer small, composable functions; model parse → transform → store as a chain of pure transforms with I/O only at the edges.
- Avoid mutable state; scope it tightly when needed.

## Clean Code

- **Names are documentation.** No abbreviations unless domain-standard (`id`, `url`, `cfg`).
- **One responsibility per module/function.** If describing it needs "and", split it.
- **No magic numbers/strings** — named constants or enum variants. (Packet offsets, UUIDs, packet size `100`, `MAX_PACKETS_TO_SEND = 10`, `HYFIND_CMD_TIME = 0` are all named constants.)
- Keep functions short; avoid deep nesting (early returns, `?`, helpers).
- **Dead code is deleted**, not commented out.

## Documentation

- Every `pub` item has a `///` rustdoc comment unless its name is fully self-explanatory.
- First line: single sentence, indicative mood.
- Add `# Errors`, `# Panics`, `# Examples` where applicable.
- Module-level `//!` comments explain module purpose/scope.

## Testing

- **Every public function has at least one happy-path unit test.**
- **Edge cases and error paths** tested explicitly. For the packet decoder specifically:
  known-good 100-byte vectors (mirror the Python `struct` output), wrong-length input,
  signed-ADC boundaries, the `time + i*dt` derivation, all 16 stimulus-nibble→mV codes
  (both channels, e.g. `0x88` → 304/304), and `parse_current` including
  negative/`adc < stim` results.
- Inline `#[cfg(test)] mod tests { ... }` in the same file; integration tests in `tests/`.
- Test names describe scenario + outcome (`decodes_known_packet`, not `test1`).
- Use `proptest`/`quickcheck` for non-trivial parsing/transform logic.
- **No test touches the real filesystem, network, or clock.** Abstract I/O behind traits and supply fakes.
- `#[should_panic]` is a last resort; prefer `assert!(result.is_err())`.

## What Not To Do

- Do not introduce new dependencies without justification in the PR description.
- Do not change unrelated code in the same PR.
- Do not leave `TODO`/`FIXME` without an associated issue number.
- Do not silence clippy with `#[allow(...)]` without an explaining comment.
- Do not merge to `main` without a passing CI run.
- Do not invent unit conversions (temp/hum/ADC→current) — raw counts are authoritative; flag unknowns.
- Do not assume a persistent BLE connection, notify-instead-of-indicate, or service-UUID advertising — all three contradict the firmware.
- Do not pull `tag-tester`'s code, wasm targets, or Pages workflow into pc_sink — it is reference only.
- Do not build UI/plotting yet — frontend is deferred.