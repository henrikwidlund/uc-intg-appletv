# Apple TV integration for Remote Two/3 (Rust)

Integration driver for the [Unfolded Circle](https://www.unfoldedcircle.com/) Remote Two and
Remote 3. It controls Apple TV devices on the local network with the
[ratv](https://github.com/henrikwidlund/ratv) library.

The driver id is `appletv_rs`, so it can run next to the Apple TV integration that comes with the
Remote.

Contents:

- [Features](#features)
- [Compared to the built-in Apple TV integration](#compared-to-the-built-in-apple-tv-integration)
- [Requirements](#requirements)
- [Install on the Remote](#install-on-the-remote)
- [Setup options](#setup-options)
- [Run on a computer](#run-on-a-computer)
- [Known limits](#known-limits)
- [Troubleshooting](#troubleshooting)
- [Debug log](#debug-log)
- [Backup and restore](#backup-and-restore)
- [Details](#details): how the driver works inside
  - [Network and wake-up](#network-and-wake-up)
  - [Log format](#log-format)
  - [systemd journal](#systemd-journal)
- [Build](#build)
- [Links](#links)
- [Credits](#credits)
- [License](#license)

## Features

- **Media player**: power, playback, seek, fast forward and rewind, skip, chapters, volume and mute,
  repeat and shuffle, audio track and subtitles, navigation, swipes, channel up and down, Guide,
  Context Menu, App Switcher, Control Center, Screensaver, like and dislike, app list,
  audio outputs, browse (apps and Up Next) and play media.
  It shows title, artist, album, artwork, position, duration and media type.
- **Remote**: buttons, hold, repeat and button sequences. It has a UI page with App Switcher,
  Screensaver, Control Center and Guide.
- **Selects**: app, audio output, audio language, subtitles, account, chapter, Up Next, playback
  speed, shuffle and repeat.
- **Sensors**: app, audio output, audio language, subtitles, playback speed, shuffle and repeat
  (the same values as the selects), episode, genre, content rating, audio format, live content and
  volume control.
- **Voice assistant**: Siri with the voice button of the Remote.

One pairing with one PIN covers all features.

## Compared to the built-in Apple TV integration

- One PIN for the pairing instead of two.
- Siri with the voice button.
- Repeat, shuffle, audio track, subtitles, browse and play media on the media player. The built-in
  integration has repeat and shuffle turned off.
- Mute and unmute as separate commands, and the mute state is shown. The built-in integration has
  only mute toggle and does not report the mute state.
- More selects and sensors. The built-in integration has only the app and the audio output.
- A hold time and a delay for remote commands. The built-in integration has only repeat.
- Playback updates come only when the Apple TV sends them. The built-in integration also asks the
  Apple TV every 10 seconds what plays.
- Entities keep their state while the Remote sleeps. They do not show as unavailable when it wakes
  up.
- Backup and restore of the configuration, and _Pair selected device again_ in the setup.
- _Debug log_ and _Siri protocol_ settings in the setup.

The built-in integration has some things that this driver does not have:

- It comes with the Remote firmware and is updated with it.
- The setup is in more languages. The setup of this driver is in English only.
- A Top Menu command.

The driver id and the entity ids are different, so activities must be set up again after a change
from one integration to the other.

## Requirements

- Apple TV HD (4th generation) or Apple TV 4K (all generations). Older models, and devices that do
  not report a model, are not shown in the setup.
  Tested with an Apple TV 4K (1st generation) on tvOS 26.6.1 and an Apple TV from 2026 (AppleTV18,1)
  on tvOS 27.2.1.
- The Apple TV and the Remote are on the same network or use an mDNS reflector.
  Multicast DNS (mDNS) must not be blocked.
- On the Apple TV, in the AirPlay settings: set _Allow Access_ to _Anyone on the Same Network_.
- For the voice assistant, Siri must be on in the Apple TV settings: Settings > General > Siri.
- A fixed IP address for the Apple TV (DHCP reservation) is recommended. The driver then finds the
  Apple TV faster after a restart.

## Install on the Remote

1. Download `uc-intg-appletv-rs-<version>-aarch64.tar.gz` from the [releases](https://github.com/henrikwidlund/uc-intg-appletv/releases).
2. Web configurator: Integrations > Add new > Install custom. Choose the file.
3. Start the setup of _Apple TV (Rust)_. On the _Restore_ page, leave _Restore from backup_ off.
4. Leave the IP address empty to search the network.
5. Choose the Apple TV and enter the PIN that the TV shows.

Start the setup again to add, remove, configure or pair a device again.

### Update

Install the new version the same way: _Install custom_ with the new file. Or use the
[Integration Manager](https://github.com/JackJPowell/uc-intg-manager) by JackJPowell.
The configuration and the entities stay.

## Setup options

For each Apple TV, on the page where you choose it and in _Configure selected device_:

- _Change volume on all connected devices_ is on by default. The volume slider then changes all
  AirPlay outputs that play with the Apple TV, and the volume shows their average. Turn it off to
  change only the Apple TV.

For the driver, on the _Restore_ page of a new setup, or when you run the setup again (then choose
_Save settings_):

- _Debug log_ writes more lines to the log. See [Debug log](#debug-log).
- _Siri protocol_: keep it at _Auto_. Siri then uses the same protocol as the iPhone remote.
  _MRP only_ is only for finding errors. See [Network and wake-up](#network-and-wake-up) for the
  protocols.

## Run on a computer

The driver can also run on a computer as an external integration. The Remote finds it with mDNS.

Each release has binaries for Linux, macOS and Windows, for amd64 and arm64.
The Linux binaries are static and run on any distribution.
Unpack the archive and start `uc-intg-appletv` (`uc-intg-appletv.exe` on Windows).
To build and run it from the source code:

```shell
cargo run --release
```

After the driver starts, add it on the Remote:

1. Web configurator: Integrations > Add new > Add new integration.
2. Wait until _Apple TV (Rust)_ shows up in the dialog, and choose it.
3. The setup starts. Do it as in [Install on the Remote](#install-on-the-remote), from step 3.

The driver reads these environment variables:

| Variable                   | Default             | Description                                                            |
|----------------------------|---------------------|------------------------------------------------------------------------|
| `UC_CONFIG_HOME`           | working directory   | Folder for the configuration and cache files.                          |
| `UC_INTEGRATION_INTERFACE` | `0.0.0.0`           | Network interface to listen on.                                        |
| `UC_INTEGRATION_HTTP_PORT` | `9090`              | WebSocket and artwork port.                                            |
| `UC_DISABLE_MDNS_PUBLISH`  | `false`             | Do not announce the driver with mDNS.                                  |
| `UC_SETUP_TIMEOUT`         | `3600`              | Setup timeout in seconds. `0` disables it.                             |
| `UC_CLIENT_NAME`           | host name           | Name shown on the Apple TV for the pairing.                            |
| `UC_API_MSG_TRACING`       | off                 | Log WebSocket messages: `all`, `in` or `out`. Needs debug log level.   |
| `RUST_LOG`                 | `info`              | Log level, for example `info,uc_intg_appletv=debug`.                   |

`config.json` contains the pairing keys. On Linux and macOS, only the owner can read it.
The folder also holds `settings.json` (debug log and Siri protocol), the cache files and the
`artwork` folder. A `config.json` that cannot be read is moved to `config.json.bad-<time>`.

### Docker

```shell
docker run -d --name uc-intg-appletv --network host \
  -v uc-intg-appletv:/data henrikwidlund/uc-intg-appletv
```

- The image runs as user `65532`. A named volume works as it is. For a folder on the host, give
  that user write access.
- `--network host` lets the driver find the Apple TV and announce itself with mDNS. It works on
  Linux only.
- Without host networking, publish the port with `-p 9090:9090` and add
  `-e UC_DISABLE_MDNS_PUBLISH=true`. Add the driver to the Remote with the IP address of the
  computer. Enter the IP address of the Apple TV in the setup.
- The image has no shell. To change a setting, use the variables in the table above.

## Known limits

- Volume control works only if the Apple TV can change the volume of the TV or receiver (HDMI CEC).
  Otherwise the volume controls stay on the Remote, but they do nothing. For set volume and mute,
  the driver writes a log line. The _Volume control_ sensor shows off.
- The Apple TV does not report the app in front. The app select shows the app opened through the
  driver or the app that plays media.
- The Apple TV does not report the current account. The account select shows the last account chosen
  through the driver.
- Metadata, artwork and playback speeds depend on the app. Many apps send only some of them.
  For example, the Apple TV app sends genre and content rating, and season and episode for shows.
  The Music app sends the genre.
- The playback speed select is empty when the app does not report speeds or does not allow a speed
  change.
- Fast-forward and rewind depend on the app. In Disney+ they skip forward and back instead.
  HBO Max and Netflix do not support them. Skip works in all three.

## Troubleshooting

- **The Apple TV is not in the setup list**: check that mDNS is not blocked between the Apple TV
  and the Remote, or enter the IP address of the Apple TV in the setup. Only Apple TV HD and
  Apple TV 4K are shown. See [Requirements](#requirements). When you run the setup again, an Apple
  TV that is already configured is not shown.
- **The pairing does not work**: in the AirPlay settings of the Apple TV, set _Allow Access_ to
  _Anyone on the Same Network_.
- **The Apple TV is not found after a tvOS update**: run the setup again, use _Configure selected
  device_ and choose it in _Apple TV on the network_. The entity ids stay the same.
- **Siri does nothing**: Siri must be on in Settings > General > Siri on the Apple TV. Keep
  _Siri protocol_ at _Auto_.
- **Volume does nothing**: the Apple TV must control the TV or receiver with HDMI CEC. See
  [Known limits](#known-limits).
- **Other errors**: turn on the [debug log](#debug-log) and add the log to a
  [bug report](https://github.com/henrikwidlund/uc-intg-appletv/issues/new/choose).

## Debug log

For an error report, turn on the debug log. Either:

- Turn on _Debug log_ on the _Restore_ page of a new setup.
- Or run the setup again, turn on _Debug log_ and choose _Save settings_.

The driver and ratv then write more lines to the log. It takes effect at once and stays on after a
restart. Turn it off the same way when you are done. `RUST_LOG` overrides this setting.

[Log format](#log-format) explains the log lines.

## Backup and restore

Run the setup again on the Remote and choose _Backup configuration_. The driver shows the data in a
text field. Copy it and keep it in a safe place.

To restore, choose _Restore configuration from backup_ in the setup, or turn on
_Restore from backup_ on the _Restore_ page of a new setup. Paste the data. It replaces all
configured devices.

The backup contains the pairing keys of the Apple TV devices. Do not share it. The driver never
writes the backup data to the log. With `UC_API_MSG_TRACING`, the backup data and the PIN are
shown as `***`.

## Details

This part explains how the driver works inside. Most users do not need it.

### Network and wake-up

The driver uses two protocols of the Apple TV. Companion is the protocol of the iPhone remote.
MRP (Media Remote Protocol) gives most of the playback data.

- With an IP address in the setup, the driver asks that address directly. This also works when the
  Apple TV is in another network (VLAN) and only mDNS is forwarded between the networks.
- If the Apple TV gets a new IP address, the driver finds it on the network and saves the new
  address.
- After the Remote wakes up, the driver connects to the last address first. While the network of the
  Remote starts, it tries again every 50 ms for up to 3 seconds. A connect step without an answer in
  0.5 seconds is started again once.
- A command that comes while the driver connects waits up to 10 seconds for the connection.
  After that, the command is dropped.
- The driver connects Companion first. Commands wait at most 0.5 seconds for MRP. Siri in the
  _MRP only_ mode waits for the MRP connect, also at most 10 seconds.
- If 3 commands time out within 60 seconds, the driver connects again.

### Log format

Log line layout: `[<UTC time> <level> <target>] <message> <fields> <spans>`.
Nested spans are joined with `:`, for example
`device{name="Living Room"}:session{protocol="apple tv" id=1}:session{protocol="mrp" id=3
addr=192.168.1.20:49152}` (one line).

- `device{name=...}`: the Apple TV that the driver works with.
- `setup{ws=...}`: the setup of the Remote connection with this address.
- `session{...}`: the ratv connection, pairing or connect. Each session gets its own `id`, so all
  lines of one connection have the same `id`.

New spans start only at debug level: with the debug log on, or with `RUST_LOG` set to
`info,uc_intg_appletv=debug,ratv=debug`.

A connection keeps the spans it got when it connected. After the debug log is turned on or off, or
after the Apple TV is renamed, its lines show the new spans from the next connect.

### systemd journal

On Linux, the output can go to the systemd journal (`JOURNAL_STREAM` names the stderr of the
driver). Then each log entry starts with a priority prefix such as `<7>`:

- The journal gets the level of each entry, and `journalctl -p warning` shows only warnings and
  errors.
- The second and later lines of a message with more than one line, and the output of a panic, get
  the default priority of the journal.

## Build

```shell
cargo build --release
```

The release build for the Remote uses the [UCR2 toolchain](https://github.com/unfoldedcircle/ucr2-toolchain).
See `.github/workflows/build.yml`. A tag like `v1.2.3` creates a GitHub release with the archive.
A tag with `-`, like `v1.2.3-beta.1`, creates a pre-release.

### Development

How the driver works: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

```shell
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features dhat-heap -- -D warnings
cargo test --locked
cargo deny check
```

The allocation tests use dhat. They count the allocations of all threads, so they run alone:

```shell
cargo test --locked --features dhat-heap dhat_ -- --ignored --test-threads=1
```

To add one, measure with `util::alloc_count::blocks` and set the limit to the measured count.
Name it `dhat_…`, gate it with the `dhat-heap` feature and mark it `#[ignore]`.

ratv is a git dependency with a fixed commit. To test local ratv changes, add this to
`~/.cargo/config.toml` and remove it when you are done:

```toml
[patch."https://github.com/henrikwidlund/ratv"]
ratv = { path = "/path/to/ratv/crates/ratv" }
```

### Device tests

`tools/manual_test.sh` runs the tests that need a person, and the tests that `tools/remote-sim` does
not have yet. It uses a real Apple TV and Remote and runs one step at a time.
It saves the results to `test-results/manual-test-results-<time>.md`.

`tools/remote-sim` tests the driver with a real Apple TV without a Remote. See
[tools/remote-sim/README.md](tools/remote-sim/README.md).

## Links

- [CONTRIBUTING.md](CONTRIBUTING.md): how to help.
- [SECURITY.md](SECURITY.md): how to report a security problem.
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md): rules for the community.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the driver works.
- [tools/remote-sim/README.md](tools/remote-sim/README.md): device tests without a Remote.

## Credits

Parts of the code come from the
[Home Assistant integration](https://github.com/unfoldedcircle/integration-home-assistant) by
Unfolded Circle, which also uses the MPL-2.0 license.

This project is not made or supported by Apple or Unfolded Circle. Apple TV, tvOS and Siri are
trademarks of Apple Inc. Other names are the property of their owners.

## License

[MPL-2.0](LICENSE).
