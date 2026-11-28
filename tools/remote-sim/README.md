# remote-sim

`tools/remote-sim` tests the driver with a real Apple TV without a Remote. It starts the driver,
connects to it like a Remote and runs the checks. Run the commands from the root of the
repository.

It uses two pairings:

- The driver pairing in `config.json`. `pair-driver` makes this file: it runs the setup of the
  driver and asks for the PIN. An old file is kept as `config.json.old-<time>`.
- A second connection with its own pairing. It reads the values of the Apple TV to compare them
  with the driver. `pair` pairs it.

Each pairing command adds a paired device on the Apple TV.

```shell
cargo build --manifest-path tools/remote-sim/Cargo.toml
tools/remote-sim/target/debug/remote-sim pair-driver 192.168.1.20
tools/remote-sim/target/debug/remote-sim pair 192.168.1.20
tools/remote-sim/target/debug/remote-sim 192.168.1.20
```

Before each run and before `pair-driver`, remote-sim builds the driver with
`cargo build --release --locked` and starts the binary that cargo names. This also works when
`CARGO_TARGET_DIR` is set. A failed build stops the run.

- `--no-build` starts `target/release/uc-intg-appletv` of the repository as it is, also when
  `CARGO_TARGET_DIR` is set. If it is missing, remote-sim stops. With another target folder, use
  `--driver <path>`.
- With `--no-build`, or `--driver` with a binary in a target folder of a repository, remote-sim
  warns if the binary may be older than the source.
- `--driver <path>` starts that binary and does not build. It also wins over `--no-build`.
- If another process uses the port (19190, or `--port`), remote-sim stops before the build.

Before the run, start a movie with chapters and more than one audio or subtitle language in the
Apple TV app.

CI checks the format of the tool, runs clippy and runs its unit tests, but not the checks
against an Apple TV.

## Checks

The checks include:

- Commands, artwork and Siri right after a wake from standby.
- Home while an app plays.
- Playback speeds set on the Apple TV.

When the second connection shows the Home Screen and nothing plays, the checks accept a cleared
title and source on the driver. `standby` requires title, image and source cleared.

Some checks in detail:

- `wake_home` pauses the video and presses Home. Then it checks that the driver shows no old item
  after a wake onto the Home Screen.
- `siri_wake` tests Siri at wake in the _Auto_ mode. Siri over MRP does not work on tvOS 27 and
  later.
- `siri_direct` sends the same audio to Siri through ratv directly, without the driver. This
  compares Siri with and without the driver.
- `third_party` asks you to open YouTube and start a video. It checks that the driver shows its app
  and title. Then it pauses the video, opens Settings and checks that the source stays Settings
  until 4.5 s after the command. It runs only with `--only third_party`.
- `quick_wake` sends standby and wake events with gaps of 0 to 500 ms, so a standby comes while
  the driver connects and a wake comes while the old connection closes. After the last wake the
  driver must connect once, log no driver line at level WARN or ERROR and no closed connection,
  show the media player as available and agree with the second connection if there is one. A
  failed or partial connect logs at WARN.
- `debug_log` starts the driver without `RUST_LOG` and turns the _Debug log_ setup option on and
  off with _Save settings_. It checks that driver and ratv debug lines start and stop. Then it
  starts the driver with the option on. At the end it sets `settings.json` back, also after a
  failure, and starts the driver with `RUST_LOG` again.
- `atv_restart` asks you to restart the Apple TV and measures the reconnect. It runs only with
  `--only atv_restart`.

## Questions on the terminal

Some checks ask on the terminal:

- To start the right video when it is missing.
- To choose a speed on the TV when tvOS does not take it from the second connection.
- Whether Siri answered.

The prompt says what each key does. For the Siri question, s + Enter does not wait for the answer.
The results file has the questions and the answers.
The time of a step includes the time you need to answer.

- Without a terminal, the checks do not ask. The results then say "no terminal".
- On a terminal, remote-sim reads it from the start, so lines typed before a prompt are dropped.
- A run in the background with a terminal stdin stops. Use `< /dev/null` there.

## Siri checks

- The Siri checks use the macOS `say` command to speak "What time is it". The audio ends 300 ms
  after the speech.
- Siri answers some seconds after the audio. Closing the Apple TV connection earlier loses the
  answer. So the Siri checks keep the connections open until you answer, or for 20 s without a
  terminal. Then they close the answer with Back.
- Do not close the answer with Apple's Siri Remote during a run.
- If the TV stays dimmed after a Siri check, press Siri on a remote, then Back twice.
- `--siri-hold` sends 1500 ms of silence after the speech instead of 300 ms, like a user who holds
  the button past the end of listening. On tvOS 27.2.1 this leaves the dimmed Siri overlay on the
  Apple TV. The overlay blocks navigation until Siri and then Back twice are pressed. The option is
  for testing that.

## Choose checks

- `--only a,b` and `--skip a,b` choose the checks. Both can be given more than once. Unknown step
  names are refused.
- For example, `--only quick_wake` runs the quick standby and wake test, and `--skip debug_log`
  leaves out the driver restarts of `debug_log`.
- `--only` also runs `setup`, `entities` and `states`, because the other checks need them. At the
  end it runs `restore_power` and `log`, unless `--skip` names them.
- If `setup`, `entities` or `states` fails, only `restore_power` and `log` run after it.
- With `--skip setup`, `entities` fails, because the driver has no device.
- `--no-power` skips the `power` check. `restore_power` still turns the Apple TV off at the end if
  it was off at the start. To keep the Apple TV on, add `--skip restore_power`.

## Results and log

The results are saved to `test-results/remote-sim-results-<time>.md`.

`RUST_LOG` is added to the driver log filter, and its values win, for example
`RUST_LOG=ratv::mrp=trace`. The checks read debug lines of the driver, so a `RUST_LOG` must keep
`uc_intg_appletv` at debug or trace. Only `debug_log` starts the driver without `RUST_LOG`.

The driver log is in the folder that the run prints. Checks read only the log lines after the last
driver start or after a point in their step. They look for texts only in lines of the driver itself,
not in lines of ratv. `debug_log` also counts the debug lines of ratv. The `log` step reads the
whole file.
