#!/usr/bin/env python3
"""Execute the pinned Codex catalog renderer, prompt wording, aliases and allocator.

The harness substitutes catalog/environment records and the contextual-fragment trait boundary.
It compiles the original renderer (including its private modules), catalog_prompt.rs, aliases.rs,
AvailableSkillsInstructions and fragment rendering. No Rusty implementation supplies expected text.
Host discovery and multi-catalog allocation are outside these single-catalog fixture cases.
"""
import json
from pathlib import Path
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
    depth, end = 1, opening + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[start:end]


def entry(index=0, *, kind="executor", root="skill://root", description="Fix lint errors.", **extra):
    package = f"{root}/skill-{index}"
    return dict(id=package, authority=dict(kind=kind, id="root" if kind == "executor" else "codex_apps"), name=f"skill-{index}", description=description, main_prompt=f"{package}/SKILL.md", **extra)


def case(name, entries, context_window=None, max_context_tokens=None, usage=False):
    return dict(name=name, entries=entries, context_window=context_window, max_context_tokens=max_context_tokens, usage=usage)


long_root = "skill://root/" + "long-shared-package-prefix/" * 5
cases = [
    case("empty", []),
    case("executor", [entry()]),
    case("usage", [entry()], usage=True),
    case("cloud", [entry(kind="cloud")], usage=True),
    case("short-description", [entry(description="Long purpose.", short_description="Concise purpose.")]),
    case("hidden-disabled", [entry(0, enabled=False), entry(1, prompt_visible=False)]),
    case("visible-order", [entry(2), entry(0), entry(1)]),
    case("mixed-sources", [entry(), entry(1, kind="cloud")], usage=True),
    case("character-fallback-multibyte", [entry(i, description="💡" * 1024) for i in range(2)]),
    case("description-limit", [entry(description="技" * 1025)]),
    case("token-minimum", [entry(description="abcdefghij")], max_context_tokens=20),
    case("omission-marker", [entry(i, description="A long description for metadata budgeting.") for i in range(20)], max_context_tokens=100),
    case("all-minimum-lines-exceed-budget", [entry(root="skill://" + "x" * 512, description="x" * 1024)], max_context_tokens=100),
    case("no-fragment-fits", [entry(root="skill://" + "x" * 512, description="x" * 1024)], max_context_tokens=1),
    case("resource-aliases", [entry(i, root=long_root, alias_root=long_root) for i in range(12)], usage=True),
    case("alias-budget-pressure", [entry(i, root=long_root, alias_root=long_root) for i in range(12)], max_context_tokens=230, usage=True),
    case("alias-overhead-too-large", [entry(alias_root="skill://" + "unused-long-root/" * 40)], max_context_tokens=100, usage=True),
    case("alias-order", [entry(i, root=f"{long_root}{2-i}", alias_root=f"{long_root}{2-i}", alias_root_order=2-i) for i in range(3)], usage=True),
    case("literal-backslash", [entry(i, root=long_root + r"foo\bar", alias_root=long_root + r"foo\bar") for i in range(4)]),
]
for window, limit in [(100000, None), (400000, None), (None, None), (0, None), (-1, None), (1, None), (100000, 5000), (None, 50000), (None, 0)]:
    cases.append(case(f"budget-{window}-{limit}", [entry(i, description="技" * 1024) for i in range(20)], context_window=window, max_context_tokens=limit))

