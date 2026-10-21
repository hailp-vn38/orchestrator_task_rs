## Agent skills

### Build, test & cleanup (token-saving)

Use `scripts/check.sh` instead of verbose `cargo` commands to keep context usage minimal (prints `ok` on success, filtered errors on failure, and auto-prunes stale artifacts):
- Gate check (`fmt` + `clippy` + `test`): `./scripts/check.sh` (or `./scripts/check.sh full`)
- Run tests quietly: `./scripts/check.sh test [cargo-test-args...]`
- Typecheck / lint quietly: `./scripts/check.sh check` or `./scripts/check.sh clippy`
- Clean build/test bloat: `./scripts/clean.sh [prune|test-bins|all] [--siblings]`

### Issue tracker

Issues and specs live as local Markdown files under `.scratch/<feature>/`. See `docs/agents/issue-tracker.md`.

### Triage labels

Use `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, and `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Use the single-context layout with root `CONTEXT.md` and `docs/adr/`. See `docs/agents/domain.md`.
