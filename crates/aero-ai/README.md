# aero-ai

AI control plane for Aero IM. It integrates Anthropic completions, Voyage
embeddings, deterministic no-key fallbacks, RAG services, transcription,
moderation, durable paid-operation accounting, budgets, and the polling
`AiWorker` job state machine.

Provider calls are fenced by durable reservations and stable operation IDs;
retries must not create a second paid request.

```bash
cargo test -p aero-ai --lib
```
