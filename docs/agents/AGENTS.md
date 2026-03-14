# AGENTS.md - airoha-uartboot

> Guidelines for AI coding agents working in this Rust codebase.

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Dependency versions:** Explicit versions for stability
- **Configuration:** Cargo.toml workspace with `workspace = true` pattern
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]` via `[workspace.lints.rust]`)

### Release Profile

The release build optimizes for size (this is a CLI binary):

```toml
[profile.release]
opt-level = "z"     # Optimize for size
lto = true          # Link-time optimization
codegen-units = 1   # Single codegen unit for better optimization
panic = "abort"     # Smaller binary, no unwinding overhead
strip = true        # Remove debug symbols
```

## Backwards Compatibility

We do not care about backwards compatibility—we're in early development with no users. We want to do things the **RIGHT** way with **NO TECH DEBT**.

- Never create "compatibility shims"
- Never create wrapper functions for deprecated APIs
- Just fix the code directly

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors and warnings (workspace-wide)
cargo check --workspace --all-targets

# Check for clippy lints (pedantic + nursery are enabled)
cargo clippy --workspace --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

If you see errors, **carefully understand and resolve each issue**. Read sufficient context to fix them the RIGHT way.

## Third-Party Library Usage

If you aren't 100% sure how to use a third-party library, **SEARCH ONLINE** to find the latest documentation and current best practices.

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

- Refactors/codemods: rename APIs, change import forms
- Policy checks: enforce patterns across a repo
- Editor/automation: LSP mode, `--json` output

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

- Recon: find strings, TODOs, log lines, config values
- Pre-filter: narrow candidate files before ast-grep

### Rule of Thumb

- Need correctness or **applying changes** -> `ast-grep`
- Need raw speed or **hunting text** -> `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

### Rust Examples

```bash
# Find structured code (ignores comments)
ast-grep run -l Rust -p 'fn $NAME($$$ARGS) -> $RET { $$$BODY }'

# Find all unwrap() calls
ast-grep run -l Rust -p '$EXPR.unwrap()'

# Quick textual hunt
rg -n 'println!' -t rust

# Combine speed + precision
rg -l -t rust 'unwrap\(' | xargs ast-grep run -l Rust -p '$X.unwrap()' --json
```

## Beads (br) — Dependency-Aware Issue Tracking

Beads provides a lightweight, dependency-aware issue database and CLI (`br` - beads_rust) for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging and file reservations.

**Important:** `br` is non-invasive—it NEVER runs git commands automatically. You must manually commit changes after `br sync --flush-only`.

### Conventions

- **Single source of truth:** Beads for task status/priority/dependencies; Agent Mail for conversation and audit
- **Shared identifiers:** Use Beads issue ID (e.g., `br-123`) as Mail `thread_id` and prefix subjects with `[br-123]`
- **Reservations:** When starting a task, call `file_reservation_paths()` with the issue ID in `reason`

### Typical Agent Flow

1. **Pick ready work (Beads):**
   ```bash
   br ready --json  # Choose highest priority, no blockers
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="br-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="br-123", subject="[br-123] Start: <title>", ack_required=true)
   ```

4. **Work and update:** Reply in-thread with progress

5. **Complete and release:**
   ```bash
   br close 123 --reason "Completed"
   br sync --flush-only  # Export to JSONL (no git operations)
   ```
   ```
   release_file_reservations(project_key, agent_name, paths=["src/**"])
   ```
   Final Mail reply: `[br-123] Completed` with summary

### Mapping Cheat Sheet

| Concept | Value |
|---------|-------|
| Mail `thread_id` | `br-###` |
| Mail subject | `[br-###] ...` |
| File reservation `reason` | `br-###` |
| Commit messages | Include `br-###` for traceability |

---

## bv — Graph-Aware Triage Engine

bv is a graph-aware triage engine for Beads projects (`.beads/beads.jsonl`). It computes PageRank, betweenness, critical path, cycles, HITS, eigenvector, and k-core metrics deterministically.

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). For agent-to-agent coordination (messaging, work claiming, file reservations), use MCP Agent Mail.

**CRITICAL: Use ONLY `--robot-*` flags. Bare `bv` launches an interactive TUI that blocks your session.**

### The Workflow: Start With Triage

