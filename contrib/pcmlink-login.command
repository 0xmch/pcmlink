#!/bin/sh
# pcmlink-login.command - start the receivers when you log in to the Mac.
#
# Add this file as a login item (System Settings > General > Login Items).
# macOS opens a .command file in Terminal, so the receivers start from a shell
# in your desktop session, with its PATH, exactly as when you run pcmlink-ctl
# by hand. That is what keeps them visible to SoundSource; launchd would not.
#
# Both commands do nothing for a receiver that is already running, so opening
# this file again is harmless.

ctl="$(dirname "$0")/pcmlink-ctl"
"$ctl" start-rx
"$ctl" start-norns-rx
