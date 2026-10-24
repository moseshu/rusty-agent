"""Execute the pinned Codex parser and selector; no Rusty code supplies expected results."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile

REV = '815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052'
root = Path(sys.argv[1]).resolve()
assert subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip() == REV
mentions = (root / 'codex-rs/skills/src/mentions.rs').read_text().split('#[cfg(test)]')[0]
selection = (root / 'codex-rs/ext/skills/src/selection.rs').read_text()
start = selection.index('#[tracing::instrument(')
end = selection.index('pub(crate) fn collect_explicit_skill_mentions', start)
selection = selection[:start] + selection[end:]
selection = selection.replace('use codex_protocol::', 'use crate::codex_protocol::').replace('use codex_skills::', 'use crate::codex_skills::')
stubs = r'''
mod codex_protocol { pub mod user_input {
    #[derive(Clone)] pub enum UserInput { Text { text:String }, Skill { name:String, path:std::path::PathBuf }, Mention { name:String, path:String }, Image {}, LocalImage {}, Audio {}, LocalAudio {}, Other }
} }
mod catalog {
    #[derive(Clone,Debug,PartialEq,Eq,Hash)] pub struct SkillAuthority(pub usize);
    #[derive(Clone,Debug,PartialEq,Eq,Hash)] pub struct SkillPackageId(pub String);
    #[derive(Clone)] pub struct Resource(pub String);
    impl Resource { pub fn as_str(&self)->&str { &self.0 } }
    #[derive(Clone)] pub struct SkillCatalogEntry { pub authority:SkillAuthority, pub id:SkillPackageId, pub name:String, pub main_prompt:Resource, pub display_path:Option<String>, pub enabled:bool }
    pub struct SkillCatalog { pub entries:Vec<SkillCatalogEntry> }
}
'''
entries = [
    dict(name='alpha', path='/one/SKILL.md', authority=0, package='one', enabled=True),
    dict(name='alpha', path='/two/SKILL.md', authority=1, package='two', enabled=True),
    dict(name='plugin:beta', path='/beta/SKILL.md', authority=0, package='beta', enabled=True),
    dict(name='disabled', path='/disabled/SKILL.md', authority=0, package='disabled', enabled=False),
    dict(name='hidden', path='/hidden/SKILL.md', authority=0, package='hidden', enabled=True),
    dict(name='PATH', path='/env/SKILL.md', authority=0, package='env', enabled=True),
    dict(name='alpha_extra', path='/extra/SKILL.md', authority=0, package='extra', enabled=True),
]
cases = [
    dict(id='plain', inputs=[dict(text='$alpha')]),
    dict(id='duplicates', inputs=[dict(text='$alpha $alpha')]),
    dict(id='qualified', inputs=[dict(text='$plugin:beta')]),
    dict(id='hidden', inputs=[dict(text='$hidden')]),
    dict(id='disabled', inputs=[dict(text='$disabled')]),
    dict(id='case_sensitive', inputs=[dict(text='$Alpha')]),
    dict(id='environment_variables', inputs=[dict(text='$PATH [$HOME](/one/SKILL.md) $XDG_CONFIG_HOME')]),
    dict(id='linked_exact_path', inputs=[dict(text='[$alpha](/two/SKILL.md)')]),
    dict(id='linked_uri', inputs=[dict(text='[$alpha](skill:///two/SKILL.md)')]),
    dict(id='linked_unknown_no_fallback', inputs=[dict(text='[$alpha](/missing/SKILL.md)')]),
    dict(id='non_skill_link', inputs=[dict(text='[$alpha](app://one) [$alpha](plugin://one) [$alpha](mcp://one)')]),
    dict(id='plain_after_link', inputs=[dict(text='[$alpha](/two/SKILL.md) $alpha')]),
    dict(id='structured_blocks_plain', inputs=[dict(text='$alpha'),dict(name='alpha',path='/two/SKILL.md',kind='skill')]),
    dict(id='structured_missing_blocks_plain', inputs=[dict(name='alpha',path='/missing/SKILL.md',kind='mention'),dict(text='$alpha')]),
    dict(id='non_skill_structured_keeps_plain', inputs=[dict(name='alpha',path='app://one',kind='mention'),dict(text='$alpha')]),
    dict(id='spacing', inputs=[dict(text='[$alpha]   ( /two/SKILL.md )')]),
    dict(id='empty_link_falls_back', inputs=[dict(text='[$alpha]()')]),
    dict(id='ordinary_markdown', inputs=[dict(text='[alpha](/two/SKILL.md) alpha')]),
    dict(id='exact_boundaries', inputs=[dict(text='$alphas $alpha_extra')]),
    dict(id='unicode_and_sigils', inputs=[dict(text='中文💡' + '$'*256 + ' $alpha.skill')]),
]
host_selection = (root / 'codex-rs/skills/src/selection.rs').read_text().split('#[cfg(test)]')[0]
host_counts = (root / 'codex-rs/skills/src/name_counts.rs').read_text()
for module in ['codex_protocol', 'codex_utils_absolute_path', 'codex_utils_path_uri']:
    host_selection = host_selection.replace('use ' + module + '::', 'use crate::' + module + '::')
host_counts = host_counts.replace('use codex_utils_path_uri::', 'use crate::codex_utils_path_uri::')
stubs += r'''
mod codex_utils_absolute_path {
    pub struct AbsolutePathBuf(pub std::path::PathBuf);
    impl AbsolutePathBuf { pub fn relative_to_current_dir(path:&std::path::Path)->Result<Self,()> { Ok(Self(if path.is_absolute(){path.to_path_buf()}else{std::env::current_dir().unwrap().join(path)})) } }
}
mod codex_utils_path_uri {
    #[derive(Clone,Debug,PartialEq,Eq,Hash)] pub struct PathUri(pub std::path::PathBuf);
    impl PathUri {
        pub fn from_abs_path(path:&crate::codex_utils_absolute_path::AbsolutePathBuf)->Self { Self(path.0.clone()) }
        pub fn from_host_native_path(path:&str)->Result<Self,()> { Ok(Self(std::path::PathBuf::from(path))) }
    }
    pub struct LegacyAppPathString(String);
    impl LegacyAppPathString {
        pub fn from_string(path:&str)->Self {Self(path.into())}
        pub fn to_inferred_path_uri(&self)->Option<PathUri> {Some(PathUri(self.0.clone().into()))}
    }
}
#[derive(Clone)] pub struct SkillMetadata { pub name:String,pub path_to_skills_md:codex_utils_path_uri::PathUri }
struct Lookup {skills:Vec<SkillMetadata>,disabled:std::collections::HashSet<codex_utils_path_uri::PathUri>}
impl host_selection::ExplicitSkillLookup for Lookup {
    fn skills(&self)->&[SkillMetadata]{&self.skills}
    fn disabled_paths(&self)->&std::collections::HashSet<codex_utils_path_uri::PathUri>{&self.disabled}
    fn skill_discovery_path_for_path(&self,_:&codex_utils_path_uri::PathUri)->Option<&codex_utils_path_uri::PathUri>{None}
}
use codex_skills::{ToolMentionKind,ToolMentions,extract_tool_mentions,normalize_skill_path,tool_kind_for_path};
use name_counts::build_skill_name_counts;
'''
quote = lambda s: json.dumps(s, ensure_ascii=False)
main = 'use catalog::*; use codex_protocol::user_input::UserInput; fn main() { let catalog = SkillCatalog { entries: vec!['
for e in entries:
    main += 'SkillCatalogEntry{authority:SkillAuthority(%s),id:SkillPackageId(%s.into()),name:%s.into(),main_prompt:Resource(%s.into()),display_path:Some(%s.into()),enabled:%s},' % (e['authority'],quote(e['package']),quote(e['name']),quote(e['path']),quote(e['path']),str(e['enabled']).lower())
main += ']}; let lookup=Lookup {skills:catalog.entries.iter().map(|e|SkillMetadata{name:e.name.clone(),path_to_skills_md:codex_utils_path_uri::PathUri(e.main_prompt.0.clone().into())}).collect(),disabled:catalog.entries.iter().filter(|e|!e.enabled).map(|e|codex_utils_path_uri::PathUri(e.main_prompt.0.clone().into())).collect()};'
for case in cases:
    main += 'let inputs = vec!['
    for i in case['inputs']:
        if 'text' in i:
            main += 'UserInput::Text{text:%s.into()},' % quote(i['text'])
        else:
            variant = 'Skill' if i['kind']=='skill' else 'Mention'
            main += 'UserInput::%s{name:%s.into(),path:%s.into()},' % (variant,quote(i['name']),quote(i['path']))
    main += ']; let result=selection::collect_explicit_skill_mentions(&inputs,&catalog); let indices:Vec<_> = result.iter().map(|e|catalog.entries.iter().position(|c|c.authority==e.authority && c.id==e.id).unwrap()).collect(); println!("{:?}",indices);'
    main += 'let host=host_selection::collect_explicit_skill_mentions(&inputs,&lookup,&std::collections::HashMap::new()); let indices:Vec<_>=host.iter().map(|e|lookup.skills.iter().position(|s|s.path_to_skills_md==e.path_to_skills_md).unwrap()).collect();println!("{:?}",indices);' 
main += '}'
with tempfile.TemporaryDirectory(prefix='codex-skill-selection-') as directory:
    source = Path(directory)/'main.rs'
    source.write_text(stubs + '\nmod codex_skills {' + mentions + '}\nmod selection {' + selection + '}\nmod name_counts {' + host_counts + '}\nmod host_selection {' + host_selection + '}\n' + main)
    binary = Path(directory)/'reference'
    subprocess.run(['rustc','--edition=2024','-Awarnings',str(source),'-o',str(binary)],check=True)
    results = subprocess.check_output([str(binary)],text=True).splitlines()
for case, result in zip(cases, zip(results[::2], results[1::2], strict=True), strict=True):
    case['selected'] = json.loads(result[0])
    case['selected_host'] = json.loads(result[1])
    # HashSet iteration order is intentionally unspecified upstream; compare selected membership.
    case['selected'].sort()
Path(__file__).with_name('upstream-selection.json').write_text(json.dumps(dict(revision=REV,entries=entries,cases=cases),ensure_ascii=False,indent=2)+'\n')
