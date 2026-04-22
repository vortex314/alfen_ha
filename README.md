# Alfen Eve to MQTT Bridge

Note: The code in this repository was generated with GitHub Copilot.

This project connects an Alfen Eve charger over Modbus TCP and publishes charger telemetry and control to MQTT, including Home Assistant auto-discovery.

## Goal

The bridge provides a reliable path from low-level charger registers to high-level home automation entities.

- Poll charger state and metering values from Modbus TCP.
- Publish normalized JSON state to MQTT.
- Publish Home Assistant discovery config so entities appear automatically.
- Accept a writable command (`max_current`) over MQTT and forward it to Modbus.
- Report online/offline availability with MQTT Last Will and reconnect logic.

## Architecture

```
+---------------------+       Modbus TCP        +------------------+
| alfen-mqtt bridge   | <---------------------> | Alfen Eve charger|
| (Rust, Tokio)       |                         | (unit_id, regs)  |
+----------+----------+                         +------------------+
           |
           | MQTT (state, discovery, commands)
           v
+---------------------+   auto-discovery   +----------------------+
| MQTT broker         | <----------------> | Home Assistant       |
+---------------------+                    +----------------------+
```

### Runtime components

- Config loader (`config.toml`): charger endpoint, MQTT endpoint, naming.
- Modbus client (`tokio-modbus`): periodic register reads and command writes.
- MQTT client (`rumqttc`):
  - publishes state payloads,
  - publishes retained discovery payloads,
  - subscribes to command topic.
- Command channel: MQTT event loop forwards command payloads to the Modbus writer.
- Recovery loop:
  - on Modbus read failure: publish `offline`, reconnect, then publish `online`.
  - MQTT uses Last Will (`offline`) for crash visibility.

## Design

### Design goals

- Keep protocol-specific logic isolated: Modbus decode/encode stays in one place, MQTT/HA in another.
- Publish a complete, self-describing state payload that Home Assistant can consume with templates.
- Fail visibly: communication failures are represented both in availability and in diagnostic state fields.
- Prefer retained MQTT messages for restart resilience.

### Data model

`ChargerState` is the canonical state snapshot published as JSON and includes:

- Identity: `station_name`, `serial_nr`, `firmware_ver`
- Electrical values: phase voltages/currents/powers, total power, power factor
- Energy: `energy_total_kwh`
- Status: `available`, `mode3_state`, `mode3_state_name`
- Control reflection: `max_current`
- Setpoint proof: `setpoint_accounted_raw`, `setpoint_accounted`

The published MQTT payload extends `ChargerState` with runtime diagnostics:

- `comm_ok`: `true` on successful poll publish, `false` when publishing explicit error state.
- `last_error`: `null` on healthy cycles, string reason on failures.
- `last_update_epoch_s`: unix timestamp of publish moment.

This keeps Home Assistant sensor templates simple while preserving operational context.

### Register decoding conventions

- Register addresses are **0-based** wire addresses.
- Endianness is **big-endian**.
- `f32` values use 2 registers (4 bytes).
- `f64` values use 4 registers (8 bytes).
- ASCII strings are packed as two bytes per register and right-trimmed (`NUL`/space).

### MQTT topic model

With:

- `topic_prefix = alfen`
- `unique_id = alfen_eve_01`

Topics are:

- State (retained): `alfen/alfen_eve_01/state`
- Availability (retained): `alfen/alfen_eve_01/availability`
- Command (subscribed): `alfen/alfen_eve_01/set/max_current`
- HA discovery prefix: `homeassistant/...`

### MQTT state behavior

The bridge publishes two state forms to `.../state`:

1. Normal state: full charger telemetry plus diagnostics (`comm_ok=true`, `last_error=null`).
2. Error state: minimal payload with `comm_ok=false`, `last_error=<reason>`, timestamp.

Error-state publishing is triggered for:

- Modbus read failures.
- `set/max_current` command parse/write failures.

State messages are retained so Home Assistant restores last-known values immediately after reconnect/restart.

