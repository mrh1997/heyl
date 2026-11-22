# Changelog

- Fixed the nightly release, which failed before building anything, so `heyl` is now actually
  published to PyPI (`uvx heyl ...`) and GitHub Releases.
- Security: updated the bundled TLS library (rustls) and other dependencies to versions that
  fix recently published advisories.
- `heyl run --project <url>` now reads a single login — the one whose address is the project
  URL — and puts each of its custom fields into the command's environment (field name = variable
  name). The old `<project>#VARIABLE` login naming no longer works.
- Waiting for your phone now opens a small window on top of your other windows (`get`, `run`,
  `session unlock`, and the pairing QR code for `session create`), saying "<display name> requests
  passwords" for unlocks. It disappears once you approve,
  and Cancel aborts. On Linux it uses zenity or kdialog. `--ui terminal` turns it off, and
  `--ui-cmd` runs your own command instead for unlock requests.
- Nightly releases: when `main` has new commits, a 02:00 UTC run tags a date-versioned release
  commit and publishes it to PyPI (`uvx heyl ...`) and to GitHub Releases, for Linux, macOS
  (Apple Silicon) and Windows on x86_64 and ARM64.
- First release. Pair with your phone (`heyl session create`/`unlock`/`list`/`remove`), read a
  login or a single field (`heyl get`), and run a command with a project's secrets in its
  environment (`heyl run`). Sessions live in the OS keychain.
