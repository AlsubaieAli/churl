<!--
Title must be a Conventional Commit: <type>(<scope>): <summary>
Branch should be: <type>/<milestone>-<slug>  (drop <milestone> when there isn't one)
See CONTRIBUTING.md for both conventions.
-->

## Summary

<!-- What this PR does and why. Link the milestone (D1 / R0 / M7.x) or issue. -->

## How it was verified

<!-- cargo test --all, PTY on the real binary, manual drive-test, etc. -->

## Checklist

- [ ] PR title is a Conventional Commit (`<type>(scope): summary`)
- [ ] Branch follows `<type>/<milestone>-<slug>`
- [ ] `cargo fmt --all --check` · `cargo clippy --all-targets --all-features -- -D warnings` · `cargo test --all` pass locally
- [ ] New or changed behaviour ships a test
- [ ] Docs updated (MILESTONES / DECISIONS / ARCHITECTURE / README) if behaviour or structure changed
