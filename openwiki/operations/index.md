# Files

- [Build, test, and CI — workspace validation](build-test-ci.md) - The three CI gates (test on Linux+macOS, fmt, clippy -D warnings), the pinned rust-toolchain, the standard tool registry, the shared resource bounds, and the VHS demo tape.
- [Security posture — sandboxing and the trust boundary](security-posture.md) - With --sandbox writes are confined to the workspace on every platform and on macOS shell commands run under Seatbelt; --sandbox-strict also denies network. Without --sandbox tools run with the process's full authority and that is still the default.
- [Where state lives — bullpen home and the SQLite store](state-layout.md) - The single durable store at $BULLPEN_HOME/bullpen.db in WAL mode, the auth.json credential file, the logs and worktrees directories, and how to read a running database with the immutable flag.
