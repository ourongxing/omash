# Repository Instructions

## Pull requests

- Do not create or propose a pull request unless the user explicitly asks for one.
- A request to commit or push changes does not imply permission to create a pull request.

## Commit messages

- Use Conventional Commit messages in the form `type(scope): description`.
- Keep the type and scope lowercase and the description concise.
- Example: `chore(ui): refine dashboard spacing`.

## Build verification

- After every code modification, run `cargo build` directly.
- Do not report a modification as complete until the build succeeds, or clearly report the build failure and its cause.
