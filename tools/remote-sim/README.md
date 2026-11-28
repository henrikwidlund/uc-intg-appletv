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
cargo build --release
cargo build --manifest-path tools/remote-sim/Cargo.toml
tools/remote-sim/target/debug/remote-sim pair-driver 192.168.1.20
tools/remote-sim/target/debug/remote-sim pair 192.168.1.20
tools/remote-sim/target/debug/remote-sim 192.168.1.20
```

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
- `atv_restart` asks you to restart the Apple TV and measures the reconnect. It runs only with
  `--only atv_restart`.

## Questions on the terminal

Some checks ask on the terminal:

- To start the right video when it is missing.
- To choose a speed on the TV when tvOS does not take it from the second connection.
- Whether Siri answered.

The prompt says what each key does. The results file has the questions and the answers.
The time of a step includes the time you need to answer.

- Without a terminal, the checks do not ask.
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
`uc_intg_appletv` at debug or trace.