catalog = r'''
use serde::Deserialize;
#[derive(Clone,Debug,PartialEq,Eq,Deserialize)] #[serde(rename_all="snake_case")]
pub enum SkillSourceKind { Host, Executor, Cloud, Custom(String) }
#[derive(Clone,Debug,Deserialize)] pub struct SkillAuthority { pub kind: SkillSourceKind, pub id:String }
#[derive(Clone,Debug,Deserialize)] pub struct SkillPackageId(pub String);
#[derive(Clone,Debug,Deserialize)] pub struct SkillResourceId(String);
impl SkillResourceId { pub fn as_str(&self)->&str { &self.0 } }
#[derive(Clone,Debug,Deserialize)] pub struct SkillCatalogEntry {
 pub id:SkillPackageId,pub authority:SkillAuthority,pub name:String,pub description:String,
 #[serde(default)] pub short_description:Option<String>,pub main_prompt:SkillResourceId,
 #[serde(default)] pub display_path:Option<String>,#[serde(default)] alias_root:Option<String>,
 #[serde(default)] alias_root_order:Option<usize>,#[serde(default="yes")] pub enabled:bool,
 #[serde(default="yes")] pub prompt_visible:bool,
}
fn yes()->bool{true}
impl SkillCatalogEntry {
 pub fn is_model_visible(&self)->bool{self.enabled && self.prompt_visible}
 pub fn prompt_scope(&self)->Option<crate::protocol::SkillScope>{None}
 pub fn rendered_path(&self)->&str{self.display_path.as_deref().unwrap_or(self.main_prompt.as_str())}
 pub fn alias_root(&self)->Option<&str>{self.alias_root.as_deref()}
 pub fn alias_root_order(&self)->Option<usize>{self.alias_root_order}
}
#[derive(Default,Deserialize)] pub struct SkillCatalog {pub entries:Vec<SkillCatalogEntry>}
'''
fragment = (skills / "fragments.rs").read_text()
fragment = fragment[fragment.index("#[derive(Clone, Debug, Eq, PartialEq)]"):fragment.index("#[derive(Clone, Debug, Eq, PartialEq)]\npub(crate) struct SkillInstructions")]
fragment = 'use crate::{ContextualUserFragment,ContentItemKind,SKILLS_INSTRUCTIONS_OPEN_TAG,SKILLS_INSTRUCTIONS_CLOSE_TAG}; use crate::catalog_prompt::{SkillPromptKind,render_available_skills_body};\n' + fragment
trait_source = (upstream / "codex-rs/context-fragments/src/fragment.rs").read_text()
source = r'''
#![allow(dead_code)]
extern crate self as codex_protocol;
extern crate self as codex_utils_string;
pub mod protocol { #[derive(Clone,Copy)] pub enum SkillScope{System,Admin,Repo,User} }
pub struct ContentItemKind(String);
const SKILLS_INSTRUCTIONS_OPEN_TAG:&str="<skills_instructions>";
const SKILLS_INSTRUCTIONS_CLOSE_TAG:&str="</skills_instructions>";
const APPROX_BYTES_PER_TOKEN:usize=4;
trait ContextualUserFragment {
 fn role(&self)->&str;fn content_kind(&self)->ContentItemKind;
 fn markers(&self)->(&str,&str);fn type_markers()->(&'static str,&'static str);fn body(&self)->String;
'''
source += block(trait_source, "    fn render(&self)") + "\n}\n"
source += block((upstream / "codex-rs/utils/string/src/truncate.rs").read_text(), "pub fn approx_token_count") + "\n"
source += block((upstream / "codex-rs/utils/string/src/lib.rs").read_text(), "pub fn take_bytes_at_char_boundary") + "\n"
source += "mod catalog {\n" + catalog + "\n}\n"
source += 'mod host_aliases {pub fn shared_host_alias_roots(_: &[&crate::catalog::SkillCatalogEntry])->Vec<String>{Vec::new()}}\n'
for name in ["aliases", "catalog_prompt", "render"]:
    source += f'#[path={json.dumps(str(skills / (name + ".rs")))}] mod {name};\n'
source += "mod fragments {\n" + fragment + "\n}\n"
source += r'''
fn main() {
 let cases:Vec<serde_json::Value>=serde_json::from_reader(std::io::stdin()).unwrap();
 let result=cases.into_iter().map(|case| {
  let catalog:catalog::SkillCatalog=serde_json::from_value(serde_json::json!({"entries":case["entries"]})).unwrap();
  let limit=case["max_context_tokens"].as_u64().and_then(|n|std::num::NonZeroUsize::new(n as usize));
  let budget=render::skill_metadata_budget(case["context_window"].as_i64(),limit);
  let usage=case["usage"].as_bool().unwrap();
  let rendered=render::render_available_skills(&catalog,render::SkillCatalogRenderPolicy::ExtensionCompatible,budget,usage);
  let prompt=rendered.and_then(|r|r.into_fragment(usage)).map(|f|f.render());
  serde_json::json!({"case":case,"budget":budget,"prompt":prompt})
 }).collect::<Vec<_>>();
 println!("{}",serde_json::to_string_pretty(&result).unwrap());
}
'''
with tempfile.TemporaryDirectory(prefix="skills-catalog-reference-") as temporary:
    project = Path(temporary)
    (project / "src").mkdir()
    (project / "Cargo.toml").write_text('[package]\nname="skills-catalog-reference"\nversion="0.0.0"\nedition="2024"\n[dependencies]\nserde={version="=1.0.229",features=["derive"]}\nserde_json="=1.0.151"\ntracing={version="0.1",features=["attributes"]}\n')
    (project / "src/main.rs").write_text(source)
    process = subprocess.run(["cargo", "run", "--quiet", "--manifest-path", str(project / "Cargo.toml")], input=json.dumps(cases), text=True, capture_output=True)
    if process.returncode:
        print(process.stderr, file=sys.stderr)
        raise SystemExit(process.returncode)
    Path(__file__).with_name("upstream-render.json").write_text(process.stdout)
