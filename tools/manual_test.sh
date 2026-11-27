#!/usr/bin/env bash
# Copyright (c) 2026 Henrik Widlund
# SPDX-License-Identifier: MPL-2.0
#
# Guided manual test with a real Remote and Apple TV. It has the steps that need a person, and the
# steps that tools/remote-sim does not check yet. Run remote-sim for the rest.
#
# The script shows one numbered step at a time: what to prepare, what to do and what to watch
# for. Enter runs the step command (if the step has one). You then type what happened. All
# answers are saved to a results file.
#
# The driver runs in one of two ways:
# - script: the script builds the driver, starts it and writes the log to a file.
# - IDE: the driver runs in RustRover (or another IDE). The script asks for the log file.
#
# Usage: tools/manual_test.sh [steps]
#   No steps: run all steps.
#   Steps: numbers and ranges, for example: 12 14-16 20
#   A number with + runs that step and all steps after it, for example: 30+

set -u

# Bash reads a script while it runs, so a change to the file breaks a running test.
# The braces make bash read the whole script first.
{

cd "$(dirname "$0")/.." || exit 1

mkdir -p test-results
RESULTS="$PWD/test-results/manual-test-results-$(date +%Y%m%d-%H%M%S).md"
BIN="$PWD/target/release/uc-intg-appletv"
DRIVER_PID=""
SELECTION="$*"
STEP=0
# Debug lines of the driver show artwork, outputs and languages.
DRIVER_LOG_LEVEL="info,uc_intg_appletv=debug"

echo "Where does the driver run?"
echo "  1 + Enter = this script builds and starts it"
echo "  2 + Enter = RustRover or another IDE"
read -r -p "Choice: " mode
if [[ "$mode" = "2" ]]; then
    IDE=1
    echo
    echo "In the RustRover run configuration:"
    echo "  - Environment variables: RUST_LOG=$DRIVER_LOG_LEVEL"
    echo "  - Logs tab: turn on 'Save console output to file' and choose a file."
    echo "  - The working directory is UC_CONFIG_HOME if it is not set."
    read -r -p "Config folder of the driver (UC_CONFIG_HOME), Enter = $PWD: " CONFIG_DIR
    CONFIG_DIR="${CONFIG_DIR:-$PWD}"
    read -r -p "Log file from 'Save console output to file', Enter = no file: " LOG
else
    IDE=0
    # Same default as the driver in RustRover, so both find the same pairing.
    read -r -p "Config folder of the driver (UC_CONFIG_HOME), Enter = ${UC_CONFIG_HOME:-$PWD}: " CONFIG_DIR
    CONFIG_DIR="${CONFIG_DIR:-${UC_CONFIG_HOME:-$PWD}}"
    LOG="$CONFIG_DIR/driver.log"
    mkdir -p "$CONFIG_DIR"
    touch "$LOG"
fi
# recent_log shows only lines written after this point.
LOG_START=0
if [[ -n "$LOG" ]] && [[ -f "$LOG" ]]; then
    LOG_START=$(wc -c <"$LOG" | tr -d ' ')
fi

# True if step $1 is in the selection.
selected() {
    local n="$1" s
    [[ -z "$SELECTION" ]] && return 0
    for s in $SELECTION; do
        case "$s" in
            *+) [[ "$n" -ge "${s%+}" ]] && return 0 ;;
            *-*) [[ "$n" -ge "${s%-*}" ]] && [[ "$n" -le "${s#*-}" ]] && return 0 ;;
            *) [[ "$n" -eq "$s" ]] && return 0 ;;
        esac
    done
    return 1
}

stop_driver() {
    if [[ -n "$DRIVER_PID" ]] && kill -0 "$DRIVER_PID" 2>/dev/null; then
        kill "$DRIVER_PID"
        wait "$DRIVER_PID" 2>/dev/null
        echo "Driver stopped."
    fi
    DRIVER_PID=""
}

start_driver() {
    if [[ "$IDE" = 1 ]]; then
        read -r -p "Restart the driver in RustRover now, then press Enter: " _
        return
    fi
    stop_driver
    UC_CONFIG_HOME="$CONFIG_DIR" RUST_LOG="${RUST_LOG:-$DRIVER_LOG_LEVEL}" "$BIN" >>"$LOG" 2>&1 &
    DRIVER_PID=$!
    sleep 1
    if kill -0 "$DRIVER_PID" 2>/dev/null; then
        echo "Driver started (pid $DRIVER_PID). Log: $LOG"
        grep "Listening on" "$LOG" | tail -n 1
    else
        echo "Driver did not start. Last log lines:"
        tail -n 20 "$LOG"
        DRIVER_PID=""
    fi
}

