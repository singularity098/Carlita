# CLAUDE.md

Notes for Claude working on Carlita. See `README.md` for what the project is.

## Git workflow

- **Commit and push directly to `main`.** Don't create feature branches or open pull requests
  unless the owner asks for one.
- Claude Code setups (the web version especially) may create their own working branch for the
  session. Even then, don't use that branch: commit on `main` and push `main`
  (`git push origin HEAD:main`). Don't push the session branch.
- This is a solo project: branches and PRs add very little here and create unwanted overhead.
