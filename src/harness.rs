//! Agents and skills for the Claude Code backend, discovered from the same
//! places the CLI reads them: `<project>/.claude/agents/*.md`,
//! `<project>/.claude/skills/<name>/SKILL.md`, and the user-level `~/.claude`
//! equivalents. Creating one writes a starter file the CLI will pick up on
//! the next turn; editing happens in the IDE tab. Deleting goes to the
//! recycle bin, the whole skill folder for a skill.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    pub fn badge(self) -> &'static str {
        match self {
            Scope::Project => "P",
            Scope::User => "U",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub description: String,
    /// Agents only. `None` means the frontmatter had no `tools:` key, which
    /// the CLI treats as every tool; render it as "all tools (inherited)".
    pub tools: Option<Vec<String>>,
    /// Agents only. `None` means inherit.
    pub model: Option<String>,
    pub path: PathBuf,
    pub scope: Scope,
}

pub struct Catalog {
    pub agents: Vec<Item>,
    pub skills: Vec<Item>,
    /// The two roots scanned, so "no agents" can say where nothing was found.
    pub roots: Vec<(Scope, PathBuf, bool)>,
}

pub fn user_claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Scan project then user directories. Project entries come first so a
/// project agent shadows a user one with the same name in the pickers.
pub fn scan(project: &Path) -> Catalog {
    let mut agents = Vec::new();
    let mut skills = Vec::new();
    let mut roots = Vec::new();
    let sources = [
        (project.join(".claude"), Scope::Project),
        (user_claude_dir().unwrap_or_default(), Scope::User),
    ];
    for (root, scope) in sources {
        roots.push((scope, root.clone(), root.is_dir()));
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
    Catalog { agents, skills, roots }
}

fn read_item(path: &Path, scope: Scope) -> Option<Item> {
    let text = fs::read_to_string(path).ok()?;
    let fallback = if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
        path.parent()?.file_name()?.to_string_lossy().into_owned()
    } else {
        path.file_stem()?.to_string_lossy().into_owned()
    };
    let fm = parse_frontmatter(&text);
    let name = fm.get("name").cloned().filter(|s| !s.is_empty()).unwrap_or(fallback);
    let description = fm.get("description").cloned().unwrap_or_default();
    let tools = fm.get("tools").map(|t| {
        t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>()
    });
    let model = fm.get("model").cloned().filter(|s| !s.is_empty());
    Some(Item { name, description, tools, model, path: path.to_path_buf(), scope })
}

/// Small YAML-subset frontmatter reader: `key: value`, folded and literal
/// scalars (`key: >` / `key: |` followed by indented lines), inline lists
/// (`[a, b]`), and block lists (`- a` lines) flattened to comma-joined text.
/// Unknown keys are kept. Not a YAML parser; enough for agent and skill files.
pub fn parse_frontmatter(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(rest) = text.strip_prefix("---") else { return out };
    let Some(end) = rest.find("\n---") else { return out };
    let block = &rest[..end];
    let mut key: Option<String> = None;
    let mut buf = String::new();
    let mut list: Vec<String> = Vec::new();
    let flush = |out: &mut BTreeMap<String, String>, key: &mut Option<String>, buf: &mut String, list: &mut Vec<String>| {
        if let Some(k) = key.take() {
            let v = if !list.is_empty() {
                list.join(", ")
            } else {
                buf.trim().trim_matches('"').trim_matches('\'').to_string()
            };
            out.insert(k, v);
        }
        buf.clear();
        list.clear();
    };
    for line in block.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented && !line.trim().is_empty() {
            if let Some((k, v)) = line.split_once(':') {
                if !k.trim().is_empty() && !k.contains(' ') {
                    flush(&mut out, &mut key, &mut buf, &mut list);
                    key = Some(k.trim().to_string());
                    let v = v.trim();
                    if v == ">" || v == "|" || v == ">-" || v == "|-" {
                        continue;
                    }
                    if let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                        buf.push_str(&inner.split(',').map(|s| s.trim().trim_matches('"').trim_matches('\'')).collect::<Vec<_>>().join(", "));
                    } else {
                        buf.push_str(v);
                    }
                    continue;
                }
            }
        }
        if key.is_some() {
            let t = line.trim();
            if let Some(item) = t.strip_prefix("- ") {
                list.push(item.trim().trim_matches('"').trim_matches('\'').to_string());
            } else if !t.is_empty() {
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(t);
            }
        }
    }
    flush(&mut out, &mut key, &mut buf, &mut list);
    out
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

/// What `delete` will remove: the agent file, or the whole skill folder.
pub fn delete_target(item: &Item) -> PathBuf {
    if item.path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
        item.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| item.path.clone())
    } else {
        item.path.clone()
    }
}

/// Move the item to the recycle bin (skill: its whole folder).
pub fn delete(item: &Item) -> Result<(), String> {
    crate::trash_ops::delete_to_trash(&delete_target(item))
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
        assert!(a.ends_with("code-reviewer.md"));
        assert!(s.ends_with("SKILL.md"));
        let cat = scan(&tmp);
        let agent = cat.agents.iter().find(|i| i.scope == Scope::Project).unwrap();
        assert_eq!(agent.name, "code-reviewer");
        assert_eq!(agent.tools.as_deref(), Some(&["Read".to_string(), "Grep".into(), "Glob".into(), "Bash".into()][..]));
        assert_eq!(agent.model.as_deref(), Some("sonnet"));
        let skill = cat.skills.iter().find(|i| i.scope == Scope::Project).unwrap();
        assert_eq!(skill.name, "release-notes");
        assert!(!skill.description.is_empty());
        assert!(create_agent(&tmp, Scope::Project, "code reviewer").is_err());
        assert!(delete_target(skill).ends_with("release-notes"));
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn agent_without_tools_key_reports_none() {
        let tmp = std::env::temp_dir().join(format!("tre-harness-nt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let dir = tmp.join(".claude").join("agents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bare.md"), "---\nname: bare\ndescription: >\n  folded line one\n  and line two\n---\nbody\n").unwrap();
        let cat = scan(&tmp);
        let a = cat.agents.iter().find(|i| i.name == "bare").unwrap();
        assert!(a.tools.is_none(), "absent tools must be None, not empty");
        assert!(a.model.is_none());
        assert_eq!(a.description, "folded line one and line two");
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn frontmatter_handles_lists_and_unknown_keys() {
        let fm = parse_frontmatter("---\nname: x\ntools:\n  - Read\n  - Bash\ncolor: blue\nmodel: [opus]\n---\n");
        assert_eq!(fm.get("tools").unwrap(), "Read, Bash");
        assert_eq!(fm.get("color").unwrap(), "blue");
        assert_eq!(fm.get("model").unwrap(), "opus");
    }
}
