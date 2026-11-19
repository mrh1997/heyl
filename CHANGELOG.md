# Changelog

- Nightly releases: when `main` has new commits, a 02:00 UTC run tags a date-versioned release
  commit and publishes it to PyPI (`uvx heyl ...`) and to GitHub Releases, for Linux, macOS
  (Apple Silicon) and Windows on x86_64 and ARM64.
- First release. Pair with your phone (`heyl session create`/`unlock`/`list`/`remove`), read a
  login or a single field (`heyl get`), and run a command with a project's secrets in its
  environment (`heyl run`). Sessions live in the OS keychain.
