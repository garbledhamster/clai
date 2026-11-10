# Working on the CLAI+ fork

This repository is the public CLAI+ fork. `main` mirrors [upstream CLAI](https://github.com/clairun/clai/tree/main). Work on the fork in `clai-plus`, where the enhanced workspace scheduler lives. The scheduler's public design is in [issue #1](https://github.com/garbledhamster/clai/issues/1).

## Bring in upstream changes

1. Start with a clean checkout of the public fork. If `upstream` is absent, add it with `git remote add upstream https://github.com/clairun/clai.git`.
2. Run `git fetch upstream main`. Update the fork's `main` by fast-forwarding to `upstream/main`. Keep feature changes on `clai-plus`.
3. Switch to `clai-plus` and merge `upstream/main`. Review conflicts against both branches. Preserve upstream security fixes and the fork's scheduler behavior.
4. Run `npm ci`, `npm run lint`, `npm run typecheck`, `npm test`, and `cargo test --manifest-path src-tauri/Cargo.toml --lib --locked`. Regenerate and check bindings if Rust IPC types changed.
5. Review the new commits and their diff before pushing `clai-plus` to the public fork. Publish from the public checkout. Live profiles, personal account details, credentials, and local deployment records belong in private storage.
