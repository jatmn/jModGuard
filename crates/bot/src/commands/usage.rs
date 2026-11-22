use std::{collections::BTreeMap, fmt};

use super::{CachedGuildConfig, behaviors, mod_reason_dm};
use crate::types::{Behavior, CommandDef};

const REPLY_TEMPLATE: &str =
    "{flavor} {problem}\nUsage: {usage}\nExample: {example}\nSee {help} for details.";

pub(super) const BAN_MINUTES_FLAG: &str = "--minutes";
pub(super) const LIST: &str = "list";
pub(super) const GET: &str = "get";
pub(super) const SET: &str = "set";
pub(super) const RESET: &str = "reset";
pub(super) const TOGGLE: &str = "toggle";
pub(super) const DRYRUN: &str = "dryrun";
pub(super) const UNSET: &str = "unset";

#[derive(Debug)]
pub(super) struct ArgumentError {
    name: &'static str,
    problem: ArgumentProblem,
    action: Option<&'static str>,
}

#[derive(Debug)]
enum ArgumentProblem {
    Missing,
    Invalid(String),
}

impl ArgumentError {
    pub(super) fn missing(name: &'static str) -> Self {
        Self {
            name,
            problem: ArgumentProblem::Missing,
            action: None,
        }
    }

    pub(super) fn invalid(name: &'static str, expected: impl Into<String>) -> Self {
        Self {
            name,
            problem: ArgumentProblem::Invalid(expected.into()),
            action: None,
        }
    }

    pub(super) fn unresolved_user() -> Self {
        Self::invalid(
            "user",
            "I couldn't find that user. Try a mention or numeric user ID.",
        )
    }

    pub(super) fn for_action(mut self, action: &'static str) -> Self {
        self.action = Some(action);
        self
    }
}

impl fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.problem {
            ArgumentProblem::Missing => write!(f, "Missing argument `{}`.", self.name),
            ArgumentProblem::Invalid(expected) => {
                write!(f, "Invalid argument `{}`. {expected}", self.name)
            }
        }
    }
}

impl std::error::Error for ArgumentError {}

pub(super) fn reply(
    error: &ArgumentError,
    cfg: &CachedGuildConfig,
    command: &CommandDef,
    invocation: &str,
) -> String {
    let prefix = &cfg.settings.prefix;
    let (usage, example) = syntax(&command.behavior, error.action);
    let flavor = match (&command.behavior, &error.problem, error.name) {
        (Behavior::Ban { .. }, ArgumentProblem::Missing, "user") => {
            "Who's getting the hammer? I need a user."
        }
        (Behavior::Ban { .. }, _, _) => "Hold the ban hammer.",
        _ => "Let's give that another try.",
    };
    let help = cfg
        .find_command("help")
        .filter(|cmd| cmd.enabled && matches!(cmd.behavior, Behavior::Help))
        .or_else(|| {
            cfg.commands
                .values()
                .filter(|cmd| cmd.enabled && matches!(cmd.behavior, Behavior::Help))
                .min_by_key(|cmd| &cmd.name)
        })
        .map_or("help", |cmd| cmd.name.as_str());
    crate::templates::render(
        REPLY_TEMPLATE,
        &BTreeMap::from([
            ("flavor".into(), flavor.into()),
            ("problem".into(), error.to_string()),
            (
                "usage".into(),
                code_span(&format!("{prefix}{invocation} {usage}")),
            ),
            (
                "example".into(),
                code_span(&format!("{prefix}{invocation} {example}")),
            ),
            (
                "help".into(),
                code_span(&format!("{prefix}{help} {}", command.name)),
            ),
        ]),
    )
}

fn code_span(command: &str) -> String {
    if !command.contains('`') {
        return format!("`{command}`");
    }
    // A guild prefix can contain backticks. Keep the actual invocable text
    // inside a delimiter longer than any run of backticks in that text.
    let longest = command.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    format!("{fence} {command} {fence}")
}

struct PrefixArg {
    syntax: String,
    required: bool,
    example: &'static str,
}