fake_ids() {
    # Replace the stored device ids with ids that no device has, as after a tvOS update.
    if [[ "$IDE" = 1 ]]; then
        read -r -p "Stop the driver in RustRover now, then press Enter: " _
    else
        stop_driver
    fi
    cp "$CONFIG_DIR/config.json" "$CONFIG_DIR/config.json.before-link-test"
    python3 -I - "$CONFIG_DIR/config.json" <<'PY'
import json, sys
path = sys.argv[1]
with open(path) as f:
    devices = json.load(f)
for d in devices:
    d["id"] = "00:00:00:00:00:01"
    d["known_ids"] = ["00:00:00:00:00:01", "00000000-0000-0000-0000-000000000001"]
with open(path, "w") as f:
    json.dump(devices, f, indent=2)
PY
    echo "Stored ids replaced. Backup: $CONFIG_DIR/config.json.before-link-test"
    start_driver
}

recent_log() {
    # Log lines of the driver from this run, without the HTTP server lines.
    if [[ -z "$LOG" ]] || [[ ! -f "$LOG" ]]; then
        echo "No log file. Look in the RustRover console for lines with: ${1:-any text}"
        return
    fi
    tail -c "+$((LOG_START + 1))" "$LOG" | grep -v "actix_server" | grep -E "${1:-.}" | tail -n "${2:-15}"
}

trap stop_driver EXIT

save() {
    printf '%s\n' "$@" >>"$RESULTS"
}

# step TITLE PREPARE DO WATCH [COMMAND]
step() {
    STEP=$((STEP + 1))
    local title="$1" prepare="$2" todo="$3" watch="$4" cmd="${5:-}"
    if ! selected "$STEP"; then
        return
    fi
    while true; do
        echo
        echo "================================================================"
        echo "Step $STEP: $title"
        echo "================================================================"
        echo "Prepare:"
        echo "$prepare" | sed 's/^/  /'
        echo "Do:"
        echo "$todo" | sed 's/^/  /'
        echo "Watch for:"
        echo "$watch" | sed 's/^/  /'
        if [[ -n "$cmd" ]]; then
            echo "Command:"
            echo "  $cmd"
        fi
        echo
        if [[ -n "$cmd" ]]; then
            read -r -p "Enter = run the command, s + Enter = skip this step, q + Enter = quit: " key
        else
            read -r -p "Enter = start this step, s + Enter = skip this step, q + Enter = quit: " key
        fi
        case "$key" in
            q | Q)
                save "" "Stopped at step $STEP."
                echo "Results: $RESULTS"
                exit 0
                ;;
            s | S)
                save "" "## $STEP. $title" "" "- Result: SKIPPED"
                return
                ;;
            *)
                # Enter or any other key: run the step.
                ;;
        esac
        if [[ -n "$cmd" ]]; then
            echo "----- command output -----"
            eval "$cmd"
            echo "--------------------------"
        fi
        echo
        read -r -p "Result: y + Enter = as expected, n + Enter = not as expected, r + Enter = run the step again: " answer
        case "$answer" in
            r | R) continue ;;
            y | Y)
                read -r -p "Note (optional), then Enter: " note
                save "" "## $STEP. $title" "" "- Result: OK"
                ;;
            *)
                read -r -p "What happened? Type it, then Enter: " note
                save "" "## $STEP. $title" "" "- Result: NOT OK"
                ;;
        esac
        if [[ -n "${note:-}" ]]; then
            save "- Note: $note"
        fi
        note=""
        return
    done
}

echo "Manual test of the Apple TV integration."
echo "Results are saved to: $RESULTS"
echo "Driver configuration: $CONFIG_DIR"
echo "Driver log: ${LOG:-RustRover console}"
echo
# The answers are kept for the next run. Enter keeps the last answer.
SETUP_FILE="$PWD/.manual-test-setup"
atv="" remote="" audio=""
if [[ -f "$SETUP_FILE" ]]; then
    { read -r atv; read -r remote; read -r audio; } <"$SETUP_FILE"
