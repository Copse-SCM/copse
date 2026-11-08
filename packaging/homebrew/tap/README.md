# Homebrew tap for copse

[copse](https://github.com/Copse-SCM/copse) is a fast version control system that works with git servers, with undo for everything and a built-in terminal interface.

```bash
brew install copse-scm/copse/copse
```

That installs the prebuilt binary for macOS (Apple Silicon or Intel) or Linux (x86_64). To build the latest main from source instead (needs Rust, which Homebrew installs for you):

```bash
brew install --HEAD copse-scm/copse/copse
```

`brew upgrade copse` picks up new releases. The formula is updated automatically by copse's release workflow; please report problems at [Copse-SCM/copse](https://github.com/Copse-SCM/copse/issues).
