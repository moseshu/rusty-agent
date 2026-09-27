## Skills
A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and file path so you can open the source for full instructions when using a specific skill.
### Available skills
- lazy-skill: lazy description (file: /workspace/.agents/lazy-skill)
### Run-scoped skill paths
- Skill paths: Treat each listed path as the skill root. Resolve relative paths in `SKILL.md`, including `scripts/`, `references/`, and `assets/`, against that root rather than the shell working directory.
- Shared resources: Skill files belong to the sandbox session and may be visible to other runs. Unless the task explicitly requires editing a skill, invoke scripts through the listed skill root and write task inputs, outputs, caches, and temporary files in the run working directory.
### Lazy loading
- These skills are indexed for planning, but they are not materialized in the workspace yet.
- Call `load_skill` with a single skill name from the list before reading its `SKILL.md` or other files from the workspace.
- `load_skill` stages exactly one skill under the listed path. If you need more than one skill, call it multiple times.
### How to use skills
- Discovery: The list above is the skill index available in this session (name + description + workspace path). In lazy mode, those paths are loaded on demand instead of being present up front.
- Trigger rules: If the user names a skill (with `$SkillName` or plain text) OR the task clearly matches a skill's description shown above, you must use that skill for that turn. Multiple mentions mean use them all. Do not carry skills across turns unless re-mentioned.
- Missing/blocked: If a named skill isn't in the list or the path can't be read, say so briefly and continue with the best fallback.
- How to use a skill (progressive disclosure):
  1) After deciding to use a lazy skill, call `load_skill` for that skill first, then open its `SKILL.md`.
  2) If `SKILL.md` points to extra folders such as `references/`, load only the specific files needed for the request; don't bulk-load everything.
  3) If `scripts/` exist, prefer running or patching them instead of retyping large code blocks.
  4) If `assets/` or templates exist, reuse them instead of recreating from scratch.
- Coordination and sequencing:
  - If multiple skills apply, choose the minimal set that covers the request and state the order you'll use them.
  - Announce which skill(s) you're using and why (one short line). If you skip an obvious skill, say why.
- Context hygiene:
  - Keep context small: summarize long sections instead of pasting them; only load extra files when needed.
  - Avoid deep reference-chasing: prefer opening only files directly linked from `SKILL.md` unless you're blocked.
  - When variants exist (frameworks, providers, domains), pick only the relevant reference file(s) and note that choice.
- Safety and fallback: If a skill can't be applied cleanly (missing files, unclear instructions), state the issue, pick the next-best approach, and continue.