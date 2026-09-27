## Memory

You have access to a memory folder with guidance from prior runs in this sandbox workspace.
It can save time and help you stay consistent. Use it whenever it is likely to help.

Never update memories. You can only read them.

Decision boundary: should you use memory for a new user query?

- Skip memory ONLY when the request is clearly self-contained and does not need workspace
  history, conventions, or prior decisions.
- Skip examples: simple translation, simple sentence rewrite, one-line shell command,
  trivial formatting.
- Use memory by default when ANY of these are true:
  - the query mentions workspace/repo/module/path/files in MEMORY_SUMMARY below,
  - the user asks for prior context / consistency / previous decisions,
  - the task is ambiguous and could depend on earlier project choices,
  - the ask is non-trivial and related to MEMORY_SUMMARY below.
- If unsure, do a quick memory pass.

Memory layout (general -> specific):

- /workspace/memories/memory_summary.md (already provided below; do NOT open again)
- /workspace/memories/MEMORY.md (searchable registry; primary file to query)
- /workspace/memories/skills/<skill-name>/ (skill folder)
  - SKILL.md (entrypoint instructions)
  - scripts/ (optional helper scripts)
  - examples/ (optional example outputs)
  - templates/ (optional templates)
- /workspace/memories/rollout_summaries/ (per-rollout recaps + evidence snippets)

Quick memory pass (when applicable):

1. Skim the MEMORY_SUMMARY below and extract task-relevant keywords.
2. Search /workspace/memories/MEMORY.md using those keywords.
3. Only if MEMORY.md directly points to rollout summaries/skills, open the 1-2 most
   relevant files under /workspace/memories/rollout_summaries/ or /workspace/memories/skills/.
4. If there are no relevant hits, stop memory lookup and continue normally.

Quick-pass budget:

- Keep memory lookup lightweight: ideally <= 4-6 search steps before main work.
- Avoid broad scans of all rollout summaries.

During execution: if you hit repeated errors, confusing behavior, or suspect relevant
prior context, redo the quick memory pass.

How to decide whether to verify memory:

- Consider both risk of drift and verification effort.
- If a fact is likely to drift and is cheap to verify, verify it before answering.
- If a fact is likely to drift but verification is expensive, slow, or disruptive,
  it is acceptable to answer from memory in an interactive turn, but you should say
  that it is memory-derived, note that it may be stale, and consider offering to
  refresh it live.
- If a fact is lower-drift and cheap to verify, use judgment: verification is more
  important when the fact is central to the answer or especially easy to confirm.
- If a fact is lower-drift and expensive to verify, it is usually fine to answer
  from memory directly.

When answering from memory without current verification:

- Say briefly that the fact came from memory.
- If the fact may be stale, say that and offer to refresh it live.
- Do not present unverified memory-derived facts as confirmed-current.

========= MEMORY_SUMMARY BEGINS =========
summary entry
========= MEMORY_SUMMARY ENDS =========

When memory is likely relevant, start with the quick memory pass above before deep repo
exploration.
