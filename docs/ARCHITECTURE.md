# Architecture

How the driver works. The diagrams show the main paths only. The code is the reference.

## Components

```mermaid
flowchart LR
    remote["Remote Two/3<br/>(core)"]
    subgraph driver["uc-intg-appletv"]
        ws["WebSocket server<br/>server/ws"]
        mdns["mDNS publish<br/>server/mdns.rs"]
        art["Artwork files<br/>server/artwork.rs"]
        ctrl["Controller actor<br/>controller/*<br/>sessions, operation mode,<br/>setup, entity state cache"]
        store[("config.json<br/>config_store.rs")]
        dev["AppleTvActor<br/>one per Apple TV<br/>device/actor.rs"]
        disc["Discovery<br/>device/discovery.rs"]
    end
    ratv["ratv library<br/>Companion + MRP"]
    atv["Apple TV"]

    mdns -. "driver found" .-> remote
    remote <-- "WebSocket: requests, responses, events" --> ws
    remote -- "GET /artwork/{device}/{file}" --> art
    ws <--> ctrl
    ctrl <--> store
    ctrl -- "Connect, Disconnect,<br/>Command, UpdateConfig" --> dev
    dev -- "DeviceEvent:<br/>Changes, Status, Config" --> ctrl
    ctrl -- "setup scan" --> disc
    dev -- "find address" --> disc
    dev -- "image files" --> art
    dev <--> ratv
    ratv <--> atv
```

- The controller connects a device when an awake Remote uses one of its entities. It
  disconnects the device when no awake Remote uses it.
- A command also starts a connect. That connection stays until a Remote goes to standby or
  leaves.
- Device actors push changed attributes. The controller compares them with its cache and sends
  `entity_change` events to the Remotes that subscribed to the entity and are not in standby.

## Operation mode

The `operation_mode` state machine in `src/controller/mod.rs` (`rust_fsm`). Labels on the arrows
are the inputs. `[timer start]` and `[timer stop]` are the `SetupFlowTimer` and
`CancelSetupFlowTimer` outputs.

```mermaid
stateDiagram-v2
    [*] --> RequireSetup
    RequireSetup --> Running: ConfigurationAvailable
    RequireSetup --> RequireSetup: AbortSetup
    RequireSetup --> SetupFlow: SetupDriverRequest [timer start]
    Running --> SetupFlow: SetupDriverRequest [timer start]
    Running --> RequireSetup: AbortSetup
    SetupFlow --> WaitSetupUserData: RequestUserInput
    SetupFlow --> Running: Successful [timer stop]
    SetupFlow --> SetupError: SetupError [timer stop]
    SetupFlow --> RequireSetup: AbortSetup [timer stop]
    WaitSetupUserData --> SetupFlow: SetupUserData
    WaitSetupUserData --> SetupError: SetupError [timer stop]
    WaitSetupUserData --> RequireSetup: AbortSetup [timer stop]
    SetupError --> Running: ConfigurationAvailable
    SetupError --> RequireSetup: AbortSetup
    SetupError --> SetupFlow: SetupDriverRequest [timer start]
    SetupError --> SetupError: SetupError
```

After an abort or a failed setup, the controller sends `ConfigurationAvailable` again when
devices are configured, so the mode goes back to `Running`. When the setup timer ends, the setup
ends with `SetupError`.

## Setup pages

`SetupState` in `src/controller/handler/setup.rs` is the page that the setup waits for. Input
that is not valid shows the same page again with an error.

```mermaid
stateDiagram-v2
    state "Setup ends: Ok or error" as Done

    [*] --> RestoreChoice: first setup
    [*] --> ConfigMode: reconfigure

    RestoreChoice --> Restore: restore from backup
    RestoreChoice --> Discover: continue

    ConfigMode --> Discover: add, reset
    ConfigMode --> Configure: configure device
    ConfigMode --> Pin: pair again
    ConfigMode --> Backup: backup
    ConfigMode --> Restore: restore
    ConfigMode --> Done: settings, remove

    Discover --> ChooseDevice: Apple TVs found
    Discover --> Done: none found, scan failed
    ChooseDevice --> Pin: pairing started
    Pin --> Done: PIN checked, config saved
    Configure --> Done: config saved
    Restore --> Done: config saved
    Backup --> Done
    Done --> [*]
```

- `Pin` is shown only when pairing could start. A pairing error ends the setup with an error.
- "Pair again" first scans for the configured device. If it is not found, the setup ends with
  an error.
- Every page that changes devices writes `config.json` first. If the write fails, nothing
  changes and the setup ends with an error.
- A first setup through discovery and `reset` replace all devices, but only when an Apple TV
  is paired. Restore always replaces all devices.

## Device connection

`DeviceStatus` in `src/device/mod.rs`, changed by `AppleTvActor` in `src/device/actor.rs`.

```mermaid
stateDiagram-v2
    [*] --> Disconnected
    Disconnected --> Connecting: Connect, command, reconnect timer
    Connecting --> Connected: connected
    Connecting --> Disconnected: failed, not found [reconnect timer]
    Connecting --> Disconnected: Disconnect
    Connecting --> Connecting: network not ready [retry every 50 ms, up to 3 s]
    Connecting --> AuthError: pairing not valid
    Connected --> Disconnected: Disconnect [waits up to 2 s for a running command]
    Connected --> Disconnected: all protocols closed [reconnect timer]
    Connected --> Connecting: 3 command timeouts in 60 s
    Connected --> Connecting: new pairing, address or ids
    AuthError --> Connecting: Connect, or new pairing, address or ids while wanted
    AuthError --> Disconnected: new pairing, address or ids while not wanted
```

- Reconnect timer: 2 s times the number of tries since the last `Connect` or the last
  connection that stayed up for 60 s, at most 30 s. It runs only while the device is wanted.
- `Connected` can have only one protocol open. The actor tries to connect the missing protocol
  again and keeps the status.
- With the MRP join, Companion connects first and the status becomes `Connected`. MRP connects
  next. Commands wait for MRP for at most 0.5 s. Siri over MRP waits for the end of the join.
- After a requested `Disconnect`, the entities keep their state. After a lost connection, they
  become unavailable.
- `AuthError` stays after a `Disconnect`. There is no reconnect until the next `Connect`.
- While one device is in `AuthError`, the driver device state is `ERROR`.

## Entity command

```mermaid
sequenceDiagram
    participant R as Remote
    participant C as Controller
    participant D as AppleTvActor
    participant T as Apple TV (ratv)

    R->>C: entity_command
    C->>D: Command
    D->>D: check parameters, connect if needed
    D-->>C: Ok or error
    C-->>R: result
    Note over D,T: The command runs in the background,<br/>after the connect and the command before it.
    D->>T: request
    T-->>D: state change (watch channel or event)
    D->>C: DeviceEvent::Changes
    C->>R: entity_change (subscribed Remotes not in standby)
```

- The result tells only that the command was accepted. Errors after the result are only logged.
- Voice commands answer through the controller with `DeviceEvent::Accepted`, so the result comes
  before the assistant events.
