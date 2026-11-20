//! Automatic specialist selection: when the user names no specialist,
//! pick one of the catalogue's agents for the task.
//!
//! Built to work with a small model as well as a large one. Choosing 1
//! of ~194 from one list is beyond a small model, and keyword matching
//! alone is unreliable on real wording, so the choice is two short
//! multiple-choice questions: the department (~15 options), then a role
//! within it. Every answer is checked against the real catalogue; an
//! answer that is not an option routes nowhere (the runner's fallback).
//! Keyword scoring ([`lexical`]) is kept only for ensemble members, which
//! must all get the same specialist without a model call.
//!
//! Pure functions only; the model call itself is the runner's.

use se_catalogue::AgentDefinition;

/// The system prompt for both routing questions.
pub const ROUTER_SYSTEM: &str = "You assign software engineering tasks to the right specialist. \
Reply with exactly one option id from the list and nothing else.";

const MISSION_CHARS: usize = 140;

/// Departments in first-seen order, with how many roles each holds.
pub fn departments(agents: &[AgentDefinition]) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for a in agents.iter().filter(|a| a.frontmatter.enabled) {
        match out.iter_mut().find(|(d, _)| *d == a.frontmatter.department) {
            Some((_, n)) => *n += 1,
            None => out.push((a.frontmatter.department.clone(), 1)),
        }
    }
    out
}

/// Every enabled role id, one line per department: what a role that deploys
/// agents needs to name a real one (ids only, about 1.5k tokens for 194).
pub fn roster(agents: &[AgentDefinition]) -> String {
    departments(agents)
        .iter()
        .map(|(d, _)| {
            let ids: Vec<&str> = agents
                .iter()
                .filter(|a| a.frontmatter.enabled && &a.frontmatter.department == d)
                .map(|a| a.frontmatter.id.as_str())
                .collect();
            format!("  {d}: {}\n", ids.join(", "))
        })
        .collect()
}

/// With `current` (the conversation's specialist and its department), KEEP is also an answer.
pub fn department_prompt(
    task: &str,
    departments: &[(String, usize)],
    current: Option<(&str, &str)>,
) -> String {
    let mut p = format!(
        "Task:\n{}\n\nWhich department should own this task? Options:\n",
        clip(task, 2_000)
    );
    for (d, n) in departments {
        p.push_str(&format!("- {d} ({n} roles)\n"));
    }
    match current {
        Some((id, d)) => p.push_str(&format!(
            "\nThe conversation is with `{id}` ({d}). If this message is a follow-up or still \
             {d} work, reply KEEP. Otherwise reply with the department id only."
        )),
        None => p.push_str("\nReply with the department id only."),
    }
    p
}

pub fn role_prompt(task: &str, roles: &[&AgentDefinition]) -> String {
    let mut p = format!(
        "Task:\n{}\n\nWhich specialist should do this task? Options:\n",
        clip(task, 2_000)
    );
    for a in roles {
        p.push_str(&format!(
            "- {}: {}\n",
            a.frontmatter.id,
            clip(a.frontmatter.mission.trim(), MISSION_CHARS)
        ));
    }
    p.push_str("\nReply with the specialist id only.");
    p
}

/// The option the reply names. Exact matches win; otherwise the longest
/// option appearing in the reply as a whole word, so a chatty model
/// ("I'd pick backend-lead.") still routes, but `backend` alone never
/// matches `backend-lead`.
pub fn pick<'a>(reply: &str, options: &[&'a str]) -> Option<&'a str> {
    let reply = reply
        .trim()
        .trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == '.');
    let lower = reply.to_ascii_lowercase();
    if let Some(exact) = options.iter().find(|o| o.eq_ignore_ascii_case(&lower)) {
        return Some(exact);
    }
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    options
        .iter()
        .filter(|o| words.iter().any(|w| w.eq_ignore_ascii_case(o)))
        .max_by_key(|o| o.len())
        .copied()
}

/// Keyword fallback: overlap between the task's words and each agent's
/// id, activation phrases and mission. Needs at least two distinct
/// matching words, so a single common word cannot pick a specialist.
pub fn lexical<'a>(task: &str, agents: &'a [AgentDefinition]) -> Option<&'a AgentDefinition> {
    let task_words = words(task);
    agents
        .iter()
        .filter(|a| a.frontmatter.enabled)
        .map(|a| {
            let mut text = a.frontmatter.id.replace('-', " ");
            for act in &a.frontmatter.activation {
                text.push(' ');
                text.push_str(act);
            }
            text.push(' ');
            text.push_str(&a.frontmatter.mission);
            let theirs = words(&text);
            (task_words.iter().filter(|w| theirs.contains(*w)).count(), a)
        })
        .filter(|(score, _)| *score >= 2)
        .max_by_key(|(score, _)| *score)
        .map(|(_, a)| a)
}

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "but", "not", "you",
    "our", "can", "all", "any", "make", "need", "want", "have", "use", "using", "when", "what",
    "how", "why", "it", "is", "to", "of", "in", "on", "a", "an", "be", "or", "as", "so", "do",
    "my", "we", "i",
];

fn words(text: &str) -> std::collections::HashSet<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2 && !STOP_WORDS.contains(w))
        .map(str::to_string)
        .collect()
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_accepts_chatty_replies_but_never_partial_ids() {
        let options = ["backend-lead", "backend-service-engineer", "frontend-lead"];
        assert_eq!(pick("backend-lead", &options), Some("backend-lead"));
        assert_eq!(pick("`Frontend-Lead`.", &options), Some("frontend-lead"));
        assert_eq!(
            pick("I would assign backend-service-engineer here.", &options),
            Some("backend-service-engineer")
        );
        assert_eq!(pick("backend", &options), None, "a prefix is not an id");
        assert_eq!(pick("no idea", &options), None);
    }

    /// The real catalogue, when this machine has it: departments are
    /// few enough for a small model, and the department prompt for the
    /// whole catalogue stays small.
    #[test]
    fn the_real_catalogue_fits_a_two_step_choice() {
        let root_buf = std::path::PathBuf::from(std::env::var("SE_CATALOGUE_ROOT").unwrap_or_else(|_| {
            format!("{}/SE/se/agents", std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default())
        }));
        let root = root_buf.as_path();
        if !root.exists() {
            eprintln!("skipped: no real catalogue on this machine");
            return;
        }
        let report = se_catalogue::load(root).unwrap();
        let depts = departments(&report.agents);
        assert!(depts.len() <= 20, "{} departments", depts.len());
        assert!(department_prompt("x", &depts, None).len() < 1_500);
        for (d, _) in &depts {
            let roles: Vec<&AgentDefinition> = report
                .agents
                .iter()
                .filter(|a| &a.frontmatter.department == d && a.frontmatter.enabled)
                .collect();
            assert!(role_prompt("x", &roles).len() < 8_000, "{d}");
        }
        let hit = lexical(
            "design the database schema and write the migration for the orders table",
            &report.agents,
        );
        assert!(hit.is_some(), "a clearly technical task finds someone");
    }
}
