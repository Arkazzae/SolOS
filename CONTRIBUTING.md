# Contributing to Sol OS

## Commits and pull requests

Use `type(scope): subject` for commit subjects and pull request titles. Write in
English, start the subject with a lowercase verb, and keep the whole first line
within 72 characters. Omit the final period.

Use a scope when it helps someone find changes to their part of Sol OS. A core
name like `gb`, `gba`, `md`, `n64`, `nes`, `pico8` or `psx` works well, as do
`web`, `savestate` and `z80`. Omit the parentheses for repository-wide changes.

| Type | Use for |
| --- | --- |
| `feat` | New behavior |
| `fix` | A bug fix |
| `perf` | A performance improvement |
| `refactor` | An internal change that preserves behavior |
| `test` | Test cases or fixtures |
| `docs` | Documentation |
| `build` | Dependencies, build tooling or packaging |
| `ci` | GitHub Actions and contribution checks |
| `chore` | Other repository maintenance |

```text
fix(gb): restore the timer after loading a save
feat(web): add a shortcut to the library
perf(n64): reuse the audio buffer between frames
docs: explain how to run the core tests
```

Keep one logical change per commit. Include its regression test when relevant.
For a subtle fix, leave a blank line after the subject and explain the previous
behavior and why the change works. Use your own Git identity. Human co-authors
can use `Co-authored-by`. Omit generated-by lines and tool signatures.

## Local checks

Enable the commit-message hook once per clone, from the repository root.

```sh
git config --local core.hooksPath .githooks
```

Run the tests for the part you changed. For example, `cargo test --release -p gb`
checks the Game Boy core. Some tests need fixtures from the core's
`tests/roms/fetch.sh`. For web changes, see the [web setup](web/README.md)
and run `pnpm --dir web types` after installing dependencies.

## Merging

Open a pull request against `main`. Describe what changed and how you checked it.
GitHub's `commits` check validates each incoming commit and the PR title,
including title edits. Squash merging uses the PR title and description for the
resulting commit. Use rebase merging when each commit should stay in the history.
