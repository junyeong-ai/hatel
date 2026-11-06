---
paths:
  - ".claude/skills/**"
  - "Cargo.toml"
---

# The hatel skill

- `SKILL.md` frontmatter keeps a `version` field tracking `workspace.package.version`. It is not
  part of the skill spec; it lets an installed skill be matched to its binaries. Bump it and the
  body's `This skill targets` version with the workspace version
  (`skill_version_tracks_the_workspace_version` checks both).
- The installer and release workflow copy only `SKILL.md`, so keep it self-contained unless both
  packaging paths ship supporting files.
- `description` is third person, at most 1,024 characters, and carries the discovery cues. Keep
  query guidance separate from setup and diagnosis.
