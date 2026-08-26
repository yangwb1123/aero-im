# aero-push

Provider-neutral mobile push gateway. It builds FCM HTTP v1 and APNs payloads,
accepts renewable credential-provider closures, classifies provider responses,
and provides a deterministic `FakeGateway` for server and bot tests.

Native mobile client SDKs are outside this repository's scope.

```bash
cargo test -p aero-push --lib
```
