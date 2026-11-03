# Contributing

## Workflow

1. Fork the repository, or create a branch if you have write access.
2. Make the change on a branch. One topic per pull request.
3. Run the checks below.
4. Open a pull request against `main`. CI must pass and one approval is required.

Pull requests are squash-merged. The PR title becomes the commit subject.

## Checks

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

## Commit and PR titles

Conventional Commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `ci:`, `chore:`.
Subject at most 50 characters, imperative, no trailing period.

## Scope

- Windows only.
- Battery reading, report parsing and statistics belong in
  [batlens-core](https://github.com/onlyatuna/batlens-core). Change them there with a
  separate pull request, then update the `rev` of the `batlens-core` dependency here.
- Write code comments and user-facing text in English.
