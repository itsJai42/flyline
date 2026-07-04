//! Fish host backend for the out-of-process `flyline-standalone` editor.
//!
//! Much simpler than zsh: fish exposes its completion engine headlessly.
//! `fish -c 'complete -C -- <buffer>'` prints `candidate\tdescription` lines
//! with the user's config and completions loaded, in a few milliseconds — no
//! pty daemon, no broker. Values are passed as argv (never interpolated into
//! scripts), so no shell injection is possible.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use crate::bash_funcs::{
    CommandWordInfo, CompletionFlags, ProgrammableCompleteReturn, find_quote_type,
};
use crate::shell::{
    HistoryRecord, ShellBackend, hostname_from_uname, run_with_timeout, shell_var_name,
};

/// Caps so a bad config.fish/completion can't hang flyline (fail-open on timeout).
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(10);
const DUMP_TIMEOUT: Duration = Duration::from_secs(10);

/// Block-structure builtins shown as syntax keywords (fish has no separate
/// reserved-word class; `builtin -n` lists these too).
const FISH_KEYWORDS: &[&str] = &[
    "and", "begin", "break", "case", "continue", "else", "end", "for", "function", "if", "not",
    "or", "return", "switch", "while",
];

/// Dumps the shell's command vocabulary as `FLYT`-prefixed lines. `printf`
/// repeats its format per argument, so no loops are needed. `abbr --show` and
/// `alias` lines are emitted raw and parsed on the Rust side.
const DUMP_SCRIPT: &str = concat!(
    "printf 'FLYT\\tbuiltin\\t%s\\n' (builtin -n)\n",
    "printf 'FLYT\\tfunction\\t%s\\n' (functions -n)\n",
    "printf 'FLYT\\trawabbr\\t%s\\n' (abbr --show 2>/dev/null)\n",
    "printf 'FLYT\\trawalias\\t%s\\n' (alias 2>/dev/null)\n",
);

/// The host shell's command vocabulary (built once per process). Backs
/// `find_alias`/`command_info`/`possible_command_words` via map lookups.
#[derive(Default)]
struct CommandTable {
    /// fish abbreviations and `alias`-style function wrappers, name → expansion.
    aliases: HashMap<String, String>,
    functions: HashSet<String>,
    builtins: HashSet<String>,
    keywords: HashSet<String>,
    /// Executables on `$PATH`, name → full path (first hit wins).
    commands: HashMap<String, String>,
    ordered: Vec<CommandWordInfo>,
}

/// Strip one level of fish single-quoting (`'it''s'` / `\'` are rare enough to
/// leave as-is; expansions are display-only).
fn unquote(s: &str) -> &str {
    s.strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(s)
}

/// Parse an `abbr --show` line: `abbr -a [flags] -- name expansion`.
/// Returns `None` for lines without the ` -- ` positional form fish emits.
fn parse_abbr_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("abbr ")?;
    let pos = rest.find(" -- ")?;
    let rest = rest[pos + 4..].trim();
    let (name, expansion) = rest.split_once(' ')?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), unquote(expansion.trim()).to_string()))
}

/// Parse an `alias` listing line: `alias name body`.
fn parse_alias_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("alias ")?;
    let (name, body) = rest.split_once(' ')?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), unquote(body.trim()).to_string()))
}

/// Executables on `$PATH`, name → full path; earlier dirs win like lookup does.
fn path_commands() -> HashMap<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let mut commands = HashMap::new();
    let Some(path_var) = std::env::var_os("PATH") else {
        return commands;
    };
    for dir in std::env::split_paths(&path_var) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if commands.contains_key(&name) {
                continue;
            }
            let is_exec = entry
                .metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            if is_exec {
                commands.insert(name, entry.path().to_string_lossy().into_owned());
            }
        }
    }
    commands
}

/// Parse `FLYT`-prefixed dump lines into a `CommandTable`, ignoring config noise.
fn parse_command_table(out: &str) -> CommandTable {
    let mut table = CommandTable::default();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("FLYT\t") else {
            continue;
        };
        let Some((kind, value)) = rest.split_once('\t') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match kind {
            "builtin" => {
                if FISH_KEYWORDS.contains(&value) {
                    table.keywords.insert(value.to_string());
                    table.ordered.push(CommandWordInfo::Keyword {
                        command: value.to_string(),
                        usage: None,
                    });
                } else {
                    table.builtins.insert(value.to_string());
                    table.ordered.push(CommandWordInfo::Builtin {
                        command: value.to_string(),
                        usage: None,
                    });
                }
            }
            "function" => {
                table.functions.insert(value.to_string());
                table.ordered.push(CommandWordInfo::Function {
                    command: value.to_string(),
                    source_file: None,
                    line: None,
                });
            }
            "rawabbr" => {
                if let Some((name, expansion)) = parse_abbr_line(value) {
                    table.ordered.push(CommandWordInfo::Alias {
                        command: name.clone(),
                        expansion: expansion.clone(),
                    });
                    table.aliases.insert(name, expansion);
                }
            }
            "rawalias" => {
                if let Some((name, expansion)) = parse_alias_line(value) {
                    table.ordered.push(CommandWordInfo::Alias {
                        command: name.clone(),
                        expansion: expansion.clone(),
                    });
                    table.aliases.insert(name, expansion);
                }
            }
            _ => {}
        }
    }
    for (name, path) in path_commands() {
        table.ordered.push(CommandWordInfo::File {
            command: name.clone(),
            path: path.clone(),
        });
        table.commands.insert(name, path);
    }
    table
}

