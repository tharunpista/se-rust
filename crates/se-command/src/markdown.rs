//! Markdown command definitions: parsing and argument substitution.
//!
//! A command file is optional `---`-delimited frontmatter followed by a
//! body, which is the prompt template. This module is a pure function
//! over strings -- it does no I/O and does not scan directories, so the
//! whole convention is testable without a filesystem. Discovery (which
//! directories are scanned, how nested namespaces become `a:b` paths)
//! and turning a [`CommandDef`] into a [`crate::CommandSpec`] are
//! separate tasks and deliberately absent here.
//!
//! **Frontmatter parser limitation.** This is a hand-rolled,
//! line-oriented parser, not YAML: the workspace has no YAML
//! dependency and adding one is an escalation. It understands exactly
//! `key: value`, `key: [a, b]`, and a `key:` followed by `- item`
//! lines. It does **not** understand nested mappings, block scalars
//! (`|`, `>`), anchors, multi-line flow collections, or comments. A
//! value is taken literally after the *first* colon, which is the
//! tolerance rule ported from `packages/core/src/config/markdown.ts`
//! at `9bbb8ce` (see `docs/reference/OPENCODE_SOURCE_AUDIT.md` §4.7):
//! `description: Use for: a thing` keeps `Use for: a thing` as the
//! value rather than losing the file. Unknown keys are ignored, with
//! their list items and indented continuation lines, because SE's own
//! catalogue has a long tail of frontmatter keys and a strict loader
//! would reject it.

/// A parsed Markdown command, before it becomes a registry entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandDef {
    /// The command path, supplied by the caller (from the file's
    /// location). Identity comes from here, never from frontmatter.
    pub path: String,
    /// The frontmatter `name`, if present. Kept separate and never
    /// allowed to override [`CommandDef::path`]: identity is the
    /// caller's to decide.
    pub declared_name: Option<String>,
    /// Empty when the file has no frontmatter, which is legal.
    pub description: String,
    pub argument_hint: Option<String>,
    /// Named arguments, in declaration order. `$name` binds to the
    /// positional argument at the same index.
    pub arguments: Vec<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// `None` means the file said nothing; the caller owns the default.
    pub disable_model_invocation: Option<bool>,
    pub user_invocable: Option<bool>,
    /// The prompt template.
    pub body: String,
}

/// A command file that could not be parsed. Never a panic and never a
/// silent skip: the caller reports it naming the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownError {
    /// The command path the caller passed in, so the message can name
    /// the offending definition.
    pub path: String,
    /// 1-based line number within the file.
    pub line: usize,
    pub reason: MarkdownErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownErrorKind {
    /// Frontmatter opened with `---` and never closed.
    UnterminatedFrontmatter,
    /// A frontmatter line that is neither `key: value`, a `- item`, nor
    /// an indented continuation.
    NotAKeyValue(String),
    /// `key: [a, b` -- an inline list with no closing bracket.
    UnterminatedList(String),
    /// A field that must be a boolean held something else.
    NotABoolean { key: String, value: String },
}

impl std::fmt::Display for MarkdownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "command /{} line {}: ", self.path, self.line)?;
        match &self.reason {
            MarkdownErrorKind::UnterminatedFrontmatter => {
                write!(f, "frontmatter opened with `---` but never closed")
            }
            MarkdownErrorKind::NotAKeyValue(l) => {
                write!(f, "frontmatter line is not `key: value`: {l:?}")
            }
            MarkdownErrorKind::UnterminatedList(k) => {
                write!(f, "inline list for `{k}` has no closing `]`")
            }
            MarkdownErrorKind::NotABoolean { key, value } => {
                write!(f, "`{key}` must be true or false, got {value:?}")
            }
        }
    }
}

impl std::error::Error for MarkdownError {}