fi
read -r -p "Apple TV model and tvOS version, Enter = '${atv:-unknown}': " answer
atv="${answer:-${atv:-unknown}}"
read -r -p "Remote model and firmware version, Enter = '${remote:-unknown}': " answer
remote="${answer:-${remote:-unknown}}"
read -r -p "Audio setup (for example HDMI with CEC, HDMI without CEC, AirPlay speakers), Enter = '${audio:-unknown}': " answer
audio="${answer:-${audio:-unknown}}"
printf '%s\n' "$atv" "$remote" "$audio" >"$SETUP_FILE"

save "# Manual test $(date '+%Y-%m-%d %H:%M')" "" \
    "- Driver: $(git describe --always --dirty 2>/dev/null || echo unknown)" \
    "- ratv: $(git -C ../ratv describe --always --dirty 2>/dev/null || echo unknown)" \
    "- Apple TV: $atv" \
    "- Remote: $remote" \
    "- Audio: $audio"

# ---------------------------------------------------------------- driver and setup

# Step 1 starts the driver. When step 1 is not selected, start it here.
if [[ "$IDE" = 0 ]] && ! selected 1; then
    echo
    echo "Building and starting the driver..."
    cargo build --release && start_driver
fi

if [[ "$IDE" = 1 ]]; then
    step "Start the driver" \
"The Mac and the Remote are on the same network as the Apple TV.
On the Apple TV: Settings > AirPlay and HomeKit > Allow Access = Anyone on the Same Network." \
"Start the driver in RustRover with the run configuration from above." \
"The log shows 'Listening on: 0.0.0.0:9090' and 'Registered mDNS service'." \
"recent_log 'Listening on|mDNS|WARN|ERROR'"
else
    step "Build and start the driver" \
"The Mac and the Remote are on the same network as the Apple TV.
On the Apple TV: Settings > AirPlay and HomeKit > Allow Access = Anyone on the Same Network." \
"Press Enter. The script builds the driver and starts it in the background." \
"The build ends without errors.
The output shows 'Driver started' and 'Listening on: 0.0.0.0:9090'." \
"cargo build --release && start_driver"
fi

step "Find the driver on the Remote" \
"Open the web configurator of the Remote." \
"Integrations > Add new > look for 'Apple TV (Rust)'." \
"The driver is listed. If it is not listed, the mDNS publish failed: check the log." \
"recent_log 'mDNS|WARN|ERROR'"

step "Setup: discovery and pairing" \
"The Apple TV is on (not sleeping). You can see the TV." \
"Select 'Apple TV (Rust)' and start the setup. Press Next on the info page.
On the page 'Restore': leave 'Restore from backup' unticked, keep 'Siri protocol' at Auto. Press Next.
On the page 'Setup mode': leave the IP address empty. Press Next.
Select the Apple TV. Keep 'Change volume on all connected devices' on. Press Next.
Enter the PIN shown on the TV. Press Next." \
"A page 'Please choose your Apple TV' with the Apple TV in a dropdown: name and IP address.
A PIN shows on the TV after Next.
After the PIN, the setup ends with success.
The log shows 'paired with' and then 'connected'." \
"recent_log 'discovering|not found|pairing|paired|connected|WARN|ERROR' 20"

step "Add the entities" \
"Setup is done." \
"Add all entities of the Apple TV: media player, remote, Siri, selects and sensors.
Open the media player and the remote on the Remote." \
"The entities show on the Remote pages. The media player shows a state (for example On,
Standby or Playing)." \
"recent_log 'missing|not connected|WARN|ERROR'"

# ---------------------------------------------------------------- buttons

step "Buttons and commands on the TV" \
"Apple TV on the Home Screen, an app icon selected. For the swipes: the focus in the middle of
the app grid (3 rows down and 2 columns right). At the end of a list, a swipe moves a bit and then
goes back: that is normal." \
"Press up, down, left, right, the middle button (OK), BACK and HOME on the Remote.
Press the MENU button on the Remote (context menu). Press BACK.
Run the Control Center command from the remote entity page (or media player 'menu'). Press BACK.
Remote entity, Main page: press App Switcher. Press BACK. Press Screensaver. Press BACK.
Press Guide in an app with a guide (for example the TV app with live TV).
Press HOME. Move the focus 3 rows down and 2 columns right in the app grid.
Run SWIPE_UP, SWIPE_DOWN, SWIPE_LEFT and SWIPE_RIGHT (simple commands of the media player)." \
"Each press moves once or selects. BACK goes back. HOME goes to the Home Screen.
MENU opens the context menu of the app icon (like holding Select for 1 s).
Control Center opens. App Switcher shows the open apps. The screensaver starts.
Guide opens the guide in the app, or does nothing in apps without a guide.
Each swipe moves the focus in its direction, and the focus stays there.
All four directions move about the same distance."

