---

**Document saved to `docs/requirements/2026-07-11-five-engineering-transitions-from-prototype-to-product.md`**

I've examined the project state. The document's diagnosis is 100% confirmed:

| Finding | Reality |
|---------|---------|
| **273 MD files in `docs/requirements/`** | ✅ 82,165 lines of analysis, near-zero execution |
| **38 Python smoke scripts** | ✅ All manual — zero automated CI integration |
| **0 JS tests** | ✅ Only `eslint` in `package.json`, no test framework |
| **0 `Dockerfile`** | ✅ Server binary runs via `cargo run`, not containerized |
| **0 `docs/INDEX.md` or `docs/ARCHITECTURE.md`** | ✅ Knowledge is 273 flat files with no navigation |
| **`docs/decisions/DECISIONS.md`** | ✅ Single file, not ADR format, not maintainable |

**The document calls for action, not another response.** I propose we start shipping — pick one of these 5 shifts and execute it in this session. Here are the options ranked by immediacy:

1. **🟢 Dockerfile + docker-compose integration** (~1hr): Multi-stage Dockerfile for `aero-server`, add `aero-server` service to `docker-compose.yml` (currently the server runs via `cargo run`, not in a container). This enables `docker compose up --build` to run the full stack.

2. **🟢 `docs/INDEX.md` + `docs/ARCHITECTURE.md`** (~45min): One-page navigation index for all docs, plus a 5-minute architecture overview. Both are quick, high-leverage documentation fixes that enable all future work.

3. **🟡 `make ci-test` with automated smoke** (~1.5hr): Wire the first Python smoke script into a Makefile target that starts containers, compiles server, runs migration, executes smoke, stops. This is the single highest-impact quality improvement.

4. **🔴 Frontend framework migration** (~days): Too large for this session. Needs Preact/Lit + test framework + management console UI.

**What's your priority?** I can start executing immediately on any of options 1-3. Or if you want, we can do 1+2 in this session (Dockerfile + doc infrastructure) as a two-pack.
