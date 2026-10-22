# Profile upgrades and restoring an older Dakia version

Dakia supports the covered forward upgrade from the previous stable release,
published v0.4.4, to the current candidate schema. The current CLI acceptance
preserved the fictional disabled account and its unread, starred message body
and identity. A source regression separately preserves credential and action
state. This is the supported forward path for that predecessor only. It does
not establish a packaged GUI upgrade or compatibility with older releases.

Going back to an older Dakia version after its profile has been upgraded is
unsupported. The published v0.4.4 app is known not to reject a profile carrying
a newer schema marker. Do not open an upgraded profile with v0.4.4.

Before installing a newer Dakia version:

1. Quit Dakia and the `dakia` CLI.
2. Back up the entire current profile directory. On macOS, the default shared
   desktop and CLI profile is `~/Library/Application Support/dev.dakia.mail`.
   If you intentionally use `DAKIA_DATA_DIR`, back up that selected directory
   instead.
3. Copy the whole directory, including `dakia.db` and any sidecar files, to a
   dated location outside the active profile directory.
4. Install and launch the newer version.

To return to the earlier version after an upgrade:

1. Quit the newer Dakia app and CLI.
2. Move the current upgraded profile directory aside. Do not merge database
   files from two versions.
3. Restore the complete pre-upgrade backup to the same profile location.
4. Launch the earlier Dakia version only after that restore is complete.

If no pre-upgrade backup exists, do not open the upgraded profile with an older
Dakia version. Continue with the newer version or obtain a compatible recovery
path before changing the profile again.

The [current CLI acceptance](mail-safety-results/v044-forward-cli-upgrade-2026-10-21.json)
covers a fictional v0.4.4 forward upgrade, account and message verification,
and byte-verified restoration from a complete pre-upgrade backup. The separate
isolated package evidence also covers restoring the old app bundle. Neither
result establishes a packaged GUI upgrade, simultaneous desktop and CLI use,
automatic rollback after replacement begins, or a safe profile downgrade. The
backup restoration procedure above is the approved path after an unsupported
downgrade attempt.
