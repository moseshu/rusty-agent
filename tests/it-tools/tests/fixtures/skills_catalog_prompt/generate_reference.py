#!/usr/bin/env python3
"""Execute the pinned Codex catalog renderer, prompt wording, aliases and allocator.

The harness substitutes catalog/environment records and the contextual-fragment trait boundary.
It compiles the original renderer (including its private modules), catalog_prompt.rs, aliases.rs,
AvailableSkillsInstructions and fragment rendering. No Rusty implementation supplies expected text.
The original world_state_catalogs.rs render_catalogs entry point and both budget helpers run
with previous_world_state=None. Each source returns its own marked fragment and report; this
harness never assembles an expected combined Rusty prompt or reimplements source selection.
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
    return dict(id=package, authority=dict(kind=kind, id="root" if kind == "executor" else "codex_apps"), name=extra.pop("name", f"skill-{index}"), description=description, main_prompt=f"{package}/SKILL.md", **extra)


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

for usage in [False, True]:
    for limit in [1, 80, 300, 1000, 10000]:
        host_root = "/home/user/plugins/cache/market/plugin/1.0.0/skills"
        hosts = [entry(i, kind="host", root=host_root, display_path=f"{host_root}/skill-{i}/SKILL.md", alias_root=host_root, name=f"host-{i}", short_description="Not used by Host", scope="repo") for i in range(12)]
        cases.append(case(f"host-{usage}-{limit}", hosts, max_context_tokens=limit, usage=usage))
        mixed = hosts + [entry(i, root=long_root, alias_root=long_root, name=f"executor-{i}") for i in range(8)] + [entry(i, kind="cloud", name=f"cloud-{i}") for i in range(5)]
        cases.append(case(f"combined-{usage}-{limit}", mixed, max_context_tokens=limit, usage=usage))
cases.append(case("cloud-prefers-qualified-name", [entry(kind="host", name="plugin:review"), entry(kind="executor", name="plugin:review"), entry(kind="cloud", name="plugin:review")]))
cases.append(case("cloud-keeps-unqualified-host", [entry(kind="host", name="review"), entry(kind="cloud", name="review")]))

def pressure_entries(kind, count):
    return [dict(id=f"{kind}/{kind}-skill-{index:02}", authority=dict(kind=kind, id="root" if kind == "executor" else "codex_apps"), name=f"{kind}-skill-{index:02}", description="x" * 1025, main_prompt=f"skill://{kind}/{kind}-skill-{index:02}/SKILL.md", display_path=f"skill://{kind}/{kind}-skill-{index:02}/SKILL.md") for index in range(count)]


# Preserve the upstream integration test's exact inventory and budgets. For each case, the
# reference runs the original entry point first with only Cloud, then with the filesystem ready.
for window in [None, 100000]:
    for executor_count, cloud_count, rebalance, all_fit in [(17, 82, False, True), (40, 5, True, True), (40, 160, False, False)]:
        record = case(f"upstream-pressure-{window}-{executor_count}-{cloud_count}", pressure_entries("executor", executor_count) + pressure_entries("cloud", cloud_count), context_window=window)
        record.update(expected_rebalance=rebalance, expected_all_fit=all_fit)
        cases.append(record)
    for mode in ["host", "mixed"]:
        filesystem = pressure_entries("host", 50) if mode == "host" else pressure_entries("host", 25) + pressure_entries("executor", 25)
        record = case(f"{mode}-rescue-{window}", filesystem + pressure_entries("cloud", 5), context_window=window)
        record.update(expected_rebalance=True, expected_all_fit=True)
        cases.append(record)
for kind in ["cloud", "executor", "host"]:
    cases.append(case(f"empty-other-sources-{kind}", pressure_entries(kind, 12), max_context_tokens=300))

for flag in ["enabled", "prompt_visible"]:
    cloud_entries = pressure_entries("cloud", 8)
    for cloud_entry in cloud_entries:
        cloud_entry[flag] = False
    cases.append(case(f"cloud-{flag}-false", pressure_entries("executor", 12) + pressure_entries("host", 12) + cloud_entries, max_context_tokens=300))
cases.append(case("cloud-cap-rounding-301", pressure_entries("executor", 8) + pressure_entries("host", 12) + pressure_entries("cloud", 5), max_context_tokens=301))

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
 #[serde(default)] scope:Option<crate::protocol::SkillScope>,
}
fn yes()->bool{true}
impl SkillCatalogEntry {
 pub fn is_model_visible(&self)->bool{self.enabled && self.prompt_visible}
 pub fn prompt_scope(&self)->Option<crate::protocol::SkillScope>{self.scope}
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
pub mod protocol { #[derive(Clone,Copy,Debug,serde::Deserialize)] #[serde(rename_all="snake_case")] pub enum SkillScope{System,Admin,Repo,User} }
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
catalog_source = (skills / "catalog.rs").read_text()
source += "mod catalog {\n" + catalog + "\n" + block(catalog_source, "impl SkillSourceKind") + "\n" + block(catalog_source, "impl std::fmt::Display for SkillSourceKind") + "\n}\n"
source += f'#[path={json.dumps(str(skills / "host_aliases.rs"))}] mod host_aliases;\n'
for name in ["aliases", "catalog_prompt", "render"]:
    source += f'#[path={json.dumps(str(skills / (name + ".rs")))}] mod {name};\n'
source += "mod fragments {\n" + fragment + "\n}\n"
# Compile the original production orchestration, including allocation selection/fingerprinting,
# deduplication, Cloud-first rendering and the rebalance probe. Only runtime/world-state services
# are substituted. No control-flow statement in these methods is rewritten.
world_state_source = (skills / "world_state_catalogs.rs").read_text()
source += "mod world_state { pub const CLOUD_SKILLS_WORLD_STATE_ID:&str=\"cloud_skills\"; }\n"
source += "mod world_state_catalogs {\nuse crate::{catalog::SkillCatalog,render::{AvailableSkillsRender,PreparedSkillCatalog,RenderedSkillCatalogs,SkillCatalogRenderPolicy,SkillMetadataBudget,render_available_skills,render_prepared_skill_catalogs}}; use serde::{Serialize,Deserialize};\n"
source += next(line for line in world_state_source.splitlines() if line.startswith("const FILESYSTEM_BUDGET_DIVISOR:")) + "\n"
for marker in ["pub(crate) struct CatalogBudgetAllocation", "enum CatalogKind", "pub(crate) enum CatalogStatus", "struct CatalogContribution", "pub(crate) struct CatalogContributions", "pub(crate) struct RenderedCatalogContribution"]:
    start = world_state_source.index(marker)
    attributes_start = world_state_source.rfind("\n\n", 0, start) + 2
    declaration = world_state_source[attributes_start:start] + block(world_state_source, marker)
    source += declaration + "\n"
source += r'''
struct Input<'a>{ previous_world_state:Option<&'a serde_json::Map<String,serde_json::Value>> }
struct Config{include_instructions:bool}
struct CatalogContext<'a>{input:Input<'a>,config:Config,metadata_budget:SkillMetadataBudget,include_usage:bool}
impl<'a> CatalogContext<'a> {
'''
for marker in ["    pub(crate) fn render_catalogs(", "    fn rebalance_to_retain_all_skills(", "    fn render_with_cloud_limit("]:
    source += block(world_state_source, marker) + "\n"
source += r'''
}
pub fn run(executor:SkillCatalog, cloud:SkillCatalog, host:SkillCatalog, budget:SkillMetadataBudget, usage:bool)->serde_json::Value {
 let contribution=|catalog|CatalogContribution{catalog,status:CatalogStatus::Enabled};
 let catalogs=CatalogContributions{executor:contribution(executor),cloud:contribution(cloud),host:contribution(host)};
 let context=CatalogContext{input:Input{previous_world_state:None},config:Config{include_instructions:true},metadata_budget:budget,include_usage:usage};
 let rendered=context.render_catalogs(catalogs);
 let records=rendered.into_iter().map(|contribution| {
  let kind=match contribution.kind{CatalogKind::Executor=>"executor",CatalogKind::Cloud=>"cloud",CatalogKind::Host=>"host"};
  let cost=contribution.rendered.as_ref().map_or(0,|rendered|rendered.metadata_cost(contribution.budget,usage));
  let report=contribution.rendered.as_ref().map(|rendered| {let r=&rendered.report; serde_json::json!({"total_count":r.total_count,"included_count":r.included_count,"omitted_count":r.omitted_count,"truncated_description_chars":r.truncated_description_chars,"truncated_description_count":r.truncated_description_count})});
  let prompt=contribution.rendered.and_then(|rendered|rendered.into_fragment(usage)).map(|fragment|crate::ContextualUserFragment::render(&fragment));
  serde_json::json!({"kind":kind,"budget":contribution.budget,"cloud_limit":contribution.allocation.cloud_limit,"metadata_cost":cost,"report":report,"prompt":prompt})
 }).collect::<Vec<_>>();
 serde_json::json!(records)
}
}
'''
source += r'''
fn main() {
 let cases:Vec<serde_json::Value>=serde_json::from_reader(std::io::stdin()).unwrap();
 let result=cases.into_iter().map(|case| {
  let catalog:catalog::SkillCatalog=serde_json::from_value(serde_json::json!({"entries":case["entries"]})).unwrap();
  let limit=case["max_context_tokens"].as_u64().and_then(|n|std::num::NonZeroUsize::new(n as usize));
  let budget=render::skill_metadata_budget(case["context_window"].as_i64(),limit);
  let usage=case["usage"].as_bool().unwrap();
  let split=|kind:catalog::SkillSourceKind| catalog::SkillCatalog{entries:catalog.entries.iter().filter(|e| e.authority.kind==kind).cloned().collect()};
  let executor=split(catalog::SkillSourceKind::Executor); let cloud=split(catalog::SkillSourceKind::Cloud); let host=split(catalog::SkillSourceKind::Host);
  let initial=world_state_catalogs::run(catalog::SkillCatalog::default(),split(catalog::SkillSourceKind::Cloud),catalog::SkillCatalog::default(),budget,usage);
  let catalogs=world_state_catalogs::run(executor,cloud,host,budget,usage);
  serde_json::json!({"case":case,"budget":budget,"catalogs":catalogs,"cloud_without_filesystem":initial[1]})
 }).collect::<Vec<_>>();
 println!("{}",serde_json::to_string_pretty(&result).unwrap());
}
'''
with tempfile.TemporaryDirectory(prefix="skills-catalog-reference-") as temporary:
    project = Path(temporary)
    (project / "src").mkdir()
    (project / "Cargo.toml").write_text('[package]\nname="skills-catalog-reference"\nversion="0.0.0"\nedition="2024"\n[dependencies]\nserde={version="=1.0.229",features=["derive"]}\nserde_json="=1.0.151"\ntracing={version="0.1",features=["attributes"]}\nblake3="1"\n')
    (project / "src/main.rs").write_text(source)
    process = subprocess.run(["cargo", "run", "--quiet", "--manifest-path", str(project / "Cargo.toml")], input=json.dumps(cases), text=True, capture_output=True)
    if process.returncode:
        print(process.stderr, file=sys.stderr)
        raise SystemExit(process.returncode)
    Path(__file__).with_name("upstream-render.json").write_text(process.stdout)
