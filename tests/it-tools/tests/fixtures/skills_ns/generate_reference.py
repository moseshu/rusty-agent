#!/usr/bin/env python3
"""Execute the pinned upstream spec bodies and schema generator in an isolated Rust harness.

Usage: python3 generate_reference.py /path/to/codex
Only dependency-heavy execution handlers are excluded. The upstream spec bodies, schema types,
input-schema normalization and wire records are compiled directly from the supplied checkout.
"""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

REVISION = "815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052"
upstream = Path(sys.argv[1]).resolve()
assert subprocess.check_output(["git", "-C", str(upstream), "rev-parse", "HEAD"], text=True).strip() == REVISION
skills = upstream / "codex-rs/ext/skills/src"


def block(text, marker):
    start = text.index(marker)
    opening = text.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[start:end]


mod = (skills / "tools/mod.rs").read_text()
source = "use schemars::JsonSchema; use serde::{Deserialize,Serialize}; use serde_json::Value;\n"
source += f'#[path={json.dumps(str(skills / "tools/schema.rs"))}] mod schema;\n'
source += "mod json_schema;\n"
source += 'use json_schema::{JsonSchema as InputSchema,parse_tool_input_schema};\n'
for name in ["SkillToolAuthoritySelector", "SkillToolAuthority"]:
    start = mod.rfind("#[derive", 0, mod.index("enum " + name + " {"))
    source += block(mod[start:], "#[derive") + "\n"
source += 'const SKILLS_NAMESPACE: &str = "skills"; type ToolOutputSchema = Value;\n'
wire = (upstream / "codex-rs/tools/src/responses_api.rs").read_text()
for name in ["ResponsesApiTool", "ResponsesApiNamespace", "ResponsesApiNamespaceTool"]:
    start = wire.rfind("#[derive", 0, wire.index("pub " + ("enum " if name.endswith("NamespaceTool") else "struct ") + name))
    record = block(wire[start:], "#[derive").replace("parameters: JsonSchema", "parameters: InputSchema")
    if name.endswith("NamespaceTool"):
        record = record[:record.index('    #[serde(rename = "custom")]')] + "}"
    source += record + "\n"
source += block(wire, "pub fn default_namespace_description") .replace('if namespace_name == DEFAULT_FUNCTION_NAMESPACE', 'if namespace_name == "functions"') + "\n"
source += '#[derive(Serialize)] #[serde(tag="type")] enum ToolSpec { #[serde(rename="namespace")] Namespace(ResponsesApiNamespace) }\n'
source += block(mod, "fn skill_function_tool") + "\n"
for name in ["list", "read"]:
    tool = (skills / f"tools/{name}.rs").read_text()
    definitions = tool[tool.index("#[derive(Deserialize, JsonSchema)]"):tool.index("#[derive(Clone)]\npub(super) struct")]
    source += f'mod {name} {{ use super::*; const TOOL_NAME: &str = "{name}";\n' + definitions
    source += block(tool, "fn spec(&self)").replace("fn spec(&self)", "pub fn spec()")
    source += '\npub fn output() -> Value { schema::output_schema_for::<' + name.title() + 'Response>() } }\n'
source += 'fn main() { println!("{}", serde_json::to_string_pretty(&serde_json::json!({"revision":"' + REVISION + '","specs":[list::spec(),read::spec()],"output_schemas":[list::output(),read::output()]})).unwrap()); }'
with tempfile.TemporaryDirectory(prefix="skills-reference-") as temporary:
    project = Path(temporary)
    (project / "src").mkdir()
    (project / "Cargo.toml").write_text('[package]\nname="skills-reference"\nversion="0.0.0"\nedition="2024"\n[dependencies]\nschemars="=0.8.22"\nserde={version="=1.0.229",features=["derive"]}\nserde_json="=1.0.151"\njsonptr="0.6"\nurlencoding="2"\n')
    shutil.copy(upstream / "codex-rs/tools/src/json_schema.rs", project / "src/json_schema.rs")
    shutil.copytree(upstream / "codex-rs/tools/src/json_schema", project / "src/json_schema")
    (project / "src/main.rs").write_text(source)
    result = subprocess.check_output(["cargo", "run", "--quiet", "--manifest-path", str(project / "Cargo.toml")], text=True)
    Path(__file__).with_name("upstream-spec.json").write_text(result)