**`bv --robot-triage` is your single entry point.** It returns:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bv --robot-triage        # THE MEGA-COMMAND: start here
bv --robot-next          # Minimal: just the single top pick + claim command
```

### Command Reference

**Planning:**
| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with `unblocks` lists |
| `--robot-priority` | Priority misalignment detection with confidence |

**Graph Analysis:**
| Command | Returns |
|---------|---------|
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core, articulation points, slack |
| `--robot-label-health` | Per-label health: `health_level`, `velocity_score`, `staleness`, `blocked_count` |
| `--robot-label-flow` | Cross-label dependency: `flow_matrix`, `dependencies`, `bottleneck_labels` |
| `--robot-label-attention [--attention-limit=N]` | Attention-ranked labels |

**History & Change Tracking:**
| Command | Returns |
|---------|---------|
| `--robot-history` | Bead-to-commit correlations |
| `--robot-diff --diff-since <ref>` | Changes since ref: new/closed/modified issues, cycles |

**Other:**
| Command | Returns |
|---------|---------|
| `--robot-burndown <sprint>` | Sprint burndown, scope changes, at-risk items |
| `--robot-forecast <id\|all>` | ETA predictions with dependency-aware scheduling |
| `--robot-alerts` | Stale issues, blocking cascades, priority mismatches |
| `--robot-suggest` | Hygiene: duplicates, missing deps, label suggestions |
| `--robot-graph [--graph-format=json\|dot\|mermaid]` | Dependency graph export |
| `--export-graph <file.html>` | Interactive HTML visualization |

### Scoping & Filtering

```bash
bv --robot-plan --label backend              # Scope to label's subgraph
bv --robot-insights --as-of HEAD~30          # Historical point-in-time
bv --recipe actionable --robot-plan          # Pre-filter: ready to work
bv --recipe high-impact --robot-triage       # Pre-filter: top PageRank
bv --robot-triage --robot-triage-by-track    # Group by parallel work streams
bv --robot-triage --robot-triage-by-label    # Group by domain
```

### Understanding Robot Output

**All robot JSON includes:**
- `data_hash` — Fingerprint of source beads.jsonl
- `status` — Per-metric state: `computed|approx|timeout|skipped` + elapsed ms
- `as_of` / `as_of_commit` — Present when using `--as-of`

**Two-phase analysis:**
- **Phase 1 (instant):** degree, topo sort, density
- **Phase 2 (async, 500ms timeout):** PageRank, betweenness, HITS, eigenvector, cycles

### jq Quick Reference

```bash
bv --robot-triage | jq '.quick_ref'                        # At-a-glance summary
bv --robot-triage | jq '.recommendations[0]'               # Top recommendation
bv --robot-plan | jq '.plan.summary.highest_impact'        # Best unblock target
bv --robot-insights | jq '.status'                         # Check metric readiness
bv --robot-insights | jq '.Cycles'                         # Circular deps (must fix!)
```



### Commit Policy
Agents are expected to commit their changes after completing each task or logical unit of work. Do not leave uncommitted changes. Follow the git commit guidelines below.

### Git workflow after QA
  - Now, based on your knowledge of the project, commit all changed files now in
    a series of logically connected groupings with super detailed commit messages
    for each. Take your time to do it right. Don't edit the code at
    all. Don't commit obviously ephemeral files.
  - Do not sign commits. Add my configured sign-off `git commit -s`
  - Use `git -c commit.gpgsign=false commit -s -m` to avoid signing
  - Avoid one `-m` per wrapped line (that inserts blank lines); use a
    single body with embedded newlines or a message file instead
  - Commit subject should include `prefix: ...` that matches the
    top-level tree/area being changed
    - Makefile: ...
    - tools: ...
    - AGENTS: ...
  - Commit description should include:
    - what is currently wrong/missing
    - why is this change needed
    - "Currently ..."
    - "So lets fix ..." or "So lets add ..." (use the verb that matches
      the change)
    - if there is some log evidence available like build/compile failure,
      always include it as another backing proof, do not wrap long lines in
      this case, make sure complete context is there, strip local paths etc.
    - Wrap commit description lines to 72 characters

### Asking Questions

When you need clarification or user input, format questions in a structured way:

1. **Yes/No questions** - For simple binary decisions
   - For each option (yes/no) include:
     - **Pros**: Benefits of this choice
     - **Cons**: Drawbacks or risks
     - **Tradeoffs**: What you gain vs. what you give up
   - End with a **Suggested option** and brief rationale

   Example format:
   ```
   Should we proceed with the migration now? (y/n)

   Yes:
      Pros: Unblocks dependent work, fixes known issues sooner
      Cons: Risk of regressions during release week
      Tradeoffs: Speed vs. stability

   No:
      Pros: More time for testing, safer timing
      Cons: Delays dependent features, tech debt lingers
      Tradeoffs: Safety vs. momentum

   Suggested: (n) - Release week is high-risk; defer to next sprint
   ```

2. **Multiple choice options** - For decisions with several alternatives
   - Present options as a/b/c/d
   - For each option include:
     - **Pros**: Benefits of this approach
     - **Cons**: Drawbacks or risks
     - **Tradeoffs**: What you gain vs. what you give up
   - End with a **Suggested option** and brief rationale

   Example format:
   ```
   How should we handle the deprecated API?

   a) Remove immediately
      Pros: Clean codebase, no legacy debt
      Cons: Breaking change for consumers
      Tradeoffs: Speed vs. compatibility

   b) Deprecation warning + removal in next major version
      Pros: Gives consumers time to migrate
      Cons: Maintenance burden, dual code paths
      Tradeoffs: Compatibility vs. complexity

   c) Keep indefinitely with wrapper
      Pros: Full backward compatibility
      Cons: Permanent tech debt
      Tradeoffs: Stability vs. maintainability

   Suggested: (b) - Balances user needs with codebase health
   ```