Example normal state payload (shape):

```json
{
  "station_name": "Alfen Eve",
  "serial_nr": "ACE123456",
  "firmware_ver": "6.2.0",
  "voltage_l1": 231.2,
  "voltage_l2": 230.9,
  "voltage_l3": 231.5,
  "current_l1": 15.8,
  "current_l2": 0.0,
  "current_l3": 0.0,
  "power_l1": 3650.0,
  "power_l2": 0.0,
  "power_l3": 0.0,
  "power_total": 3650.0,
  "power_factor": 0.99,
  "energy_total_kwh": 1245.311,
  "available": true,
  "mode3_state": 3,
  "mode3_state_name": "C1 (charging, no ventilation)",
  "max_current": 16.0,
  "setpoint_accounted_raw": 1,
  "setpoint_accounted": true,
  "comm_ok": true,
  "last_error": null,
  "last_update_epoch_s": 1776521013
}
```

Example error state payload (shape):

```json
{
  "comm_ok": false,
  "last_error": "modbus_read_error: timed out",
  "last_update_epoch_s": 1776521075
}
```

### Home Assistant entity model

The bridge publishes retained discovery configs for 20 entities:

- Sensors: power total, power L1/L2/L3, voltage L1/L2/L3, current L1/L2/L3, power factor, energy total, status text, max current (read-only), applied max current (`reg 1206`), last error.
- Binary sensors: connectivity (`comm_ok` -> `ON/OFF`, diagnostic category), setpoint accounted (`reg 1214` -> `ON/OFF`, diagnostic category).
- Number: max charge current command (`0..32 A`, step `1 A`).
- Switch: friendly charging control (`ON/OFF`) mapped to max current writes.

All entities share one HA device descriptor keyed by `charger.unique_id` and use MQTT availability topic.

### Runtime flow

1. Load `config.toml`.
2. Connect MQTT and set Last Will to `offline`.
3. Connect Modbus TCP (slave `unit_id`).
4. Read initial state and publish HA discovery.
5. Publish availability `online`.
6. Poll Modbus on interval and publish retained state.
7. Process MQTT command topic for `max_current` writes.
8. On read/write failures publish diagnostic error state; on read failures also toggle availability and attempt reconnect.

### Update Cadence Logic

Not all Home Assistant entities appear to update at the same pace. This is expected and comes from how the bridge publishes different topics.

#### Poll-driven updates

- The bridge polls Modbus every `alfen.poll_interval_secs` seconds.
- On each successful poll, it publishes one retained JSON payload to:
  - `<topic_prefix>/<unique_id>/state`
- Most telemetry entities are template sensors that read fields from this shared state payload.

In other words, power/current/voltage/energy/status are sent every poll cycle.

#### Event-driven updates

Some values only change on specific events:

- Availability topic (`.../availability`):
  - `online` at startup/reconnect
  - `offline` on Modbus read failure or MQTT LWT event
- Diagnostic fields (`comm_ok`, `last_error`) in state payload:
  - updated when errors occur (read failures, command write failures)
  - otherwise remain in healthy state
- Home Assistant discovery topics (`homeassistant/.../config`):
  - published at startup (retained)
  - not republished every poll

### Control Watchdog and Proof Registers

Alfen control setpoint handling can be transient and priority-gated.

#### Register `1210` (`MAX_CURRENT`) watchdog behavior

- The charger can treat register `1210` as temporary setpoint input with watchdog semantics.
- If no refresh arrives before watchdog expiry, charger may fall back to a safe/default current.
- The bridge therefore keeps an internal `desired_max_current` (last accepted MQTT command) and rewrites it periodically.
- Heartbeat period is configurable via:
  - `alfen.setpoint_heartbeat_secs`
  - default: `30` seconds
  - safety clamp: minimum `5` seconds

This means you do not need a separate Home Assistant automation that re-sends unchanged values.

#### Register `1214` proof (`SETPOINT_ACCOUNTED`)

The bridge reads register `1214` and logs/exports proof state:

