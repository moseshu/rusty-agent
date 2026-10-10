//! Ported from the tests of Codex's `shell-command/src/bash.rs`.

use ra_coding::shell_command::{parse_shell_lc_plain_commands, parse_shell_script_into_commands};

fn parse_seq(src: &str) -> Option<Vec<Vec<String>>> {
    parse_shell_script_into_commands(src)
}

#[test]
fn accepts_single_simple_command() {
    let cmds = parse_seq("ls -1").unwrap();
    assert_eq!(cmds, vec![vec!["ls".to_string(), "-1".to_string()]]);
}

#[test]
fn accepts_multiple_commands_with_allowed_operators() {
    let src = "ls && pwd; echo 'hi there' | wc -l";
    let cmds = parse_seq(src).unwrap();
    let expected: Vec<Vec<String>> = vec![
        vec!["ls".to_string()],
        vec!["pwd".to_string()],
        vec!["echo".to_string(), "hi there".to_string()],
        vec!["wc".to_string(), "-l".to_string()],
    ];
    assert_eq!(cmds, expected);
}

#[test]
fn extracts_double_and_single_quoted_strings() {
    let cmds = parse_seq("echo \"hello world\"").unwrap();
    assert_eq!(
        cmds,
        vec![vec!["echo".to_string(), "hello world".to_string()]]
    );

    let cmds2 = parse_seq("echo 'hi there'").unwrap();
    assert_eq!(
        cmds2,
        vec![vec!["echo".to_string(), "hi there".to_string()]]
    );
}

#[test]
fn accepts_double_quoted_strings_with_newlines() {
    let cmds = parse_seq("git commit -m \"line1\nline2\"").unwrap();
    assert_eq!(
        cmds,
        vec![vec![
            "git".to_string(),
            "commit".to_string(),
            "-m".to_string(),
            "line1\nline2".to_string(),
        ]]
    );
}