fn build_command_table() -> CommandTable {
    // `fish -c` loads the user's config in a few ms; no disk cache needed
    // (unlike the ~1s zsh rc boot).
    match run_with_timeout("fish", &["-c", DUMP_SCRIPT], DUMP_TIMEOUT) {
        Some(out) => parse_command_table(&out),
        None => CommandTable::default(),
    }
}

/// Run a fish snippet with `extra` passed as `$argv` (injection-safe).
fn fish_eval(script: &str, extra: &[&str]) -> Option<String> {
    let mut args = vec!["-c", script];
    if !extra.is_empty() {
        args.push("--");
        args.extend_from_slice(extra);
    }
    let output = Command::new("fish").args(&args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.trim_end_matches(&['\n', '\r'][..]).to_string())
}

/// Expand `~` (own home) and `$VAR`/`${VAR}` against the environment.
/// ponytail: no `~user`, globs, or command substitution — path prefixes only.
fn expand_words(s: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let s = if !home.is_empty() {
        match s {
            "~" => home.clone(),
            _ if s.starts_with("~/") => format!("{home}{}", &s[1..]),
            _ => s.to_string(),
        }
    } else {
        s.to_string()
    };

    if !s.contains('$') {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let rest = &s[i + 1..];
        let (name, consumed) = if let Some(inner) = rest.strip_prefix('{') {
            match inner.find('}') {
                Some(end) => (&inner[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            (&rest[..end], end)
        };
        if name.is_empty() || shell_var_name(name).is_none() {
            out.push('$');
            continue;
        }
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => {
                out.push('$');
                continue;
            }
        }
        for _ in 0..consumed {
            chars.next();
        }
    }
    out
}

/// The fish host backend. Select with `shell::set_backend(&FISH_BACKEND)` at
/// standalone startup before any `shell::backend()` call.
pub struct FishBackend {
    // command vocabulary built once per process
    command_table: OnceLock<CommandTable>,
}

pub static FISH_BACKEND: FishBackend = FishBackend {
    command_table: OnceLock::new(),
};

impl FishBackend {
    fn table(&self) -> &CommandTable {
        self.command_table.get_or_init(build_command_table)
    }
}

impl ShellBackend for FishBackend {
    fn is_bash(&self) -> bool {
        false
    }

    fn cwd(&self) -> String {
        std::env::var("PWD")
            .ok()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_default()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        if let Ok(v) = std::env::var(name) {
            return Some(v);
        }
        // Fall back to a helper fish, which also sees universal variables.
        let safe = shell_var_name(name)?;
        let script = format!("set -q {safe}; and string join -- ' ' ${safe}");
        fish_eval(&script, &[]).filter(|s| !s.is_empty())
    }

    fn hostname(&self) -> String {
        std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("HOST").ok())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(hostname_from_uname)
    }

    fn format_var(&self, name: &str) -> String {
        match self.env_var(name) {
            Some(value) => format!("${name}={value}"),
            None => format!("${name}="),
        }
    }

    fn vars_with_prefix(&self, prefix: &str) -> Vec<String> {
        let bare = prefix.strip_prefix('$').unwrap_or(prefix);
        if bare.is_empty() {
            return Vec::new();
        }
        let names = fish_eval("set --names", &[]).unwrap_or_default();
        let mut vars: Vec<String> = names
            .lines()
            .filter(|l| l.starts_with(bare))
            .map(|l| format!("${l}"))
            .collect();
        if vars.is_empty() {
            vars = std::env::vars()
                .map(|(name, _)| name)
                .filter(|name| name.starts_with(bare))
                .map(|name| format!("${name}"))
                .collect();
        }
        vars.sort();
        vars.dedup();
        vars
    }

    fn expand_path(&self, path: &str) -> String {
        let expanded = self.expand_filename(path);
        if expanded.is_empty() {
            return std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        if Path::new(&expanded).is_absolute() {
            expanded
        } else {
            match std::env::current_dir() {
                Ok(cwd) => format!("{}/{}", cwd.display(), expanded),
                Err(_) => expanded,
            }
        }
    }

    fn expand_filename(&self, filename: &str) -> String {
        expand_words(filename)
    }

    fn decode_prompt(&self, raw: &str, _is_prompt: bool) -> Option<String> {
        // fish prompts are functions; the widget passes them pre-rendered
        // (ANSI), so there is nothing to decode.
        Some(raw.to_string())
    }

    fn find_alias(&self, cmd: &str) -> Option<String> {
        self.table()
            .aliases
            .get(cmd)
            .filter(|e| !e.is_empty())
            .cloned()
    }

    fn command_info(&self, cmd: &str) -> CommandWordInfo {
        let table = self.table();
        if let Some(expansion) = table.aliases.get(cmd) {
            return CommandWordInfo::Alias {
                command: cmd.to_string(),
                expansion: expansion.clone(),
            };
        }
        if table.functions.contains(cmd) {
            return CommandWordInfo::Function {
                command: cmd.to_string(),
                source_file: None,
                line: None,
            };
        }
        if table.keywords.contains(cmd) {
            return CommandWordInfo::Keyword {
                command: cmd.to_string(),
                usage: None,
            };
        }
        if table.builtins.contains(cmd) {
            return CommandWordInfo::Builtin {
                command: cmd.to_string(),
                usage: None,
            };
        }
        if let Some(path) = table.commands.get(cmd) {
            return CommandWordInfo::File {
                command: cmd.to_string(),
                path: path.clone(),
            };
        }
        CommandWordInfo::Unknown {
            command: cmd.to_string(),
        }
    }

    fn run_programmable_completions(
        &self,
        full_command: &str,
        _command_word: &str,
        word_under_cursor: &str,
        cursor_byte_pos: usize,
        _word_under_cursor_byte_end: usize,
    ) -> anyhow::Result<ProgrammableCompleteReturn> {
        let buffer_at_cursor = full_command
            .get(..cursor_byte_pos.min(full_command.len()))
            .unwrap_or(full_command);
        // `complete --do-complete` already emits flyline's `value\tdescription`
        // format; the attached `=` form parses identically on fish 3.x and 4.x.
        let out = run_with_timeout(
            "fish",
            &[
                "-c",
                "complete \"--do-complete=$argv[1]\"",
                "--",
                buffer_at_cursor,
            ],
            COMPLETE_TIMEOUT,
        )
        .ok_or_else(|| anyhow::anyhow!("fish complete -C failed or timed out"))?;

        let mut seen = HashSet::new();
        let completions: Vec<String> = out
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !l.is_empty())
            .filter(|l| seen.insert(l.to_string()))
            .map(|l| l.to_string())
            .collect();

        let flags = CompletionFlags {
            quote_type: find_quote_type(word_under_cursor),
            some_dont_end_in_equal_sign: completions.iter().any(|s| !s.ends_with('=')),
            ..Default::default()
        };
        let compspec_was_useful = !completions.is_empty();
        Ok(ProgrammableCompleteReturn::new(
            completions,
            flags,
            compspec_was_useful,
        ))
    }

    fn possible_command_words(&self) -> Vec<CommandWordInfo> {
        self.table().ordered.clone()
    }

    fn evaluate_shell_string(&self, script: &str) -> anyhow::Result<()> {
        let status = Command::new("fish").args(["-c", script]).status()?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("fish -c exited with {status}")
        }
    }

    fn reset_caches(&self) {}

    fn warm_completion_caches(&self) {
        // Build the command table off the hot path.
        let _ = self.table();
    }

    fn read_terminating_signal(&self) -> libc::c_int {
        0
    }

    fn resolve_completion_script_path(
        &self,
        command_word: &str,
        flycomp_output: Option<&str>,
    ) -> PathBuf {
        let poss_alias = self.find_alias(command_word);
        let alias_def = poss_alias
            .as_deref()
            .filter(|alias| !alias.is_empty())
            .unwrap_or(command_word);
        let cmd_word = alias_def.split_whitespace().next().unwrap_or(alias_def);
        let file_name = Path::new(cmd_word)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(cmd_word);
        // flycomp synthesizes bash-syntax completion scripts; keep them in the
        // bash-completion dir like the zsh backend does.
        let output_dir = flycomp_output.unwrap_or("~/.local/share/bash-completion/completions/");
        Path::new(&self.expand_path(output_dir)).join(file_name)
    }

    fn resolve_and_write_completion_script(
        &self,
        command_word: &str,
        script: &str,
        flycomp_output: Option<&str>,
    ) -> Result<PathBuf, std::io::Error> {
        let write_path = self.resolve_completion_script_path(command_word, flycomp_output);
        if let Some(parent) = write_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&write_path, script)?;
        Ok(write_path)
    }

    fn parse_history_from_memory(&self) -> Vec<HistoryRecord> {
        // History is file-mediated: the widget runs `history save` before
        // launching flyline and `HistoryManager` reads the fish history file.
        vec![]
    }

    fn last_command_exit_status(&self) -> i32 {
        std::env::var("FLYLINE_LAST_EXIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn multiline_command_count(&self) -> i32 {
        0
    }

    fn shell_pgrp(&self) -> libc::pid_t {
        unsafe { libc::getpgrp() }
    }
}

