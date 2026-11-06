---
paths:
  - "crates/cli/src/service.rs"
  - "crates/cli/tests/service.rs"
---

# Service units

- A unit is hatel's own only while it is byte-equal to a rendering some release wrote. `service`
  and `service --restart` replace only such a unit and keep any other as a hand edit, since
  replacing it would silently drop what it sets.
- When `launchd_plist` or `systemd_unit` changes, move the rendering it replaces into
  `earlier_launchd_plists` / `earlier_systemd_units`, with a copy in the classification test. The
  pinned-rendering tests fail on the change but cannot check the move, and without it every
  installed unit reads as hand-edited.
- The unit carries nothing to configure: the receiver reads `config.toml` like every other process.