#[test]
fn accepts_mixed_quote_concatenation() {
    assert_eq!(
        parse_seq(r#"echo "/usr"'/'"local"/bin"#).unwrap(),
        vec![vec!["echo".to_string(), "/usr/local/bin".to_string()]]
    );
    assert_eq!(
        parse_seq(r#"echo '/usr'"/"'local'/bin"#).unwrap(),
        vec![vec!["echo".to_string(), "/usr/local/bin".to_string()]]
    );
}

#[test]
fn rejects_double_quoted_strings_with_expansions() {
    assert!(parse_seq(r#"echo "hi ${USER}""#).is_none());
    assert!(parse_seq(r#"echo "$HOME""#).is_none());
}

#[test]
fn accepts_numbers_as_words() {
    let cmds = parse_seq("echo 123 456").unwrap();
    assert_eq!(
        cmds,
        vec![vec![
            "echo".to_string(),
            "123".to_string(),
            "456".to_string()
        ]]
    );
}

#[test]
fn rejects_parentheses_and_subshells() {
    assert!(parse_seq("(ls)").is_none());
    assert!(parse_seq("ls || (pwd && echo hi)").is_none());
}

#[test]
fn rejects_redirections_and_unsupported_operators() {
    assert!(parse_seq("ls > out.txt").is_none());
    assert!(parse_seq("echo hi & echo bye").is_none());
}

#[test]
fn rejects_command_and_process_substitutions_and_expansions() {
    assert!(parse_seq("echo $(pwd)").is_none());
    assert!(parse_seq("echo `pwd`").is_none());
    assert!(parse_seq("echo $HOME").is_none());
    assert!(parse_seq("echo \"hi $USER\"").is_none());
}

#[test]
fn rejects_runtime_expansion_in_plain_words() {
    for script in [
        "find . -{delete,print}",
        "rg --pre{=,=sh} pattern payload.sh",
        "find . -del*",
        "find . -delet?",
        "find . -delet[e]",
        r"find . -de\lete",
        "echo ~",
        "echo ~HOME",
        "echo HEAD~1",
        "echo HEAD^",
        "echo file~",
        "echo =sh",
        "echo foo^bar",
        "echo foo#bar",
        "l* -l",
    ] {
        for shell in ["bash", "zsh"] {
            for flag in ["-c", "-lc"] {
                let command = [shell, flag, script].map(str::to_owned);
                assert_eq!(parse_shell_lc_plain_commands(&command), None, "{command:?}");
            }
        }
    }
}

#[test]
fn preserves_quoted_literals() {
    for (script, expected) in [
        (r#"rg -g"*.py" pattern"#, vec!["rg", "-g*.py", "pattern"]),
        (r#"echo "\n""#, vec!["echo", r"\n"]),
        (
            r#"echo "~HOME" 'HEAD~1' "HEAD^" 'foo#bar' "=sh" 'file~'"#,
            vec![
                "echo", "~HOME", "HEAD~1", "HEAD^", "foo#bar", "=sh", "file~",
            ],
        ),
        (
            r#"echo -"{a,b}" '*?[]~^#=\\'"#,
            vec!["echo", "-{a,b}", r"*?[]~^#=\\"],
        ),
    ] {
        let expected = vec![expected.into_iter().map(str::to_owned).collect::<Vec<_>>()];
        assert_eq!(
            parse_shell_script_into_commands(script),
            Some(expected),
            "{script:?}"
        );
    }
}

#[test]
fn rejects_double_quoted_escapes() {
    for script in [
        r#"echo "\$HOME\`\"\\\n""#,
        "find . \"-de\\\nlete\"",
        r#"echo "\\""#,
    ] {
        assert_eq!(parse_seq(script), None, "{script:?}");
        for shell in ["bash", "zsh"] {
            let command = [shell, "-lc", script].map(str::to_owned);
            assert_eq!(parse_shell_lc_plain_commands(&command), None, "{command:?}");
        }
    }
}

#[test]
fn rejects_variable_assignment_prefix() {
    assert!(parse_seq("FOO=bar ls").is_none());
}

#[test]
fn rejects_trailing_operator_parse_error() {
    assert!(parse_seq("ls &&").is_none());
}

#[test]
fn rejects_empty_command_position_with_leading_operator() {
    assert!(parse_seq("&& ls").is_none());
}

#[test]
fn rejects_empty_command_position_with_double_separator() {
    assert!(parse_seq("ls ;; pwd").is_none());
}

#[test]
fn rejects_empty_command_position_with_empty_pipeline_segment() {
    assert!(parse_seq("ls | | wc").is_none());
}

#[test]
fn parse_zsh_lc_plain_commands() {
    let command = vec!["zsh".to_string(), "-lc".to_string(), "ls".to_string()];
    let parsed = parse_shell_lc_plain_commands(&command).unwrap();
    assert_eq!(parsed, vec![vec!["ls".to_string()]]);
}

#[test]
fn accepts_concatenated_flag_and_value() {
    // Test case: -g"*.py" (flag directly concatenated with quoted value)
    let cmds = parse_seq("rg -n \"foo\" -g\"*.py\"").unwrap();
    assert_eq!(
        cmds,
        vec![vec![
            "rg".to_string(),
            "-n".to_string(),
            "foo".to_string(),
            "-g*.py".to_string(),
        ]]
    );
}

#[test]
fn accepts_concatenated_flag_with_single_quotes() {
    let cmds = parse_seq("grep -n 'pattern' -g'*.txt'").unwrap();
    assert_eq!(
        cmds,
        vec![vec![
            "grep".to_string(),
            "-n".to_string(),
            "pattern".to_string(),
            "-g*.txt".to_string(),
        ]]
    );
}

#[test]
fn rejects_concatenation_with_variable_substitution() {
    // Environment variables in concatenated strings should be rejected
    assert!(parse_seq("rg -g\"$VAR\" pattern").is_none());
    assert!(parse_seq("rg -g\"${VAR}\" pattern").is_none());
}

#[test]
fn rejects_concatenation_with_command_substitution() {
    // Command substitution in concatenated strings should be rejected
    assert!(parse_seq("rg -g\"$(pwd)\" pattern").is_none());
    assert!(parse_seq("rg -g\"$(echo '*.py')\" pattern").is_none());
}
