#!/bin/sh
# pcmlink-login.command - start the receivers when you log in to the Mac.
#
# Run at login by pcmlink-login.applescript, which has Terminal run this file,
# so the receivers start from a shell in your desktop session, with its PATH,
# exactly as when you run pcmlink-ctl by hand. That is what keeps them visible
# to SoundSource; launchd would not.
#
# Both commands do nothing for a receiver that is already running, so running
# this again is harmless. What happened at the last login is in the log.

ctl="$(dirname "$0")/pcmlink-ctl"
log="$HOME/Library/Logs/pcmlink/login.log"
mkdir -p "$(dirname "$log")"
{ date; "$ctl" start-rx; "$ctl" start-norns-rx; } 2>&1 | tee -a "$log"
