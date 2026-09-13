# Member callback authority fixture

This separate, test-only native executable deliberately sends raw framed callbacks using its real invocation lease. Its `member_probe` operation declares no capabilities; `operator_probe` provides positive controls with document and echo grants. The integration test stages it only into disposable manager state.

```sh
cargo build --locked --manifest-path crates/oracle-modules/tests/fixtures/member-callbacks/Cargo.toml
MEMBER_CALLBACK_PROBE_BINARY="$PWD/crates/oracle-modules/tests/fixtures/member-callbacks/target/debug/oracle-member-callback-probe" cargo test --locked -p oracle-modules --test member_callbacks -- --ignored
```

The test exercises all six current callback methods plus an unknown future method, for ordinary member ingress and an operator invoking the member operation. Valid and deliberately malformed callback payloads must fail before document reads/writes, operation/effect reservations or transport calls. Privileged positive controls confirm those same service paths work, and malformed privileged controls distinguish decoding from earlier member-authority rejection.
