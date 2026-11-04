# Native macOS scheduler wake verification

This opt-in harness runs in a disposable process with in-memory workspace identities. It uses the production scheduler, schedule calculation, startup registration and power observers. It never starts CLAI, opens workspace files or databases, reads provider credentials, calls a provider, initiates sleep, or changes the clock. There is no app installation or persistent test workspace to clean up.

The default native test still posts synthetic notifications. The explicit real mode never posts notifications itself. Its separate notification counters establish native event delivery; its scheduler checks establish timing behavior. Apple specifies the workspace notification center for [will-sleep](https://developer.apple.com/documentation/appkit/nsworkspace/willsleepnotification) and [did-wake](https://developer.apple.com/documentation/appkit/nsworkspace/didwakenotification) notifications.

## Awake preparation

From the repository root, using the development Rust toolchain:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --offline --test wake_monitor_native
cargo test --manifest-path src-tauri/Cargo.toml --offline --test wake_monitor_native -- --preflight
```

The first command must print `Synthetic macOS sleep, wake, clock and removal notifications passed`. The second must print `INITIALIZATION PASS` and `PREFLIGHT PASS`. Preflight registers and removes the real observers, pumps the main run loop, and checks fresh enhanced interval/cron initialization despite overdue legacy anchors. It does not establish a real sleep/wake result.

## One manual sleep/wake cycle

At the Mac, run:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --offline --test wake_monitor_native -- --observe-real
```

1. Wait for `READY` in that terminal.
2. Choose **Apple menu > Sleep**. Leave the Mac asleep for 90 seconds, measured on another device. This exceeds the probe's 75-second minimum and crosses an interval and at least one UTC cron minute.
3. Wake the Mac normally. Leave the terminal process running until it prints `REAL WAKE PASS` or a failure. Its post-wake phase has a 90-second limit.
4. Keep the complete output, the source revision used, and the macOS version with the test record.

The probe waits at most ten awake minutes for the manual cycle. Ctrl-C aborts it. If sleep is shorter than the minimum or another power/clock event interrupts the timing phase, the probe exits with an error instead of claiming a pass. Record failures for investigation; a clean preflight is not a substitute for a successful real run.

Expected evidence is separate:

- `SIGNALS PASS`: actual will-sleep and did-wake callbacks were received, with UTC epoch timestamps. No synthetic notification is sent in this mode.
- `REARM PASS`: the production scheduler did not replay an overdue enhanced attempt on its first post-wake poll.
- `INTERVAL PASS`: its first fake interval attempt became eligible no earlier than 15 elapsed seconds after rearming.
- `CRON PASS`: its first fake cron attempt waited for a future UTC minute.
- `REAL WAKE PASS`: both native delivery and these scheduler checks passed, then observers were removed and the process exited.

Attempts are claimed, committed and completed entirely inside the scheduler; the assistant engine and scripts are never entered. Timing is best effort, so later execution is allowed within the bounded probe window.

## Limits of this evidence

This verifies native notification delivery and the production scheduler's behavior for one real macOS sleep/wake cycle. Fresh in-memory initialization is checked; persisted workspace reload and an actual application restart are separate checks. This does not establish full runner/UI acceptance, provider dispatch during real sleep, permission behavior, other platforms, or real wall-clock adjustment handling. The harness records clock-change callbacks but does not change the system clock. Keep issue #2 open until its remaining acceptance evidence is satisfied.
