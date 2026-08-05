# AGENTS.md

Maggie – Native Wayland screen magnifier and utility tool, written in Rust (Edition 2024). 

## Toolchain & Environment
- **Rust Edition:** 2024 (`rust-version = "2024"`)
- **Task Runner:** `just` (see `justfile` in project root for available recipes like `just build`, `just run`, `just test`, and `just check`)

## Project Documentation & Source of Truth
- **Specification & Design:** All functional requirements, technical roadmaps, configuration schemas, and architectural decisions are maintained in **`SPEC.md`**. 
- **Compliance & Alignment:** Sarah is responsible for maintaining `SPEC.md` and ensuring all ongoing work strictly adheres to it. Agents must consult `SPEC.md` for project scope and design details.

## Team Dispatch Protocol (Autonomous Routing)
The primary agent must actively delegate tasks based on the following triggers:
- **Research / External Documentation:** If the task requires looking up unfamiliar Wayland protocol specs, Smithay crates, or Rust syntax edge cases, automatically invoke **Donnie** to research and summarize before writing code.
- **Architecture / System Design:** If introducing a new module (e.g., the screenshot window grid or OSD layout), consult **Kayla** first to map out the structure against `SPEC.md`.
- **Compliance & Spec Tracking:** After completing any feature implementation or milestone, invoke **Sarah** to audit the changes against `SPEC.md` and update documentation if necessary.
- **Implementation:** **James** handles core coding and git commits once specs are clear.

*Do not hesitate to query or switch to subagents when a task falls under their designated capability.*

## Standard Workflow & Rules
- **Display Server:** Strictly Wayland-only (as mandated in `SPEC.md`).
- **Release Build Mandate:** After **every** change or implementation iteration (fix, feature, tuning, revert, or documentation that affects code), the responsible agent **must** build the release target using the justfile recipe (`just build` — the `build` recipe in the project root `justfile`, which runs `cargo build --release`) and confirm it succeeds before reporting completion. Never finish a task with a stale or unbuilt `target/release/maggie`.
- **Commit Mandate:** James is responsible for committing changes after every successful feature or milestone with a descriptive, concise commit message.
- **Agent Roles:** Follow the individual agent parameters defined in `.opencode/agents/`.