- `1`: charger is listening to Modbus setpoint.
- `0`: charger received setpoint but is ignoring it due to higher-priority control.
- other value: unexpected, logged as warning.

Read results are exposed in MQTT state as:

- `setpoint_accounted_raw` (`u16` or `null`)
- `setpoint_accounted` (`true`/`false` or `null`)

And in Home Assistant as:

- Binary sensor `Setpoint Accounted` (`ON` when `1214 == 1`, `OFF` otherwise)

The proof check runs:

- after `set/max_current` command writes
- after `set/charging` command writes
- after each heartbeat refresh

#### Requested vs applied visibility

After command writes, the bridge performs immediate direct readback of `1210` and compares:

- requested amps (from MQTT command)
- applied amps (decoded from raw registers)

It logs explicit mismatch warnings when the charger overrules or maps values differently.

#### Register `1206` applied-limit visibility

The bridge also reads register `1206` as `applied_max_current` (effective limit actually enforced by charger logic).

- MQTT state field: `applied_max_current`
- Home Assistant sensor: `Applied Max Current`

If `1214 == 1` (setpoint accounted) but `applied_max_current` is lower than requested `max_current`, a higher-priority cap is likely active (for example station installer limit / local control policy).

#### Home Assistant payload format for `max_current`

Register `1210` is written as Modbus `float32` by this bridge.

- Recommended HA command payload: decimal text, e.g. `10.0`.
- The bridge also accepts plain integer text (`10`), quoted text (`"10.0"`), and JSON numeric payloads.

Example HA MQTT publish service call:

```yaml
service: mqtt.publish
data:
  topic: alfen/alfen_eve_01/set/max_current
  payload: "10.0"
```

If a command is accepted, logs show both requested and applied value after direct readback.

#### Optional identity reads are one-shot

Station identity registers (`station_name`, `serial_nr`, `firmware_ver`) are intentionally probed once per process start.

- Reason: some charger firmware returns `Illegal data address` for these registers.
- One-shot probing avoids repeated warning spam on every poll.
- If unavailable, defaults are used.

#### Why HA may show different timestamps

Home Assistant often distinguishes:

- `last_updated`: message received / state processed
- `last_changed`: value actually changed

If a value is republished but unchanged, `last_changed` may remain old even though updates are still arriving.

## Modbus Packet Forms

This bridge uses Modbus TCP ADU format:

`MBAP (7 bytes) + PDU`

### MBAP header

- Transaction ID: 2 bytes (client-chosen correlation ID)
- Protocol ID: 2 bytes (always `0x0000` for Modbus)
- Length: 2 bytes (Unit ID + PDU length)
- Unit ID: 1 byte (`config.alfen.unit_id`, typically `0x01`)

### PDU function codes used

- `0x03` Read Holding Registers
- `0x10` Write Multiple Registers

### 1) Read Holding Registers request (`0x03`)

PDU request:

- Function: `0x03`
- Start address: 2 bytes
- Quantity: 2 bytes

Wire form:

`[TID hi][TID lo][00][00][00][06][Unit][03][Addr hi][Addr lo][Qty hi][Qty lo]`

Example: read Voltage L1..L3 block (`start=306 (0x0132), qty=6 (0x0006)`)

`00 01 00 00 00 06 01 03 01 32 00 06`

Response form:

`[TID][PID][Len][Unit][03][ByteCount][Data ...]`

For `qty=6`, `ByteCount=12`, returning 6 registers (12 bytes).

### 2) Write Multiple Registers request (`0x10`)

Used for `MAX_CURRENT` at register `1210 (0x04BA)`, encoded as `f32` over 2 registers.

PDU request:

- Function: `0x10`
- Start address: 2 bytes
- Quantity: 2 bytes
- Byte count: 1 byte (`quantity * 2`)
- Register values: N*2 bytes

Wire form:

`[TID][00 00][Len][Unit][10][Addr hi][Addr lo][Qty hi][Qty lo][ByteCount][Data ...]`