/// Parses a command Markdown file. `path` is the command's slash path
/// (without the leading `/`) and is used only for identity and error
/// messages; nothing in the file can change it.
pub fn parse(path: &str, source: &str) -> Result<CommandDef, MarkdownError> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut def = CommandDef {
        path: path.to_string(),
        ..Default::default()
    };

    // CRLF is normalised away rather than handled per-branch: these
    // files are authored on Windows as often as not.
    let lines: Vec<&str> = source
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();

    if lines.first().map(|l| l.trim_end()) != Some("---") {
        // No frontmatter is legal: the whole file is the template and
        // the description is empty.
        def.body = lines.join("\n").trim_start_matches('\n').to_string();
        return Ok(def);
    }

    let close = lines
        .iter()
        .skip(1)
        .position(|l| l.trim_end() == "---")
        .map(|i| i + 1)
        .ok_or_else(|| MarkdownError {
            path: path.to_string(),
            line: 1,
            reason: MarkdownErrorKind::UnterminatedFrontmatter,
        })?;

    let err = |line: usize, reason: MarkdownErrorKind| MarkdownError {
        path: path.to_string(),
        line,
        reason,
    };
    let as_bool = |key: &str, value: &str, line: usize| match value {
        "true" | "yes" => Ok(true),
        "false" | "no" => Ok(false),
        other => Err(err(
            line,
            MarkdownErrorKind::NotABoolean {
                key: key.to_string(),
                value: other.to_string(),
            },
        )),
    };

    // The key a bare `- item` line belongs to, so an unknown key's list
    // items are ignored along with it.
    let mut pending_list_key: Option<String> = None;

    for (i, raw) in lines[1..close].iter().enumerate() {
        let line = i + 2; // 1-based, and line 1 is the opening `---`.
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }
        if let Some(item) = raw.trim_start().strip_prefix("- ") {
            if pending_list_key.as_deref() == Some("arguments") {
                def.arguments.push(unquote(item.trim()).to_string());
            }
            continue;
        }
        // An indented line that is not a dash item is a nested mapping
        // or a continuation; this parser does not model either, and
        // ignoring it is the tolerance rule, not a silent failure of a
        // field we claim to support.
        if raw.starts_with(' ') || raw.starts_with('\t') {
            continue;
        }

        let Some(colon) = raw.find(':') else {
            return Err(err(line, MarkdownErrorKind::NotAKeyValue(raw.to_string())));
        };
        // First colon separates; everything after is the value, colons
        // and all. This is the §4.7 tolerance.
        let key = raw[..colon].trim().to_ascii_lowercase();
        let value = raw[colon + 1..].trim();
        pending_list_key = if value.is_empty() {
            Some(key.clone())
        } else {
            None
        };

        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "name" => def.declared_name = Some(unquote(value).to_string()),
            "description" => def.description = unquote(value).to_string(),
            "argument-hint" => def.argument_hint = Some(unquote(value).to_string()),
            "agent" => def.agent = Some(unquote(value).to_string()),
            "model" => def.model = Some(unquote(value).to_string()),
            "disable-model-invocation" => {
                def.disable_model_invocation = Some(as_bool(&key, value, line)?)
            }
            "user-invocable" => def.user_invocable = Some(as_bool(&key, value, line)?),
            "arguments" => {
                def.arguments = parse_inline_list(&key, value, line).map_err(|(l, r)| err(l, r))?
            }
            _ => {}
        }
    }

    def.body = lines[close + 1..]
        .join("\n")
        .trim_start_matches('\n')
        .to_string();
    Ok(def)
}

fn unquote(value: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    value
}

type ErrAt = (usize, MarkdownErrorKind);

fn parse_inline_list(key: &str, value: &str, line: usize) -> Result<Vec<String>, ErrAt> {
    let Some(inner) = value.strip_prefix('[') else {
        // A bare scalar where a list was expected is split on
        // whitespace and commas rather than refused -- taken as one
        // name it would declare an argument no `$name` can ever match,
        // which is a dead declaration rather than an error.
        return Ok(unquote(value)
            .split([',', ' ', '\t'])
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect());
    };
    let inner = inner
        .strip_suffix(']')
        .ok_or((line, MarkdownErrorKind::UnterminatedList(key.to_string())))?;
    Ok(inner
        .split(',')
        .map(|p| unquote(p.trim()).to_string())
        .filter(|p| !p.is_empty())
        .collect())
}

