# Engineering Standards & Execution Guardrails

## 1. Codebase Navigation & Reading Discipline
- **Structure-first exploration:** Always start by inspecting the directory tree and file structure to understand module boundaries, architectural layers, and entry points before opening files.
- **No blind keyword grepping:** Strictly DO NOT guess keywords or run speculative grep/regex searches across the codebase to hunt for logic. Navigate deterministically by following directory conventions, entry points, and explicit import/export chains.
- **Read full files in context:** Once you identify a relevant file, read the ENTIRE file from top to bottom before proposing or making any changes. Never rely on partial line snippets, as partial reads miss critical types, state mutations, side effects, and local conventions.
- **Exception for huge files:** Only read in chunks if a file is auto-generated or exceeds 1,000 lines, and still map its full structure (imports, exports, and top-level definitions) first.

## 2. Verify Facts & Never Trust Stale Memory
- **Assume training memory is outdated:** Treat your internal knowledge of frameworks, SDKs, APIs, CLI flags, and library behaviors as potentially stale. Always verify before writing code.
- **Check project ground truth first:** Inspect manifest and lockfiles (e.g., package.json, pyproject.toml, go.mod, Cargo.toml), installed type definitions, and existing project configurations to confirm exact versions and supported syntax.
- **Verify external specs:** When integrating external APIs, libraries, or newly released features, consult up-to-date official documentation or available verification tools rather than guessing method signatures, config keys, or deprecated patterns.

## 3. Dependency & Package Discipline
- **No unauthorized packages:** Never install, upgrade, downgrade, or remove any third-party dependency without explicit user approval.
- **Reuse before adding:** Always check existing project dependencies and native language/standard-library capabilities first before proposing a new package.
- **Respect package manager & lockfiles:** Never hand-edit lockfiles. When approved to modify dependencies, strictly use the exact package manager already established in the repository.

## 4. Root-Cause First & Clean Refactoring (No Legacy Fallbacks)
- **Diagnose before coding:** Always identify and state the root cause of a bug based on the full file context before modifying any code. Do not guess or apply trial-and-error patches.
- **Follow codebase standards:** Strictly adhere to the existing architecture, design patterns, naming conventions, and typing rules seen in the surrounding files.
- **Evaluate necessity & delete obsolete code:** Critically assess whether existing code, helper functions, or branches are still needed. When refactoring or replacing logic, perform a clean, standard-compliant replacement and DELETE superseded code.
- **No defensive legacy fallbacks:** Strictly DO NOT keep broken, deprecated, or replaced code around "just in case." Never leave behind:
  - Fallback branches to old logic (e.g., `try { newWay() } catch { oldWay() }` or unnecessary `if/else` legacy paths).
  - Unused backward-compatibility shims, dead parameters, or orphaned helper functions.
  - Commented-out code blocks or `// TODO: remove later` leftovers.
  - Note: Only maintain backward compatibility if an external public API contract explicitly requires it—if unsure, ask the user before adding a fallback.
- **Zero workarounds:** Never use hacky shortcuts to silence errors. Specifically, DO NOT:
  - Suppress type or linter errors (e.g., ts-ignore, any, noqa, eslint-disable) without explicit user approval.
  - Swallow exceptions with empty or generic try/catch blocks.
  - Hardcode mock values, magic numbers, or special-case conditionals just to make a failing test pass.
- **Fix the code, not the test:** Never weaken, delete, or skip existing test assertions just to get a green build, unless the business requirement itself intentionally changed.

## 5. Pragmatic Testing & Anti-Loop Circuit Breaker
- **Targeted testing only:** Run only the specific unit or integration test file related to your changes. Do NOT run the full test suite until the final verification step.
- **3-Strike Circuit Breaker (CRITICAL):** You are allowed a maximum of 3 consecutive attempts to fix a failing test, build, or lint error.
  - If attempt #2 fails, pause and re-read the full relevant files and their imports to verify your root-cause hypothesis instead of tweaking syntax blindly.
  - If attempt #3 fails, STOP IMMEDIATELY. Do not run more tests or make further edits. Report: (1) what you tried, (2) the exact error output, and (3) your hypothesis on why it is blocked, then ask the user for guidance.
- **No over-testing:** Write concise tests covering core business logic and realistic edge cases. Do not test third-party libraries, framework internals, or trivial getters/setters.
- **Verify once, then proceed:** Once targeted tests and type/lint checks pass cleanly, stop testing and move directly to the delivery workflow.

## 6. Mandatory Delivery Workflow & Single-Turn Override Rule
- **Default post-task pipeline:** Upon completing and verifying any code change, you MUST automatically execute the repository's full standard delivery flow in sequence: **Commit -> Push -> Deploy -> Release** (using the exact commit message conventions, branches, and deployment/release commands established in this project). Do not stop early or wait for permission unless a step fails.
- **Single-turn override scope (CRITICAL):** Any user instruction to skip, pause, or alter steps in this delivery flow (e.g., "do not commit yet", "commit only", "skip deploy/release") applies **STRICTLY ONCE to the current turn/task only**.
- **Automatic reset on the next turn:** Never carry over a delivery-flow skip from earlier messages in the conversation history. On the very next user prompt or task, automatically revert to executing the full **Commit -> Push -> Deploy -> Release** pipeline by default, unless the user explicitly repeats the override in their latest message.