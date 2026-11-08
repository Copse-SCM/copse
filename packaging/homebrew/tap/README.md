# Homebrew tap for copse

[copse](https://github.com/Copse-SCM/copse) is a fast version control system that works with git servers, with undo for everything and a built-in terminal interface.

```bash
brew install copse-scm/copse/copse            # the latest release
brew install copse-scm/copse/copse-nightly    # or the latest daily build of main
```

Both install the prebuilt binary for macOS (Apple Silicon or Intel) or Linux (x86_64), and both provide the `copse` command, so install one or the other (`brew uninstall` one before switching). `brew upgrade` picks up new releases, or each day's new nightly.

To build main from source instead (needs Rust, which Homebrew installs for you):

```bash
brew install --HEAD copse-scm/copse/copse
```

The formulas are written by copse's release and nightly workflows (`packaging/homebrew/render.py`); please report problems at [Copse-SCM/copse](https://github.com/Copse-SCM/copse/issues).
