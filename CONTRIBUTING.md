**Copyright notice**
This template is based on https://github.com/dotnet/runtime/blob/main/CONTRIBUTING.md

# Contributing to `uc-intg-appletv`

You can contribute with issues and PRs. Filing an issue for a problem you find is a great way to contribute. Code contributions are welcome.

Please follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Contribution "Bar"

Maintainers merge changes that improve the project.

Maintainers do not merge changes that only change style or make the code harder to read. A change can be reverted if it breaks something.

Contributions must also follow the other rules in this document.

## DOs and DON'Ts

Please do:

- **DO** follow the style of the file you change, even if it differs from the general guidelines.
- **DO** run `cargo fmt` and `cargo clippy` before you push.
- **DO** keep discussions focused. For a new topic, open a new issue.

Please do not:

- **DON'T** make PRs for style changes only.
- **DON'T** send big PRs without warning. Open an issue first, so we can agree on a direction before you spend a lot of time.
- **DON'T** commit code that you did not write. If you find code that fits this project, open an issue and discuss it first.
- **DON'T** change license files or headers. If you see a problem with them, open an issue.
- **DON'T** add new features without an issue and a discussion first.

## Breaking Changes

Describe breaking changes in the PR and explain why they are needed.

## Suggested Workflow

1. Create an issue for your work.
    - You can skip this for small changes.
    - Use an existing issue on the topic if there is one.
    - Get agreement that the change is a good one.
    - Say if you plan to implement it. You can ask to be assigned to the issue.
2. Create a fork of the repository on GitHub, if you do not have one.
3. In your fork, create a branch from `main` (`git checkout -b mybranch`).
    - Use a name that shows the purpose, for example `githubhandle/bug-description` or `githubhandle/feat-issue_id`.
4. Make and commit your changes to the branch.
    - See [Build](README.md#build) for how to build.
    - Follow [Commit Messages](#commit-messages).
5. Add tests for your change, if applicable.
6. Run the checks from [Development](README.md#development):
    ```
    cargo fmt --all --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    cargo deny check
    ```
    - All checks must pass.
7. Create a PR against the **main** branch.
    - Describe the issue or improvement, the type of change and linked issues.
    - Check that all CI checks pass.
8. Wait for feedback from the maintainers.
9. When the maintainers approve and all checks pass, the PR is merged.
    - You can delete your branch after the merge.

## Testing with real devices

Some changes need a real Apple TV and Remote. `tools/manual_test.sh` runs the manual tests step by step. Run `tools/remote-sim` for the checks that need no person. In your PR, say which devices and software versions you tested with.

## Commit Messages

Format commit messages like this (based on [A Note About Git Commit Messages](http://tbaggery.com/2008/04/19/a-note-about-git-commit-messages.html)):

```
Summarize change in 50 characters or less

Add more detail after the first line. Leave one blank line below the
summary and wrap all lines at 72 characters or less.

If the change fixes an issue, leave another blank line after the last
paragraph and write the issue in the format below.

Fix #42
```

Keep commits focused. Do not mix unrelated changes in one commit. Do not split one change into many small commits.

## PR - CI Process

CI runs the build, tests and checks for each PR. They must pass.

If CI fails, fix your changes or ask for help.

## PR Feedback

Maintainers and community members review PRs. Community feedback is valued. At least one maintainer reviews every PR before merge.

Be clear and explicit in your feedback. Be patient with people who do not understand your approach at first.