Example command: set max current to `16.0 A`

- `16.0f32` bytes = `41 80 00 00`
- Registers = `0x4180`, `0x0000`
- Start `0x04BA`, qty `0x0002`, byte count `0x04`

Request bytes:

`00 02 00 00 00 0B 01 10 04 BA 00 02 04 41 80 00 00`

Normal response echoes write range:

`00 02 00 00 00 06 01 10 04 BA 00 02`

### Register blocks polled

The bridge optimizes reads by polling contiguous blocks:

- Station name: `100`, qty `25` (ASCII)
- Serial number: `125`, qty `11` (ASCII)
- Firmware version: `136`, qty `5` (ASCII)
- Voltage L1..L3: `306`, qty `6` (`f32 x3`)
- Current L1..L3: `320`, qty `6` (`f32 x3`)
- Power L1..L3 + total + factor: `334`, qty `10` (`f32 x5`)
- Energy total: `374`, qty `4` (`f64`)
- Availability + Mode3 state: `1200`, qty `2`
- Max current: `1210`, qty `2` (`f32`)

### Register To MQTT Entity Mapping

The table below maps Modbus register addresses (0-based wire addresses) to MQTT state fields and Home Assistant entities discovered by this bridge.

| Modbus Register(s) | Type | MQTT State Field | HA Entity (name / suffix) | Notes |
|---|---|---|---|---|
| `100..124` | ASCII (`25 x u16`) | `station_name` | Device metadata only | Optional read; not a standalone HA sensor |
| `125..135` | ASCII (`11 x u16`) | `serial_nr` | Device metadata only | Optional read; not a standalone HA sensor |
| `136..140` | ASCII (`5 x u16`) | `firmware_ver` | Device metadata only | Optional read; used in device `sw_version` |
| `306..307` | `f32` | `voltage_l1` | `Voltage L1` / `voltage_l1` | Sensor |
| `308..309` | `f32` | `voltage_l2` | `Voltage L2` / `voltage_l2` | Sensor |
| `310..311` | `f32` | `voltage_l3` | `Voltage L3` / `voltage_l3` | Sensor |
| `320..321` | `f32` | `current_l1` | `Current L1` / `current_l1` | Sensor |
| `322..323` | `f32` | `current_l2` | `Current L2` / `current_l2` | Sensor |
| `324..325` | `f32` | `current_l3` | `Current L3` / `current_l3` | Sensor |
| `334..335` | `f32` | `power_l1` | `Power L1` / `power_l1` | Sensor |
| `336..337` | `f32` | `power_l2` | `Power L2` / `power_l2` | Sensor |
| `338..339` | `f32` | `power_l3` | `Power L3` / `power_l3` | Sensor |
| `340..341` | `f32` | `power_total` | `Power Total` / `power_total` | Sensor |
| `342..343` | `f32` | `power_factor` | `Power Factor` / `power_factor` | Sensor |
| `374..377` | `f64` (Wh) | `energy_total_kwh` | `Energy Total` / `energy_total` | Bridge converts Wh to kWh |
| `1200` | `u16` | `available` | Availability topic (`.../availability`) | Not a discovered sensor; drives online/offline semantics |
| `1201` | `u16` | `mode3_state`, `mode3_state_name` | `Status` / `mode3_state` | Text sensor uses `mode3_state_name` |
| `1210..1211` | `f32` | `max_current` | `Max Current` / `max_current_ro` | Read-only sensor reflection |
| `1210..1211` (write) | `f32` | Command topic: `.../set/max_current` | `Max Charge Current` / `max_current_set` | Writable HA number entity |
| `1210..1211` (write, via helper command) | `f32` | Command topic: `.../set/charging` | `Charging` / `charging` | HA switch: `ON` -> `6A`, `OFF` -> `0A` |

Additional MQTT-only diagnostic entities:

| Source | MQTT State Field | HA Entity (name / suffix) | Notes |
|---|---|---|---|
| Bridge runtime | `comm_ok` | `Connectivity` / `connectivity` | Binary sensor (`ON/OFF`) |
| Bridge runtime | `last_error` | `Last Error` / `last_error` | Diagnostic sensor |
| Bridge runtime | `last_update_epoch_s` | Not discovered as entity | Timestamp in state payload |

## Configuration

See `config.toml`:

- `[alfen]` host, port, unit id, poll interval
- `[mqtt]` broker host/port, credentials, topic prefixes
- `[charger]` user-facing name and stable unique id

## Modbus Explorer

This repository also includes a separate GUI tool to inspect and write Modbus registers in real time:

- Binary: `src/bin/modbus_explorer.rs`
- Config: `modbus_explorer.yaml`
- UI: `egui` table view with live values, raw register words, errors, and per-row write inputs.

The explorer is intended for register discovery/debugging and manual control testing. It does not publish MQTT and runs independently from the bridge.

### Explorer YAML format

Top-level fields:

- `tcp.host`: Modbus TCP host/IP.
- `tcp.port`: Modbus TCP port (normally `502`).
- `tcp.unit_id`: default slave/unit id used when a row does not set its own `unit_id`.
- `tcp.poll_interval_ms`: polling interval in milliseconds.
- `tcp.timeout_ms`: connect timeout in milliseconds.

Each entry in `registers` supports:

- `name`: label shown in UI.
- `address`: 0-based Modbus register address.
- `unit_id` (optional): per-register slave override (useful for Alfen split map, e.g. socket `1`, station `200`).
- `register_type`: `holding` or `input`.
- `data_type`: `u16`, `i16`, `u32`, `i32`, `u64`, `f32`, `f64`, `bool`, `string`.
- `endian`: `big`, `little`, or `word_swap`.
- `unit` (optional): engineering unit label.
- `writable` (optional): `true` enables a write input/button for that row.
- `length` (optional): number of 16-bit words for `string`.
- `scale` (optional): numeric multiplier applied after decode.

### Explorer behavior

- Polls all configured rows continuously and updates the table in real time.
- Shows decoded value and raw register words.
- Retries `Illegal data address` reads once at `address + 1`.
- Reconnects only on transport errors (broken pipe, reset, timeout, etc.).
- Writes use Modbus single-register (`0x06`) or multi-register (`0x10`) depending on datatype width.

### Run Explorer

From terminal:

```bash
cargo run --bin modbus_explorer -- modbus_explorer.yaml
```

From VS Code Task Runner:

- Task label: `modbus explorer`
- File: `.vscode/tasks.json`

## Run

```bash
cargo run
# or
cargo run -- config.toml
```

On startup, the bridge:

1. Connects to MQTT and Modbus.
2. Reads initial charger state.
3. Publishes Home Assistant discovery payloads (retained).
4. Publishes availability `online`.
5. Enters poll-and-publish loop and listens for write commands.

## Troubleshooting

### 1) Bridge starts but never reaches `Modbus connected`

Symptoms:

- Log stops around `Connecting to Alfen Eve at ...`
- No state messages are published.

Checks:

- Verify `alfen.host` and `alfen.port` in `config.toml`.
- Ensure charger Modbus TCP is enabled and reachable on port `502`.
- Confirm `alfen.unit_id` matches charger slave/unit ID.
- Test network path from host running this bridge to charger VLAN/subnet.

### 2) Repeated `Modbus read error` and reconnect loops

Symptoms:

- Alternating read errors and reconnect attempts.
- Availability toggles between `offline` and `online`.

Checks:

- Increase polling interval (`alfen.poll_interval_secs`) to reduce load.
- Check for unstable network links, AP roaming, firewall rules, or NAT timeouts.
- Verify no other clients are saturating charger Modbus sessions.

### 3) MQTT connect/auth problems

Symptoms:

- MQTT errors in logs.
- No `state` or discovery topics on broker.

Checks:

