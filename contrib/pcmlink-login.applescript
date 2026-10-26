-- pcmlink-login - start the receivers when you log in to the Mac.
--
-- Build it once, then add the app as a login item (System Settings > General >
-- Login Items):
--
--   osacompile -o ~/Applications/pcmlink-login.app contrib/pcmlink-login.applescript
--
-- It has Terminal run pcmlink-login.command. Adding the .command file as the
-- login item instead leaves it to whichever app opens .command files, and
-- Ghostty, for one, ignores the file when it is not already running.
--
-- The first run asks for permission to control Terminal.

tell application "Terminal" to do script "~/projects/pcmlink/contrib/pcmlink-login.command; exit"
