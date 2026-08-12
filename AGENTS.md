# Repository Instructions

## Pull requests

- Do not create or propose a pull request unless the user explicitly asks for one.
- A request to commit or push changes does not imply permission to create a pull request.

## Commit messages

- Use Conventional Commit messages in the form `type(scope): description`.
- Keep the type and scope lowercase and the description concise.
- Example: `chore(ui): refine dashboard spacing`.

## Build verification

- Run `cargo build` directly after modifying any Rust source file (`*.rs`).
- Changes limited to documentation, metadata, configuration, licenses, or other non-Rust files do not require a build.
- When a build is required, do not report the modification as complete until it succeeds, or clearly report the build failure and its cause.