step "Held buttons and repeat" \
"Apple TV on the Home Screen with many apps." \
"Hold DPAD down for about 3 s. Then hold DPAD right for about 3 s." \
"The cursor moves step by step while the button is held.
It does not jump 4 steps at a time."

# ---------------------------------------------------------------- now playing

step "Now playing: Music" \
"Start a song in the Apple Music app." \
"Look at the media player on the Remote for 30 s." \
"Artist, album and cover art are shown.
The position moves forward without jumps. Duration is correct."

step "Now playing: TV show" \
"Start an episode of a TV show in the TV app. Then repeat with an episode in another app
(for example Disney+ or HBO Max)." \
"Look at the media player and these sensors (added in step 'Add the entities'):
'<device name> Episode' (sensor.<key>.episode), '<device name> Genre',
'<device name> Content rating' and '<device name> Audio format'." \
"Title is the episode, artist is the show name, media type is TV show.
The Episode sensor shows for example 'Show - S2E5'.
TV app: Genre and Content rating show a value. Audio format is only sent by Music.
Other apps: the sensors show a value if the app sends one, else they are empty.
Then play a movie in the TV app: title is the movie, Genre and Content rating show a value.
In the note: for each app, the title shown, and if it is not the episode title, what it is."

step "Artwork without URL" \
"Start content in an app that sends only image data (many third party apps).
The log needs debug lines (RUST_LOG from the start of the script)." \
"Look at the cover art on the Remote for 30 s. Then go back and open the media player again." \
"Cover art is shown and stays. When the program changes (live TV), the new image is shown.
Telia: change between channels, also back to a channel seen before. Each channel shows its image.
A channel without an image shows no image, not the image of the last channel.
The log shows 'artwork url' (the image has a URL), or 'from Companion' (the image came with now playing)
or 'fetching artwork', and then 'artwork file'.
The file URL stays the same for the same image.
Telia: a channel seen before with a logo shows the logo again, also when the TV sends none
(log: 'using the cached image').
If the image goes black: copy the artwork lines and the lines around them into the note." \
"recent_log 'artwork' 20"

step "Live content" \
"Start a live stream (for example a live channel in Disney+ or the TV app)." \
"Look at the media player and the binary sensor '<device name> Live' (sensor.<key>.is_live)." \
"No wrong duration (a duration of 0 is not shown as a full program).
The Live sensor is on (apps that do not report live: on when there is no duration).
Then play a movie: the Live sensor is off."

# ---------------------------------------------------------------- playback

step "Next, previous and stop" \
"Music is playing in Apple Music." \
"Media player: next, previous, stop." \
"Each command works once. Next and previous change the song.
Stop stops or pauses playback."

step "Play/pause from the screensaver" \
"Pause a video, then start the screensaver (Screensaver command)." \
"Press play/pause on the Remote." \
"The screensaver closes and the video plays again."

step "Skip forward and backward" \
"Play a video in the TV app. Then repeat in Disney+ or Netflix." \
"Run SKIP_FORWARD and SKIP_BACKWARD (simple commands)." \
"The video moves by the skip time of the app (for example 10 s or 15 s), in both apps."

step "Fast forward and rewind with the NEXT and PREV buttons" \
"Play a video in the TV app. Open the remote entity (not the media player)." \
"Press NEXT one time. Wait 3 s. Press play/pause.
Press PREV one time. Wait 3 s. Press play/pause." \
"NEXT starts fast forward, PREV starts rewind. Play/pause ends it.
The Remote sends no event when a button is released, so holding a button is not tested."

step "Fast forward until stop" \
"Play a video in the TV app." \
"Media player: press fast forward one time. Wait 5 s. Press play/pause." \
"Fast forward continues until play/pause. Play/pause ends fast forward."

step "Fast forward in apps without fast forward" \
"Play a video in Disney+, Netflix or HBO Max." \
"Media player: press fast forward one time, then rewind one time." \
"Fast forward skips forward one time, rewind skips back one time.
No continuous fast forward or rewind."