impl PrefixArg {
    fn new(syntax: impl Into<String>, required: bool, example: &'static str) -> Self {
        Self {
            syntax: syntax.into(),
            required,
            example,
        }
    }
}

// Start with the ordered option definitions used for slash registration and
// argument adaptation. Describe prefix-only positions, flags and defaults here.
fn syntax(behavior: &Behavior, action: Option<&str>) -> (String, String) {
    let mut args: Vec<_> = behaviors::slash_spec(behavior)
        .into_iter()
        .map(|spec| {
            let example = match spec.name {
                "user" => "42",
                "reason" => "repeated spam",
                "minutes" => "60",
                "count" => "10",
                "seconds" => "15",
                "prefix" => "?",
                "pattern" => "spam",
                "sample" => "example spam",
                "text" => "Please read the rules",
                "event" => "mod_action",
                "channel" => "<#42>",
                "command" => "ban",
                "name" => "ModGuard",
                "value" => "on",
                _ => "",
            };
            PrefixArg::new(spec.name, spec.required, example)
        })
        .collect();
    match behavior {
        Behavior::Ban { .. } | Behavior::Kick { .. } => {
            // Prefix parsing strips --dm immediately after the target.
            args.pop();
            args.insert(1, PrefixArg::new(mod_reason_dm::PREFIX_DM_FLAG, false, ""));
            if matches!(behavior, Behavior::Ban { .. }) {
                args[2] = PrefixArg::new(format!("{BAN_MINUTES_FLAG} N"), false, "--minutes 60");
            }
        }
        Behavior::Banlist => args.push(PrefixArg::new("limit", false, "10")),
        Behavior::Warn { .. } | Behavior::Slowmode { .. } | Behavior::Testregex => {
            // The prefix forms have defaults, even though slash requires these options.
            args.last_mut().unwrap().required = false;
        }
        Behavior::Say => args.insert(0, PrefixArg::new("#channel", false, "")),
        Behavior::Plan { steps } if super::plans::plan_requires_target(steps) => {
            args[0].required = true;
        }
        Behavior::Template { .. } | Behavior::Rule { .. } | Behavior::Logchannel { .. } => {
            let (choices, example_action) = match behavior {
                Behavior::Template { .. } => ([LIST, GET, SET, RESET].join("|"), GET),
                Behavior::Rule { .. } => ([LIST, TOGGLE, DRYRUN].join("|"), DRYRUN),
                _ => ([LIST, SET, UNSET].join("|"), SET),
            };
            if matches!(behavior, Behavior::Rule { .. }) {
                args.swap(0, 1);
                args[1] = PrefixArg::new("rule", false, "mention_spam");
                args[2].syntax = "on|off".into();
            }
            if matches!(behavior, Behavior::Logchannel { .. }) {
                args[2].syntax = "#channel".into();
            }
            match action {
                Some(action) => {
                    let n = match (behavior, action) {
                        (Behavior::Template { .. } | Behavior::Logchannel { .. }, SET) => 3,
                        (Behavior::Rule { .. }, DRYRUN) => 3,
                        _ => 2,
                    };
                    args.truncate(n);
                    for arg in &mut args[1..] {
                        arg.required = true;
                    }
                    if matches!(behavior, Behavior::Rule { .. }) && action == DRYRUN {
                        args[2].required = false;
                    }
                    let (tail, example) = render(&args[1..]);
                    return (format!("{action} {tail}"), format!("{action} {example}"));
                }
                None => {
                    args[0] = PrefixArg::new(choices, false, example_action);
                    if matches!(behavior, Behavior::Template { .. }) {
                        args[2].example = "";
                    }
                }
            }
        }
        _ => {}
    }
    render(&args)
}

fn render(args: &[PrefixArg]) -> (String, String) {
    let usage = args
        .iter()
        .map(|arg| {
            if arg.required {
                format!("<{}>", arg.syntax)
            } else {
                format!("[{}]", arg.syntax)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let example = args
        .iter()
        .map(|arg| arg.example)
        .filter(|example| !example.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (usage, example)
}