#[cfg(test)]
fn fish_available() -> bool {
    Command::new("which")
        .arg("fish")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquote_strips_single_quotes() {
        assert_eq!(unquote("'git checkout'"), "git checkout");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote("'unbalanced"), "'unbalanced");
    }

    #[test]
    fn parse_abbr_line_plain() {
        assert_eq!(
            parse_abbr_line("abbr -a -- gco 'git checkout'"),
            Some(("gco".to_string(), "git checkout".to_string())),
        );
    }

    #[test]
    fn parse_abbr_line_with_flags() {
        assert_eq!(
            parse_abbr_line("abbr -a --position anywhere -- L '| less'"),
            Some(("L".to_string(), "| less".to_string())),
        );
    }

    #[test]
    fn parse_abbr_line_rejects_garbage() {
        assert_eq!(parse_abbr_line("not an abbr line"), None);
        assert_eq!(parse_abbr_line("abbr -a -- lonely"), None);
    }

    #[test]
    fn parse_alias_line_plain() {
        assert_eq!(
            parse_alias_line("alias ll 'ls -l'"),
            Some(("ll".to_string(), "ls -l".to_string())),
        );
        assert_eq!(parse_alias_line("garbage"), None);
    }

    #[test]
    fn parse_command_table_classifies_kinds_and_ignores_noise() {
        let dump = "config noise line\n\
            FLYT\tbuiltin\tprintf\n\
            FLYT\tbuiltin\tif\n\
            FLYT\tfunction\tmy_fn\n\
            FLYT\trawabbr\tabbr -a -- gco 'git checkout'\n\
            FLYT\trawalias\talias ll 'ls -l'\n\
            more noise\n";
        let table = parse_command_table(dump);

        assert!(table.builtins.contains("printf"));
        assert!(table.keywords.contains("if"));
        assert!(!table.builtins.contains("if"));
        assert!(table.functions.contains("my_fn"));
        assert_eq!(
            table.aliases.get("gco").map(String::as_str),
            Some("git checkout")
        );
        assert_eq!(table.aliases.get("ll").map(String::as_str), Some("ls -l"));
    }

    #[test]
    fn parse_command_table_skips_entries_without_a_name() {
        let table = parse_command_table("FLYT\tbuiltin\t\nFLYT\tbuiltin\tvalid\n");
        assert!(!table.builtins.contains(""));
        assert!(table.builtins.contains("valid"));
    }

    #[test]
    fn expand_words_tilde_and_vars() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_words("~"), home);
        assert_eq!(expand_words("~/x"), format!("{home}/x"));
        assert_eq!(expand_words("$HOME/x"), format!("{home}/x"));
        assert_eq!(expand_words("${HOME}/x"), format!("{home}/x"));
        assert_eq!(expand_words("plain/path"), "plain/path");
        assert_eq!(expand_words("$"), "$");
        assert_eq!(expand_words("a$/b"), "a$/b");
    }

    #[test]
    fn expand_words_keeps_unset_vars_literal() {
        assert_eq!(
            expand_words("$FLYLINE_DEFINITELY_UNSET_VAR/x"),
            "$FLYLINE_DEFINITELY_UNSET_VAR/x"
        );
    }

    #[test]
    fn fish_env_var_reads_home() {
        if !fish_available() {
            return;
        }
        let backend = &FISH_BACKEND;
        assert_eq!(backend.env_var("HOME"), std::env::var("HOME").ok());
    }

    #[test]
    fn fish_run_programmable_completions_git() {
        if !fish_available() {
            return;
        }
        let backend = &FISH_BACKEND;
        let result = backend
            .run_programmable_completions("git ch", "git", "ch", 6, 6)
            .expect("git completion capture");
        assert!(!result.completions.is_empty());
        assert!(result.completions.iter().any(|s| s.starts_with("checkout")));
    }

    #[test]
    fn fish_command_table_has_builtins_and_commands() {
        if !fish_available() {
            return;
        }
        let table = build_command_table();
        assert!(table.builtins.contains("printf"));
        assert!(table.keywords.contains("if"));
        assert!(!table.commands.is_empty());
    }
}
