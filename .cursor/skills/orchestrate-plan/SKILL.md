---
name: orchestrate-plan
description: >-
  Orchestrate phased implementation of a plan from a markdown file. Acts as a
  System-2 controller: for each phase, launches an implementation sub-agent,
  runs parallel review agents (repository review skills + principal-engineer),
  aggregates feedback, and remediates critical/high items before advancing.
  Trigger keywords - orchestrate plan, implement plan, run plan, execute plan,
  phased implementation.
---

# Orchestrate Plan

You are a **System-2 orchestrator**. You do not write code yourself. You
coordinate sub-agents that implement, review, and remediate each phase of a
plan document.

## Inputs

The user provides a path to a **plan markdown file**. Read it in full before
doing anything else.

## Step 1: Parse Phases

Scan the plan for phase headings. Phases are markdown headings (any level)
whose text starts with "Phase" (case-insensitive), e.g.:

```
## Phase 0 -- Specification and failing test
## Phase 1 -- VMM backend abstraction
### Phase 1.5 -- Guest rootfs
```

For each phase, extract:

- **Phase ID**: the heading text (e.g. "Phase 0 -- Specification and failing test")
- **Phase body**: all content from this heading until the next phase heading or end of file
- **Acceptance gate**: any fenced code block under an "Acceptance gate" sub-heading within the phase body. If none exists, the phase has no automated gate.

Build an ordered list of phases. Present the list to the user and confirm
before proceeding.

## Step 2: Execute Phases Sequentially

Process each phase in order. **Never skip ahead.** Each phase goes through
the full cycle: Implement → Review → Remediate.

### 2a. Implement

Launch a **single sub-agent** (Task tool, `subagent_type="generalPurpose"`)
with a prompt that includes:

1. The full phase body (verbatim from the plan).
2. The acceptance gate commands (if any).
3. These instructions:

> You are implementing a phase of a larger plan. Your job:
>
> 1. Read and understand every task in this phase.
> 2. Implement each task. Follow the plan precisely — do not improvise
>    scope beyond what the phase describes.
> 3. After implementation, run the acceptance gate commands (if provided).
>    If any gate check fails, diagnose and fix until all gates pass.
> 4. When all tasks are done and gates pass, return a structured summary:
>    - **Completed tasks**: list of what you did
>    - **Files changed**: list of files created or modified
>    - **Gate results**: stdout/stderr of each gate command and pass/fail
>    - **Issues encountered**: anything surprising or unresolved

Wait for the sub-agent to complete. Record its summary.

If the sub-agent reports that acceptance gates failed after multiple attempts,
stop the orchestration and report the failure to the user with full context.

### 2b. Review

After successful implementation, launch **review agents in parallel** using
the Task tool. Launch one agent per review skill plus one principal-engineer
reviewer.

#### Discover review skills

List review skills dynamically:

```bash
ls -d .claude/skills/review-*/SKILL.md 2>/dev/null
```

For each skill found (e.g. `review-github-pr`, `review-security-issue`),
launch a Task sub-agent (`subagent_type="generalPurpose"`) whose prompt
instructs it to:

1. Read the skill file at `.claude/skills/<skill-name>/SKILL.md`.
2. Apply the skill's review methodology to the changes made in this phase.
3. Since there is no PR yet, review the **current branch diff** against the
   base branch: `git diff main...HEAD` (or the appropriate base).
4. Return findings in this format:

> ## Review: <skill-name>
>
> ### Critical
> - <finding with file:line reference>
>
> ### High
> - <finding with file:line reference>
>
> ### Medium
> - <finding>
>
> ### Low / Informational
> - <finding>

#### Principal-engineer reviewer

Additionally, launch one Task sub-agent with
`subagent_type="principal-engineer-reviewer"` whose prompt includes:

1. The phase body from the plan.
2. The implementation summary from step 2a.
3. Instruction to review the diff (`git diff main...HEAD`) for:
   - Architectural soundness
   - Correctness and edge cases
   - Security implications
   - Performance concerns
   - Code quality and maintainability
4. Return findings in the same Critical/High/Medium/Low format above.

**Launch all review agents in a single message** so they run concurrently.

### 2c. Aggregate Feedback

Once all review agents return, merge their findings into a single report
grouped by severity:

```
## Phase <id> — Review Summary

### Critical (must fix before proceeding)
- [review-github-pr] <finding>
- [principal-engineer] <finding>

### High (should fix before proceeding)
- [review-security-issue] <finding>

### Medium (fix if straightforward, otherwise note for later)
- ...

### Low / Informational (no action required)
- ...
```

Present this summary to the user.

### 2d. Remediate

If there are **any Critical or High findings**, launch a remediation
sub-agent (Task tool, `subagent_type="generalPurpose"`) with a prompt that
includes:

1. The aggregated Critical and High findings (verbatim).
2. The list of files changed in step 2a.
3. These instructions:

> You are remediating review findings for a completed implementation phase.
>
> 1. Address every Critical finding. These are blocking.
> 2. Address every High finding. These are strongly recommended.
> 3. Do NOT address Medium or Low findings unless they are trivial one-line
>    fixes adjacent to code you are already changing.
> 4. After remediation, re-run the acceptance gate commands (if any) to
>    confirm nothing regressed.
> 5. Return a structured summary:
>    - **Findings addressed**: which findings you fixed and how
>    - **Findings deferred**: any High findings you chose not to address, with
>      justification
>    - **Gate results**: pass/fail after remediation

Wait for the remediation agent to complete.

If the remediation agent reports that gates broke or Critical findings
could not be resolved, stop and escalate to the user.

If there are **no Critical or High findings**, skip remediation and proceed.

### 2e. User Checkpoint

After remediation (or after reviews if no remediation was needed), pause and
prompt the user before advancing. Present:

1. The phase completion status line:
   ```
   ✅ Phase <id> complete. <N> files changed, <M> findings remediated.
   ```
2. Any **Medium** findings that were deferred.
3. Any **High** findings the remediation agent chose to defer with
   justification.

Then ask:

> **Ready to proceed to Phase <next>?**
> - **Continue** — advance to the next phase
> - **Address issues** — you will describe what to fix; I will launch a
>   sub-agent to address it before moving on

If the user chooses **Address issues**, wait for their instructions, then
launch a sub-agent (`subagent_type="generalPurpose"`) to carry out the
requested changes. After the sub-agent completes, re-present the checkpoint
prompt.

If the user chooses **Continue**, proceed to the next phase (back to
step 2a).

## Step 3: Plan Complete

After all phases are done, print a final summary:

```
## Orchestration Complete

| Phase | Files Changed | Reviews | Critical/High Fixed |
|-------|--------------|---------|---------------------|
| Phase 0 | 3 | 3 | 1 |
| Phase 1 | 8 | 3 | 2 |
| ... | ... | ... | ... |

Total files changed: <N>
Total findings remediated: <M>
```

## Error Handling

- **Sub-agent timeout or crash**: Report which phase and step failed. Do not
  retry automatically — ask the user how to proceed.
- **Acceptance gate persistent failure**: After the implementation agent
  reports it cannot pass gates, stop and present the gate output to the user.
- **Review skill not found**: If `ls .claude/skills/review-*/SKILL.md` returns
  nothing, skip skill-based reviews and only run the principal-engineer
  reviewer. Warn the user.

## Important Constraints

- **Do not implement code yourself.** You are the orchestrator. All code
  changes go through sub-agents.
- **Do not skip reviews.** Every phase gets the full review cycle.
- **Do not skip phases.** Phases are sequential by design.
- **Preserve sub-agent output.** Keep implementation summaries and review
  findings available for the user to inspect.
