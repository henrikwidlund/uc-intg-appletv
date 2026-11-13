# Apple TV integration for Remote Two/3 (Rust)

Integration driver for the [Unfolded Circle](https://www.unfoldedcircle.com/) Remote Two and Remote 3.
It controls Apple TV devices on the local network with the [ratv](https://github.com/henrikwidlund/ratv) library.

The driver id is `appletv_rs`, so it can run next to the Apple TV integration that comes with the Remote.

## Features

- **Media player**: power, playback, seek, fast forward and rewind, volume and mute, repeat and shuffle,
  audio track and subtitles, navigation, app list, audio outputs, browse (apps and Up Next).
  Title, artist, album, artwork, position and media type.
- **Remote**: buttons, hold, repeat and button sequences.
- **Selects**: app, audio output, audio language, subtitles, account, chapter, Up Next, playback speed,
  shuffle and repeat.
- **Sensors**: app, audio output, audio language, subtitles, playback speed, shuffle and repeat (the same
  values as the selects), episode, genre, content rating, audio format, live content and volume control.
- **Voice assistant**: Siri with the voice button of the Remote.

One pairing with one PIN covers all features.

## Compared to the built-in Apple TV integration

- One PIN for the pairing instead of two.
- Siri with the voice button.
- Repeat, shuffle, audio track, subtitles, browse and play media on the media player.
- Mute and unmute as separate commands. The built-in integration has only mute toggle.
- More selects and sensors. The built-in integration has only the app and the audio output.
- Updates come only when the Apple TV sends them. There is no poll every 10 seconds.
- Entities keep their state while the Remote sleeps. They do not show as unavailable when it wakes up.

## Network and wake-up

- With an IP address in the setup, the driver asks that address directly. This also works when the
  Apple TV is in another network (VLAN) and only mDNS is forwarded between the networks.
- If the Apple TV gets a new IP address, the driver finds it on the network and saves the new address.
- After the Remote wakes up, the driver connects to the last address first. While the network of the
  Remote starts, it tries again every 50 ms for up to 3 seconds. A connect step without an answer in
  0.5 seconds is started again once.
- A command that comes while the driver connects waits for the connection. It does not fail.
- If 3 commands time out within 60 seconds, the driver connects again.

## Requirements

- Apple TV HD (4th generation) or Apple TV 4K (all generations). Older models are not shown in the setup.
  Tested with an Apple TV 4K (1st generation) on tvOS 26.6.1.
- The Apple TV and the Remote are on the same network or use a mDNS reflector. Multicast DNS (mDNS) must not be blocked.
- On the Apple TV, in the AirPlay settings: set _Allow Access_ to _Anyone on the Same Network_.
- For the voice assistant, Siri must be on in the Apple TV settings: Settings > General > Siri.
- A fixed IP address for the Apple TV (DHCP reservation) is recommended. The driver then finds the Apple TV
  faster after a restart.

## Known limits

- Volume control works only if the Apple TV can change the volume of the TV or receiver (HDMI CEC).
  Otherwise the volume controls stay on the Remote, but they return an error, and the _Volume control_
  sensor shows off.
- The Apple TV does not report the app in front. The app select shows the app opened through the driver or
  the app that plays media.
- The Apple TV does not report the current account. The account select shows the last account chosen
  through the driver.
- Metadata, artwork and playback speeds depend on the app. Many apps send only some of them.
  For example, the Apple TV app sends genre and content rating, and season and episode for shows.
  The Music app sends the genre.
- The playback speed select is empty when the app does not report speeds.
- Fast-forward and rewind depend on the app. In Disney+ they skip forward and back instead.
  HBO Max and Netflix do not support them. Skip works in all three.

## Install on the Remote

1. Download `uc-intg-appletv-rs-<version>-aarch64.tar.gz` from the [releases](https://github.com/henrikwidlund/uc-intg-appletv/releases).
2. Web configurator: Integrations > Add new > Install custom. Choose the file.
3. Start the setup of _Apple TV (Rust)_. Leave the IP address empty to search the network.
4. Choose the Apple TV and enter the PIN that the TV shows.

Start the setup again to add, remove, configure or pair a device again.
If the Apple TV is not found after a tvOS update, use _Configure selected device_ and choose it in
_Apple TV on the network_. The entity ids stay the same.

## Run on a computer

The driver can also run on a computer as an external integration. The Remote finds it with mDNS.

Each release has binaries for Linux, macOS and Windows, for amd64 and arm64. The Linux binaries are static
and run on any distribution.

```shell
cargo run --release
```

| Variable                   | Default             | Description                                                            |
|----------------------------|---------------------|------------------------------------------------------------------------|
| `UC_CONFIG_HOME`           | working directory   | Folder for the configuration and cache files.                          |
| `UC_INTEGRATION_INTERFACE` | `0.0.0.0`           | Network interface to listen on.                                        |
| `UC_INTEGRATION_HTTP_PORT` | `9090`              | WebSocket and artwork port.                                            |
| `UC_DISABLE_MDNS_PUBLISH`  | `false`             | Do not announce the driver with mDNS.                                  |
| `UC_SETUP_TIMEOUT`         | `300`               | Setup timeout in seconds. `0` disables it.                             |
| `UC_CLIENT_NAME`           | host name           | Name shown on the Apple TV for the pairing.                            |
| `UC_API_MSG_TRACING`       | off                 | Log WebSocket messages: `all`, `in` or `out`. Can contain PINs.        |
| `RUST_LOG`                 | `info`              | Log level, for example `info,uc_intg_appletv=debug`.                   |

`config.json` contains the pairing keys. Only the owner can read it.

### Docker

```shell
docker run -d --name uc-intg-appletv --network host \
  -v uc-intg-appletv:/data henrikwidlund/uc-intg-appletv
```

- The image runs as user `65532`. A named volume works as it is. For a folder on the host, give that user write access.
- `--network host` lets the driver find the Apple TV and announce itself with mDNS. It works on Linux only.
- Without host networking, publish the port with `-p 9090:9090` and add `-e UC_DISABLE_MDNS_PUBLISH=true`.
  Add the driver to the Remote with the IP address of the computer, and enter the IP address of the
  Apple TV in the setup.
- The image has no shell. To change a setting, use the variables in the table above.

## Debug log

For an error report, turn on the debug log: tick _Debug log_ on the _Restore_ page of a new setup, or run the
setup again, tick _Debug log_ and choose _Save settings_.
The driver and ratv then write more lines to the log. It takes effect at once and stays on after a restart.
Turn it off the same way when you are done. `RUST_LOG` overrides this setting.

_Siri protocol_ is on the same pages. Keep it at _Auto_: Siri then uses the same protocol as the iPhone remote.
_MRP only_ is only for finding errors.

## Backup and restore

Run the setup again on the Remote and choose _Backup configuration_. The driver shows the data in a text field.
Copy it and keep it in a safe place.

To restore, choose _Restore configuration from backup_ in the setup, or tick _Restore from backup_ on the
_Restore_ page of a new setup. Paste the data. It replaces all configured devices.

The backup contains the pairing keys of the Apple TV devices. Do not share it. The driver never writes the backup
data to the log. With `UC_API_MSG_TRACING`, the messages with backup data are hidden.

## Build

```shell
cargo build --release
```

The release build for the Remote uses the [UCR2 toolchain](https://github.com/unfoldedcircle/ucr2-toolchain).
See `.github/workflows/build.yml`. A `v*` tag creates a GitHub release with the archive.

### Development

```shell
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo deny check
```

`tools/manual_test.sh` runs the tests with a real Apple TV and Remote, one step at a time.
It saves the results to `manual-test-results-<time>.md`.

`tools/remote-sim` tests the driver with a real Apple TV without a Remote. It starts the driver,
connects to it like a Remote and runs the checks that need no person. It uses the pairing in
`config.json` of an earlier run of the driver. A second connection with its own pairing reads the
values of the Apple TV to compare them with the driver. Pair it one time with a PIN:

```shell
cargo build --release
cargo build --manifest-path tools/remote-sim/Cargo.toml
tools/remote-sim/target/debug/remote-sim pair 192.168.1.20
tools/remote-sim/target/debug/remote-sim 192.168.1.20
```

Start a video with chapters and more than one audio language before the run to also test
playback and languages. `--only a,b` and `--skip a,b` choose the checks, `--no-power` keeps the
Apple TV on. The results are saved to `remote-sim-results-<time>.md`.

ratv is a git dependency with a fixed commit. To test local ratv changes, add this to
`~/.cargo/config.toml` and remove it when you are done:

```toml
[patch."https://github.com/henrikwidlund/ratv"]
ratv = { path = "/path/to/ratv/crates/ratv" }
```

## License

[MPL-2.0](LICENSE). Parts of the code come from the
[Home Assistant integration](https://github.com/unfoldedcircle/integration-home-assistant) by Unfolded Circle,
which uses the same license.
