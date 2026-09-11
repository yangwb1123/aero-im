# Snaplink SDK vendor

`snaplink_sso_client.ts` is the generated client from Snaplink's
`docs/sdks/typescript/client.ts`, copied from Snaplink revision
`aa45e779904672c0c1bac7b400b15504c7df3ed6`. It is not a hand-written Aero API
client.

The browser cannot execute TypeScript directly, so the checked-in
`snaplink_sso_client.js` is the esbuild output of that exact generated source.
When Snaplink regenerates its SDK, replace the TypeScript file from the
Snaplink repository and run:

```bash
pnpm install --frozen-lockfile
pnpm run build:snaplink-sdk
```

The SPA imports only the generated JavaScript build. `snaplink_auth.js` adds
page policy and Aero-session handoff around the SDK's `login` and
`postMFAComplete` operations; it does not duplicate the SDK transport.
