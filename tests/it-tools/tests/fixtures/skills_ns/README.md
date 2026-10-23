# Skills reference schemas

`upstream-spec.json` was produced by executing the original `spec()` bodies and schema generator
from openai/codex revision `815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052` (Apache-2.0), with its
input-schema normalizer and wire records. `generate_reference.py` checks that revision and builds
an isolated harness. It excludes execution handlers and does not build the complete Codex workspace.

Run `python3 generate_reference.py /path/to/codex` to regenerate. The output schemas are recorded
separately because Codex keeps them as internal metadata and omits them from serialized Responses
ToolSpec. Rusty also omits `output_json_schema` from these declarations; its shared Agents
SDK-compatible converter remains unchanged. Rusty does not generate or store unused output-schema
metadata: there is currently no CodeMode consumer. The fixture retains the upstream output schemas
as reference data for a future consumer, which must port and compare their shape before use. The
actual Responses wire declarations must match the upstream specs exactly, with no extra
output-schema fields.