step "Seek" \
"Play a video." \
"Open the media player on the Remote. Drag the position bar to the middle.
If the bar cannot be moved, also try with the Python Apple TV integration." \
"The bar can be moved. The position on the Remote matches the TV.
In the note: does the bar move at all, and does the Python integration allow it?" \
"recent_log 'seek|WARN|ERROR'"

# ---------------------------------------------------------------- volume

step "Volume" \
"Note your audio setup. With HDMI without CEC, the volume controls stay but return an error." \
"Press VOLUME UP, VOLUME DOWN and MUTE on the Remote. Move the volume slider of the media player." \
"With volume control: the volume changes, mute toggles, the slider sets the level.
Without volume control: the Volume control sensor is off. The volume controls stay. A command shows
'Volume control is not available on this Apple TV'." \
"recent_log 'volume|WARN|ERROR'"

step "Volume with AirPlay speakers" \
"Group the Apple TV with an AirPlay speaker on the TV (Control Center > AirPlay)." \
"Move the volume slider of the media player." \
"With 'Change volume on all connected devices' on: all outputs change.
The volume shown is the average of the outputs."

# ---------------------------------------------------------------- selects and browse

step "App select and source list" \
"Apple TV on the Home Screen." \
"Media player: open the source list (the 'Source' input of the media player) and choose an app.
Use select next and select previous on the App select." \
"Each choice opens the app. The media player source shows it.
Pause a video and press Home: after about 2 s the app, image and media info are empty.
Open the same app again: the paused video shows again.
Play music and press Home: music info stays.
Pause in an app, skip forward: nothing is cleared."

step "Audio language and subtitles" \
"Play a movie with more than one audio language and subtitles (for example in the TV app)." \
"Media player: run audio track and subtitle commands. Look at the Audio language sensor." \
"The TV changes the audio and subtitles. The sensor shows the current audio language.
The subtitle command goes through all subtitles, then Off.
If the app has automatic subtitles, the Subtitles select has 'Auto' and marks it when it is on.
If a select or the sensor shows the wrong option: copy the 'audio:' log lines into the note." \
"recent_log 'audio:' 10"

step "Chapters" \
"Play a movie with chapters." \
"Chapter select: choose a later chapter. Then use NEXT_CHAPTER and PREVIOUS_CHAPTER." \
"The video moves to the chapter. The select shows the current chapter, also when the
video plays into the next chapter by itself."

step "Up Next and browse" \
"Play an album in Apple Music, so Up Next has items." \
"Up Next select: choose an item. Then browse the media player: open Apps and Up Next,
play an item from Up Next, and launch an app from Apps." \
"The chosen item plays. Browse shows Apps and Up Next. Play works from browse."

step "Shuffle and repeat" \
"Play an album in Apple Music." \
"Media player: turn shuffle on and off, set repeat to all, one and off.
Then use the Shuffle and Repeat selects." \
"The TV changes the mode. The media player, selects and sensors show the new mode."

step "Accounts" \
"The Apple TV has more than one user account." \
"Account select: choose another account." \
"The Apple TV changes the user. The select shows the chosen account."

step "Audio output" \
"An AirPlay speaker, or a Mac with AirPlay Receiver on, is on the network.
Wait 1 min after the driver connects, so the output scan is done." \
"Audio output select: choose the speaker, then the Apple TV alone." \
"Audio goes to the chosen output. The select shows the current output combination.
Each option starts with the Apple TV name, for example 'Apple TV 4K, MacBook Pro'.
If not: copy the 'outputs:' and 'SetOutputs' log lines into the note." \
"recent_log 'outputs:|SetOutputs|AirPlay|WARN|ERROR' 20"

# ---------------------------------------------------------------- Siri

step "Siri" \
"Apple TV on the Home Screen. The Siri entity is added to the Remote." \
"Hold the voice button on the Remote and say 'What is the weather today'. Release." \
"Siri shows on the TV and answers.
The Remote shows that the voice command ended (no error).
The log shows 'mode Auto'." \
"recent_log 'Siri|voice|WARN|ERROR'"

step "Siri off on the Apple TV" \
"Turn Siri off on the Apple TV: Settings > General > Siri." \
"Hold the voice button on the Remote and say something. Release.
Then turn Siri on again on the Apple TV." \
"The Remote shows an error right away: 'Siri is off on the Apple TV'. No listening animation stays.
The log shows 'Siri is off' when you turn it off, and 'Siri failed: Siri is off on the device'." \
"recent_log 'Siri|voice|WARN|ERROR'"

