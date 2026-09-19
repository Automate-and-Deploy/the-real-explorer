//! Agents and skills for the Claude Code backend, discovered from the same
//! places the CLI reads them: `<project>/.claude/agents/*.md`,
//! `<project>/.claude/skills/<name>/SKILL.md`, and the user-level `~/.claude`
//! equivalents. Creating one writes a starter file the CLI will pick up on
//! the next turn; editing happens in the IDE tab.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    Project,
    User,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: Scope,
}

pub struct Catalog {
    pub agents: Vec<Item>,
    pub skills: Vec<Item>,
}

fn user_claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Scan project then user directories. Project entries come first so a
/// project agent shadows a user one with the same name in the pickers.
#[allow(dead_code)]
pub fn scan(project: &Path) -> Catalog {
    let mut agents = Vec::new();
    let mut skills = Vec::new();
    let roots = [
        (project.join(".claude"), Scope::Project),
        (user_claude_dir().unwrap_or_default(), Scope::User),
    ];
    for (root, scope) in roots {
        if let Ok(rd) = fs::read_dir(root.join("agents")) {
            let mut v: Vec<Item> = rd
                .flatten()
                .map(|de| de.path())
                .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
                .filter_map(|p| read_item(&p, scope))
                .collect();
            v.sort_by(|a, b| a.name.cmp(&b.name));
            agents.extend(v);
        }
        if let Ok(rd) = fs::read_dir(root.join("skills")) {
            let mut v: Vec<Item> = rd
                .flatten()
                .map(|de| de.path().join("SKILL.md"))
                .filter(|p| p.is_file())
                .filter_map(|p| read_item(&p, scope))
                .collect();
            v.sort_by(|a, b| a.name.cmp(&b.name));
            skills.extend(v);
        }
    }
    Catalog { agents, skills }
}

/// Name from frontmatter `name:`, else the file/dir name. Description from
/// frontmatter `description:` (first line only).
fn read_item(path: &Path, scope: Scope) -> Option<Item> {
    let text = fs::read_to_string(path).ok()?;
    let fallback = if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
        path.parent()?.file_name()?.to_string_lossy().into_owned()
    } else {
        path.file_stem()?.to_string_lossy().into_owned()
    };
    let (mut name, mut description) = (fallback, String::new());
    if let Some(fm) = frontmatter(&text) {
        for line in fm.lines() {
            if let Some(v) = line.strip_prefix("name:") {
                name = v.trim().trim_matches('"').to_string();
            } else if let Some(v) = line.strip_prefix("description:") {
                description = v.trim().trim_matches('"').to_string();
            }
        }
    }
    Some(Item { name, description, path: path.to_path_buf(), scope })
}

fn frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn slug(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    s.trim_matches('-').to_string()
}

/// Write a starter agent file. Errors if the name is empty or the file exists.
#[allow(dead_code)]
pub fn create_agent(project: &Path, scope: Scope, name: &str) -> Result<PathBuf, String> {
    let slug = slug(name);
    if slug.is_empty() {
        return Err("agent needs a name".into());
    }
    let dir = match scope {
        Scope::Project => project.join(".claude").join("agents"),
        Scope::User => user_claude_dir().ok_or("no home dir")?.join("agents"),
    };
    let path = dir.join(format!("{slug}.md"));
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body = format!(
        "---\nname: {slug}\ndescription: What this agent is for and when to use it.\ntools: Read, Grep, Glob, Bash\nmodel: sonnet\n---\n\nYou are the {slug} agent.\n\n## Job\n\nDescribe the task this agent owns.\n\n## Rules\n\n- Report findings with file paths and line numbers.\n- Do not edit files unless asked.\n"
    );
    fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Write a starter skill (`<name>/SKILL.md`). Errors if the name is empty or
/// the directory exists.
#[allow(dead_code)]
pub fn create_skill(project: &Path, scope: Scope, name: &str) -> Result<PathBuf, String> {
    let slug = slug(name);
    if slug.is_empty() {
        return Err("skill needs a name".into());
    }
    let dir = match scope {
        Scope::Project => project.join(".claude").join("skills").join(&slug),
        Scope::User => user_claude_dir().ok_or("no home dir")?.join("skills").join(&slug),
    };
    let path = dir.join("SKILL.md");
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body = format!(
        "---\nname: {slug}\ndescription: One line on what this skill does and when to invoke it.\n---\n\n# {slug}\n\nInvoke with `/{slug}` in the assistant panel.\n\n## Steps\n\n1. First step.\n2. Second step.\n\n## Output\n\nWhat the result should look like.\n"
    );
    fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_scans_project_agent_and_skill() {
        let tmp = std::env::temp_dir().join(format!("tre-harness-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let a = create_agent(&tmp, Scope::Project, "Code Reviewer").unwrap();
        let s = create_skill(&tmp, Scope::Project, "release notes").unwrap();
        assert!(a.ends_with(".claude/agents/code-reviewer.md") || a.ends_with(".claude\\agents\\code-reviewer.md"));
        assert!(s.ends_with("SKILL.md"));
        let cat = scan(&tmp);
        let agent = cat.agents.iter().find(|i| i.scope == Scope::Project).unwrap();
        assert_eq!(agent.name, "code-reviewer");
        let skill = cat.skills.iter().find(|i| i.scope == Scope::Project).unwrap();
        assert_eq!(skill.name, "release-notes");
        assert!(!skill.description.is_empty());
        assert!(create_agent(&tmp, Scope::Project, "code reviewer").is_err());
        fs::remove_dir_all(&tmp).unwrap();
    }
}
