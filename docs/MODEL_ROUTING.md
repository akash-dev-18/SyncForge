# SyncEngine OpenCode Model Routing

This is a starter configuration, not a claim that a model has won a benchmark. Model quality, availability, quotas, and provider terms can change. Evaluate these assignments using a small repeatable task suite and adjust based on observed results.

## Model IDs

OpenCode Zen's published model IDs use the `opencode/<model-id>` format:

- `opencode/deepseek-v4.1-flash-free`
- `opencode/mimo-v2.6-flash-free`
- `opencode/nemotron-3.5-lightning-free`
- `opencode/nemotron-3-ultra-free`
- `opencode/nemotron-3.5-super-vl-free`
- `opencode/beam-free`
- `opencode/big-pickle`
- `opencode/space-bunny-free`

Confirm each model is visible in `/models` and the provider is connected before expecting it to work.

## Primary assignment heuristic

| Agent | Primary model | Intended use |
|---|---|---|
| Build / `@coder` | DeepSeek V4.1 Flash Free | General implementation and tool-driven coding |
| `@editor` | Nemotron 3.5 Lightning Free | Narrow edits and quick fixes |
| `@architect` | Nemotron 3 Ultra Free | Architecture and trade-off review |
| `@test-engineer` | MiMo V2.6 Flash Free | Test generation and edge cases |
| `@debugger` | MiMo V2.6 Flash Free | Root-cause investigation |
| `@security-reviewer` | Nemotron 3 Ultra Free | Security audit (review only) |
| `@database-reviewer` | Nemotron 3 Ultra Free | Schema/transaction review |
| `@protocol-reviewer` | DeepSeek V4.1 Flash Free | Sync/transport protocol reasoning |
| `@performance-reviewer` | Nemotron 3.5 Lightning Free | Measurement-guided optimization review |
| `@code-reviewer` | DeepSeek V4.1 Flash Free | Independent read-only diff review |
| `@rust-mentor` | MiMo V2.6 Flash Free | Code explanation and exercises |
| `@docs-writer` | Beam Free | Documentation drafting |
| `@diagram-reviewer` | Nemotron 3.5 Super VL Free | UML/visual review when images are actually supplied |

These are hypotheses chosen from the available names and intended role—not verified comparative rankings. Space Bunny and Big Pickle are listed by OpenCode as stealth models, so their underlying model identity is not transparent. They are kept as fallback candidates rather than being treated as proven specialists.

## Automatic fallback

The `opencode-runtime-fallback` community plugin is configured to retry with the next model for detected transient provider errors, rate limits, quota exhaustion, overloads, and first-token timeouts. Per-agent chains live in root `opencode.json`; global defaults live in `.opencode/opencode-fallback.jsonc`.

Fallback is not a perfect transparent continuation: a failed tool-call turn may be restarted by another model. Review code and diffs; never let retries justify unsafe commands or skip tests. When every configured model fails, OpenCode should surface the failure rather than pretending the task completed.

## Suggested small evaluation set

Before trusting the routing, run the same low-risk prompt against each candidate: (1) add a small Rust function plus tests, (2) find a bug in a deliberately flawed test, (3) review an SQLite migration, (4) identify a file-sync crash window, (5) use available tools to run a test and report exact output. Score compile success, test quality, tool-call correctness, hallucinations, latency, and availability. Keep a record instead of trusting model branding alone.

## Privacy warning

Free model availability and data handling differ by provider and may change. OpenCode's model catalogue notes special limits/retention terms for these trial models; some model providers may retain prompts or use them to improve models, and the Nemotron free endpoints have a trial-use warning not to submit personal or confidential data. Do not send API keys, passwords, private keys, personal data, proprietary secrets, or sensitive customer files to free endpoints. Review the current model terms before sending private source code.