/// Splits raw argument text shell-style: `"hello world" second` is two
/// arguments. A backslash is *not* an escape outside quotes, so a
/// Windows path (`C:\Users\x`) survives -- no backslash is ever an
/// escape here, inside quotes or out. A quote only opens a quoted
/// section at the *start* of a token, so `don't` stays `don't` rather
/// than swallowing the rest of the line. An unterminated quote takes
/// the rest of the input as one argument rather than failing.
pub fn split_args(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in raw.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if (c == '"' || c == '\'') && !started => {
                // `""` is a present-but-empty argument.
                started = true;
                quote = Some(c);
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Substitutes arguments into a command's body, per
/// `docs/reference/CLAUDE_CODE_PUBLIC_UX_MAP.md` §1.
///
/// - `$ARGUMENTS` -- every argument as originally typed, quotes and all.
/// - `$ARGUMENTS[N]` and `$N` -- **0-based**, so `$0` is the *first*
///   argument.
/// - `$name` -- a name declared in `arguments`, bound positionally.
/// - `\$` renders a literal `$`; no other backslash is touched.
///
/// Two documented decisions:
///
/// 1. An index or declared name with no corresponding argument
///    substitutes **empty**, not an error. A command template that
///    mentions `$2` is a template that works with two arguments as
///    well as three, and failing the whole command over a missing
///    optional tail argument is worse than rendering nothing.
/// 2. If the template consumes no placeholder at all and arguments
///    were given, `ARGUMENTS: <raw>` is appended, so what the user
///    typed is never silently dropped. With no arguments the body is
///    returned unchanged.
///
/// The template is scanned once, left to right, and substituted text is
/// never rescanned -- a chain of string replacements would confuse `$1`
/// with `$10`, `$file` with `$filename`, and would re-expand a `$0` that
/// happened to appear inside an argument.
pub fn render(def: &CommandDef, raw_args: &str) -> String {
    let args = split_args(raw_args);
    let raw = raw_args.trim();
    let mut out = String::with_capacity(def.body.len() + raw.len());
    let mut consumed = false;

    let at = |i: Option<usize>| -> &str {
        i.and_then(|i| args.get(i))
            .map(String::as_str)
            .unwrap_or("")
    };

    let chars: Vec<char> = def.body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // A digit run stops at the first non-digit, so `$0_test.rs` is
        // argument 0 followed by literal `_test.rs` rather than an
        // unmatched `$0_test`. A name run takes identifier characters.
        let start = i + 1;
        let digits = chars.get(start).is_some_and(char::is_ascii_digit);
        let mut end = start;
        while end < chars.len()
            && (if digits {
                chars[end].is_ascii_digit()
            } else {
                chars[end].is_alphanumeric() || chars[end] == '_'
            })
        {
            end += 1;
        }
        let mut ident: String = chars[start..end].iter().collect();
        if ident.is_empty() {
            // A bare `$` is literal.
            out.push('$');
            i += 1;
            continue;
        }
        if ident == "ARGUMENTS" {
            // `$ARGUMENTS[N]`, else plain `$ARGUMENTS`. A malformed
            // `$ARGUMENTS[x]` is `$ARGUMENTS` followed by literal text.
            if chars.get(end) == Some(&'[') {
                if let Some(close) = chars[end + 1..].iter().position(|c| *c == ']') {
                    let idx: String = chars[end + 1..end + 1 + close].iter().collect();
                    if let Ok(n) = idx.parse::<usize>() {
                        out.push_str(at(Some(n)));
                        consumed = true;
                        i = end + close + 2;
                        continue;
                    }
                }
            }
            out.push_str(raw);
            consumed = true;
            i = end;
            continue;
        }
        if !digits {
            // A declared name may contain characters the identifier run
            // stops at (`pr-number`), so prefer the longest declared
            // name that matches here over the bare run.
            let rest: String = chars[start..].iter().collect();
            if let Some(longest) = def
                .arguments
                .iter()
                .filter(|a| !a.is_empty() && rest.starts_with(a.as_str()))
                .max_by_key(|a| a.chars().count())
            {
                end = start + longest.chars().count();
                ident = longest.clone();
            }
        }
        if let Ok(n) = ident.parse::<usize>() {
            out.push_str(at(Some(n)));
            consumed = true;
            i = end;
            continue;
        }
        if let Some(pos) = def.arguments.iter().position(|a| *a == ident) {
            out.push_str(at(Some(pos)));
            consumed = true;
            i = end;
            continue;
        }
        // An undeclared `$name` (`$HOME`, `$PATH`) stays literal: this
        // module is not a shell and must not eat text it does not own.
        out.push('$');
        out.push_str(&ident);
        i = end;
    }

    if !consumed && !raw.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\nARGUMENTS: ");
        out.push_str(raw);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(arguments: &[&str], body: &str) -> CommandDef {
        CommandDef {
            path: "t".to_string(),
            arguments: arguments.iter().map(|a| a.to_string()).collect(),
            body: body.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn every_documented_placeholder_form_substitutes_into_the_template() {
        let d = def(
            &["issue", "branch"],
            "all=[$ARGUMENTS] idx=[$ARGUMENTS[1]] short=[$0] named=[$branch]",
        );
        assert_eq!(
            render(&d, "123 main"),
            "all=[123 main] idx=[main] short=[123] named=[main]"
        );
    }

    #[test]
    fn dollar_zero_is_the_first_argument_not_the_second() {
        // Pinned deliberately. The binding convention is 0-BASED (see
        // CLAUDE_CODE_PUBLIC_UX_MAP.md §1): `$0` is the FIRST argument
        // and `$1` is the SECOND. A shell-style or OpenCode-style
        // reading in which `$1` is the first argument would be WRONG
        // here, and this assertion is what catches that off-by-one.
        let d = def(&[], "first=$0 second=$1");
        assert_eq!(render(&d, "alpha beta"), "first=alpha second=beta");
    }

    #[test]
    fn a_quoted_argument_containing_spaces_stays_a_single_argument() {
        // The acceptance case named in IMPLEMENTATION_SEQUENCE.md
        // Phase 1: `cmd "hello world" second`.
        let d = def(&[], "0=[$0] 1=[$1]");
        assert_eq!(
            render(&d, "\"hello world\" second"),
            "0=[hello world] 1=[second]"
        );
        assert_eq!(split_args("\"hello world\" second").len(), 2);
    }

    #[test]
    fn an_escaped_dollar_sign_stays_literal_and_is_not_an_argument() {
        let d = def(&[], r"$0 costs \$1.00 today");
        // `\$1` must NOT bind to the second argument ("beta").
        assert_eq!(render(&d, "alpha beta"), "alpha costs $1.00 today");
    }

    #[test]
    fn a_backslash_that_is_not_escaping_a_dollar_is_left_alone() {
        let d = def(&[], r"path C:\Users\me and $0");
        assert_eq!(render(&d, r"C:\tmp"), r"path C:\Users\me and C:\tmp");
    }

    #[test]
    fn an_index_with_no_corresponding_argument_substitutes_empty_rather_than_failing() {
        let d = def(&["a", "b"], "[$0][$1][$7][$b]");
        assert_eq!(render(&d, "only"), "[only][][][]");
    }

    #[test]
    fn an_undeclared_dollar_name_and_a_bare_dollar_are_left_literal() {
        let d = def(&[], "$HOME costs $ and $0");
        assert_eq!(render(&d, "x"), "$HOME costs $ and x");
    }

    #[test]
    fn a_template_with_no_placeholder_at_all_still_renders() {
        let d = def(&[], "just review the code");
        // No arguments: the body is untouched.
        assert_eq!(render(&d, ""), "just review the code");
        // Arguments given but unconsumed are appended rather than lost.
        assert_eq!(
            render(&d, "src/lib.rs"),
            "just review the code\n\nARGUMENTS: src/lib.rs"
        );
    }

    #[test]
    fn an_argument_that_itself_contains_a_placeholder_is_not_re_expanded() {
        let d = def(&[], "$0|$1");
        assert_eq!(render(&d, "$1 beta"), "$1|beta");
    }

    #[test]
    fn a_two_digit_index_is_not_confused_with_a_one_digit_one() {
        let d = def(&[], "$10");
        let args = (0..11).map(|n| n.to_string()).collect::<Vec<_>>().join(" ");
        assert_eq!(render(&d, &args), "10");
    }

    #[test]
    fn every_frontmatter_field_round_trips_through_the_parser() {
        let src = "---\n\
                   name: review-pr\n\
                   description: Review a pull request\n\
                   argument-hint: <pr-number> [branch]\n\
                   arguments: [pr, branch]\n\
                   agent: code-reviewer\n\
                   model: anthropic/claude-opus-4\n\
                   disable-model-invocation: true\n\
                   user-invocable: false\n\
                   allowed-tools: Read, Grep\n\
                   ---\n\
                   Review PR $pr on $branch.\n";
        let d = parse("review-pr", src).unwrap();
        assert_eq!(d.path, "review-pr");
        assert_eq!(d.declared_name.as_deref(), Some("review-pr"));
        assert_eq!(d.description, "Review a pull request");
        assert_eq!(d.argument_hint.as_deref(), Some("<pr-number> [branch]"));
        assert_eq!(d.arguments, vec!["pr", "branch"]);
        assert_eq!(d.agent.as_deref(), Some("code-reviewer"));
        assert_eq!(d.model.as_deref(), Some("anthropic/claude-opus-4"));
        assert_eq!(d.disable_model_invocation, Some(true));
        assert_eq!(d.user_invocable, Some(false));
        assert_eq!(d.body, "Review PR $pr on $branch.\n");
        assert_eq!(render(&d, "42 main"), "Review PR 42 on main.\n");
    }

    #[test]
    fn a_dash_list_declares_named_arguments_the_same_way_an_inline_list_does() {
        let src = "---\narguments:\n  - pr\n  - branch\n---\n$pr/$branch";
        let d = parse("x", src).unwrap();
        assert_eq!(d.arguments, vec!["pr", "branch"]);
        assert_eq!(render(&d, "7 main"), "7/main");
    }

    #[test]
    fn an_unquoted_colon_inside_a_description_does_not_lose_the_file() {
        // Ported tolerance, OPENCODE_SOURCE_AUDIT.md §4.7: a strict
        // YAML parser rejects this line and the whole command vanishes.
        let src = "---\ndescription: Use for: a thing\n---\nbody";
        let d = parse("x", src).unwrap();
        assert_eq!(d.description, "Use for: a thing");
        assert_eq!(d.body, "body");
    }

    #[test]
    fn a_file_with_no_frontmatter_is_a_valid_command_with_an_empty_description() {
        let d = parse("plain", "Summarise $0 please").unwrap();
        assert_eq!(d.description, "");
        assert_eq!(d.body, "Summarise $0 please");
        assert_eq!(d.declared_name, None);
        // Quoted, because unquoted `the diff` is two arguments.
        assert_eq!(render(&d, "\"the diff\""), "Summarise the diff please");
    }

    #[test]
    fn a_horizontal_rule_in_a_body_without_frontmatter_is_body_not_a_delimiter() {
        let d = parse("x", "intro\n---\nmore").unwrap();
        assert_eq!(d.body, "intro\n---\nmore");
    }

    #[test]
    fn crlf_line_endings_and_a_byte_order_mark_parse_the_same_as_plain_lf() {
        let d = parse("x", "\u{feff}---\r\ndescription: hi\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(d.description, "hi");
        assert_eq!(d.body, "body\n");
    }

    #[test]
    fn an_unterminated_frontmatter_block_is_a_typed_error_naming_the_command() {
        let err = parse("broken", "---\ndescription: hi\nbody with no close").unwrap_err();
        assert_eq!(err.reason, MarkdownErrorKind::UnterminatedFrontmatter);
        assert_eq!(err.path, "broken");
        assert!(err.to_string().contains("broken"), "got {err}");
    }

    #[test]
    fn a_frontmatter_line_that_is_not_a_key_value_pair_is_a_typed_error_naming_the_command() {
        let err = parse("broken", "---\nthis is not yaml\n---\nbody").unwrap_err();
        assert_eq!(
            err.reason,
            MarkdownErrorKind::NotAKeyValue("this is not yaml".to_string())
        );
        assert_eq!(err.line, 2);
        assert!(err.to_string().contains("broken"), "got {err}");
    }

    #[test]
    fn a_non_boolean_value_for_a_boolean_field_is_a_typed_error_rather_than_a_guess() {
        let err = parse("broken", "---\nuser-invocable: maybe\n---\nb").unwrap_err();
        assert_eq!(
            err.reason,
            MarkdownErrorKind::NotABoolean {
                key: "user-invocable".to_string(),
                value: "maybe".to_string(),
            }
        );
    }

    #[test]
    fn an_inline_list_with_no_closing_bracket_is_a_typed_error_rather_than_a_truncated_list() {
        let err = parse("broken", "---\narguments: [pr, branch\n---\nb").unwrap_err();
        assert_eq!(
            err.reason,
            MarkdownErrorKind::UnterminatedList("arguments".to_string())
        );
    }

    #[test]
    fn frontmatter_name_never_overrides_the_path_the_caller_supplied() {
        // Identity is the caller's, from the file's location; a command
        // that could rename itself could squat another command's path.
        let d = parse("frontend:component", "---\nname: permissions\n---\nb").unwrap();
        assert_eq!(d.path, "frontend:component");
        assert_eq!(d.declared_name.as_deref(), Some("permissions"));
    }

    #[test]
    fn an_unknown_frontmatter_key_and_its_list_items_are_ignored_not_rejected() {
        let src = "---\nhooks:\n  - PreToolUse\nlicense: MIT\ndescription: ok\n---\nb";
        let d = parse("x", src).unwrap();
        assert_eq!(d.description, "ok");
        assert!(d.arguments.is_empty());
    }

    #[test]
    fn an_indexed_placeholder_stops_at_the_first_non_digit_so_adjacent_text_survives() {
        // `$0_test.rs` is argument 0 followed by literal text; a greedy
        // identifier run would read `0_test` and match nothing, then
        // append the arguments as if the template had ignored them.
        let d = def(&[], "src/$0_test.rs and $1st");
        assert_eq!(render(&d, "parser 21"), "src/parser_test.rs and 21st");
    }

    #[test]
    fn a_hyphenated_declared_argument_name_still_substitutes() {
        let d = def(&["pr-number"], "PR $pr-number");
        assert_eq!(render(&d, "42"), "PR 42");
    }

    #[test]
    fn an_apostrophe_inside_a_word_does_not_open_a_quoted_section() {
        // A quote only quotes at the start of a token; otherwise
        // `don't break it` collapses into one mangled argument.
        assert_eq!(split_args("don't break it"), vec!["don't", "break", "it"]);
    }

    #[test]
    fn a_whitespace_separated_scalar_declares_several_named_arguments() {
        // Read as a single name it would declare `pr branch`, which no
        // `$name` can ever match -- a dead declaration.
        let d = parse("x", "---\narguments: pr branch\n---\n$pr/$branch").unwrap();
        assert_eq!(d.arguments, vec!["pr", "branch"]);
        assert_eq!(render(&d, "7 main"), "7/main");
    }

    #[test]
    fn a_comment_line_in_frontmatter_is_skipped_rather_than_failing_the_file() {
        let d = parse("x", "---\n# authored by hand\ndescription: ok\n---\nb").unwrap();
        assert_eq!(d.description, "ok");
    }

    #[test]
    fn a_crlf_body_is_normalised_even_when_the_file_has_no_frontmatter() {
        let d = parse("x", "line one\r\nline two\r\n").unwrap();
        assert_eq!(d.body, "line one\nline two\n");
    }

    #[test]
    fn an_unterminated_quote_takes_the_rest_of_the_input_without_panicking() {
        assert_eq!(split_args("\"hello world"), vec!["hello world"]);
        assert_eq!(split_args("a \"\" b"), vec!["a", "", "b"]);
    }

    #[test]
    fn non_ascii_arguments_and_templates_do_not_panic_or_split_mid_character() {
        let d = def(&["ville"], "ciudad=$ville año=$0");
        assert_eq!(render(&d, "München"), "ciudad=München año=München");
    }
}
