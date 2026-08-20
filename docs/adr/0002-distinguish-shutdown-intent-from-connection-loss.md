# Distinguish shutdown intent from connection loss

Managed clients treat an unexplained connection loss as a crash and recover by ensuring a server is running, but an authenticated final server event can identify an intentional manual stop or build replacement. A manual stop disables recovery and cleanly exits attached TUIs, while a replacement makes clients wait for and attach to the new instance; this prevents `suru server stop` from being immediately undone without sacrificing crash recovery.