step "Siri with MRP only" \
"Run the setup again. Set 'Siri protocol' to 'MRP only' and choose 'Save settings'." \
"Hold the voice button on the Remote and say 'What is the weather today'. Release.
Then set 'Siri protocol' back to Auto." \
"The log shows 'mode MRP only'. Note in the answer if Siri answered on the TV.
On tvOS 27 this can fail: Siri records but does not answer." \
"recent_log 'Siri|voice|WARN|ERROR'"

# ---------------------------------------------------------------- connection

step "Apple TV restart and reconnect" \
"Media player state is shown. The Apple TV is connected." \
"Restart the Apple TV: Settings > System > Restart (or unplug it). Wait until it is on again
(about 1 min). Then press Enter here to show the log." \
"Entities become unavailable. They are available again within about 30 s after the Apple TV
has started.
The log shows 'reconnect in' with longer waits while the Apple TV is off (2s, 4s, 6s ...),
then 'connected'." \
"recent_log 'reconnect in|connected|closed|not found|WARN|ERROR' 25"

# ---------------------------------------------------------------- reconfigure

step "Reconfigure: pair again" \
"Look at the TV." \
"Reconfigure, choose the device and 'Pair selected device again'. Press Next.
When the PIN page shows, type one space as the PIN and press Next.
Then enter the PIN from the TV and press Next.
Then press Enter here to show the log." \
"A PIN shows on the TV. The space (an empty PIN) shows the PIN page again with an error.
The real PIN ends the setup with success. The log shows 'connected' after it.
The entities keep their ids (activities still work).
If no PIN shows: copy the log lines into the note." \
"recent_log 'pairing|paired|not found|discovery|invalid setup input|connected|WARN|ERROR' 20"

step "Wrong credentials" \
"On the Apple TV: Settings > Remotes and Devices > Remote App and Devices: remove this driver
(the name shown is the host name of the Mac)." \
"Restart the Apple TV or wait until the connection drops." \
"The driver stops reconnecting. The log shows 'pairing is not valid anymore'.
Then pair again now (repeat step 'Reconfigure: pair again'). The next steps need the connection." \
"recent_log 'pairing|auth|WARN|ERROR' 20"

# ---------------------------------------------------------------- identity

step "Link: the device ids change" \
"The driver runs and the Apple TV is connected." \
"Press Enter. The script stops the driver, replaces the stored device ids with ids that no
device has (as after a tvOS update), and starts the driver again. Wait 30 s." \
"The entities become unavailable. The log shows 'not found on the network'." \
"fake_ids; sleep 30; recent_log 'not found|connected|WARN|ERROR'"

step "Link: choose the device again" \
"Step 'Link: the device ids change' is done." \
"Start the setup of the integration again (reconfigure). Choose the device and
'Configure selected device'. Press Next.
In 'Apple TV on the network', choose the Apple TV. Press Next.
Then press Enter here to show the log." \
"Setup ends with success. The log shows 'linked to' and then 'connected'.
The entities are available again with the same entity ids (activities still work)." \
"recent_log 'linked to|configuration changed|connected|not found|WARN|ERROR' 20"

# ---------------------------------------------------------------- more

step "Siri two times in a row" \
"The Apple TV is on." \
"Hold the voice button and say: What is the weather. Release it.
Right away, hold it again and say: Open Settings. Release it.
Then press Enter here to show the log." \
"Both commands work. The log has no 'Siri failed' line (for example SiriActive)." \
"recent_log 'Siri|WARN|ERROR'"

step "Chapter select during playback" \
"A video with chapters plays (for example a movie in the Apple TV app)." \
"Watch the chapter select in the web configurator while the video plays.
Seek over a chapter start." \
"The current chapter changes at the chapter start and after the seek.
The chapter list stays the same." \
"recent_log 'WARN|ERROR'"

# ---------------------------------------------------------------- end

step "Check the log for errors" \
"The steps above are done." \
"Press Enter to show warnings and errors from the driver log." \
"Each WARN or ERROR line is about a step above where something did not work.
Steps 'Apple TV restart and reconnect', 'Wrong credentials' and 'Siri off on the Apple TV' can
cause WARN lines on purpose.
In the note: copy lines that you cannot connect to a step." \
"recent_log 'WARN|ERROR' 40"

save "" "Done."
echo
echo "All steps done. Results: $RESULTS"
exit
}