- Verify `mqtt.host`, `mqtt.port`, `mqtt.username`, `mqtt.password`.
- Prefer a dedicated broker user for this bridge (for example `alfen_bridge`), separate from Home Assistant's MQTT user.
- If broker allows anonymous access, keep username empty to skip credentials.
- Confirm TLS expectations: this binary uses `rumqttc` with rustls support, but current config uses plain MQTT (`1883`).
- Confirm broker ACL allows publish/subscribe on:
  - `<topic_prefix>/<unique_id>/#`
  - `<ha_discovery_prefix>/#`

### 4) Home Assistant entities do not appear

Symptoms:

- Bridge runs, but no new entities in HA.

Checks:

- Ensure HA MQTT integration is connected to the same broker.
- Ensure `ha_discovery_prefix` matches HA discovery prefix (usually `homeassistant`).
- Ensure discovery topics are present and retained:
  - `homeassistant/sensor/<unique_id>_*/config`
  - `homeassistant/number/<unique_id>_max_current_set/config`
- Ensure `charger.unique_id` is stable; changing it creates a new device identity.

### 5) State is present, but values look wrong

Symptoms:

- Unexpected voltage/current/power values.

Checks:

- Confirm charger model/register map matches assumptions in `src/main.rs`.
- This bridge decodes using big-endian `f32`/`f64`; a different firmware/register map may require offsets or format changes.
- Verify station info fields decode as expected (name, serial, firmware) to confirm general addressing sanity.

### 6) `set/max_current` command has no effect

Symptoms:

- Command published, but charger limit does not change.
- Log may show parse or range errors.

Checks:

- Publish numeric payload only, for example `16` or `16.0`.
- Allowed range is `0..=32` A; out-of-range values are rejected.
- Confirm MQTT topic exactly matches:
  - `<topic_prefix>/<unique_id>/set/max_current`
- Friendly switch topic is also supported:
  - `<topic_prefix>/<unique_id>/set/charging` with payload `ON`/`OFF` (also accepts `1`/`0`, `true`/`false`).
- Ensure charger is in a state that accepts dynamic current limit updates.
- Check for local charger settings that override remote control.

### 7) Availability is stuck `offline`

Symptoms:

- HA shows device unavailable after restart.

Checks:

- Availability topic is retained; verify latest retained payload on:
  - `<topic_prefix>/<unique_id>/availability`
- Ensure only one bridge instance is active per `client_id`.
- Duplicate MQTT `client_id` values can disconnect the active session and trigger LWT behavior.

### Quick diagnostics

- Run with debug logs:

```bash
RUST_LOG=alfen_mqtt=debug cargo run
```

- Validate config file path explicitly:

```bash
cargo run -- config.toml
```

- Watch key broker topics (example with mosquitto):

```bash
mosquitto_sub -h <broker> -t 'alfen/#' -v
mosquitto_sub -h <broker> -t 'homeassistant/#' -v
```

## Docker

Build image:

```bash
docker build -t alfen-ha:local .
```

Run container (using bundled `/app/config.toml`):

```bash
docker run --rm --network host alfen-ha:local
```

Run container with custom config file:

```bash
docker run --rm --network host \
  -v "$(pwd)/config.toml:/app/config.toml:ro" \
  alfen-ha:local /app/config.toml
```

Run with Docker Compose:

```bash
docker compose up -d --build
```

Stop compose stack:

```bash
docker compose down
```

Notes:

- `--network host` is recommended for direct LAN access to charger and broker.
- The binary reads config from the argument path (default in container: `/app/config.toml`).
- `docker-compose.yml` mounts `./config.toml` read-only into the container so local config changes are picked up after restart.

### Export Image For Portainer

Use the helper script (analogous to `marstek_ha`):

```bash
./scripts/export_portainer_image.sh
```

Optional flags:

```bash
./scripts/export_portainer_image.sh --image alfen-ha:local --output alfen-ha-local.tar
./scripts/export_portainer_image.sh --gzip
./scripts/export_portainer_image.sh --no-build
```

In Portainer: `Images` -> `Load image` and upload the tar (or tar.gz).
