---
paths:
  - ".github/workflows/**"
---

# Workflows and release

- After a workflow edit, run `actionlint .github/workflows/*.yml` and `uvx zizmor .github/`
  (security audit).
- Pushing a tag `v*` runs `release.yml`: the test gate runs on all three shipped OSes, every target
  builds on a pinned runner image (release jobs are deliberately cache-free), the archives bundle
  both binaries plus the skill, provenance is attested
  (`gh attestation verify <archive> --repo junyeong-ai/hatel`), and the runner-bundled `gh`
  publishes the release.
