//! Skills over MCP (`io.modelcontextprotocol/skills`).
//!
//! Every server built from this template ships at least one **skill**: a
//! natural-language playbook that tells a connected agent *when* and *how* to
//! use the server's tools. Tools are the hands; the skill is the manual.
//!
//! The extension rides on the existing MCP **resources** primitive rather than
//! a new protocol method — see [`crate::server`] for the `resources/list`,
//! `resources/templates/list` and `resources/read` handlers that expose what
//! this module holds. Discovery is progressive:
//!
//! 1. A client reads [`INDEX_URI`] (`skill://index.json`) once at session
//!    start and gets a lightweight catalog: skill names plus the trigger
//!    descriptions it matches user requests against.
//! 2. Only when a request matches does it read
//!    `skill://<name>/SKILL.md` — the full playbook.
//! 3. Relative Markdown links inside a `SKILL.md` resolve against the skill
//!    root, so `[Tools](references/TOOLS.md)` is
//!    `skill://<name>/references/TOOLS.md`, pulled in only if the model needs
//!    that depth.
//!
//! Skill files are embedded with [`include_str!`], so the component is
//! self-contained: no filesystem, no network, and the playbook version can
//! never drift from the tool implementation it documents.
//!
//! ## Adding a skill
//!
//! Put it under `skills/<dir>/SKILL.md` (plus any supporting files) and add an
//! entry to [`SKILLS`]. The catalog's description comes from the `SKILL.md`
//! YAML frontmatter, so there is exactly one place to edit it.
//!
//! Directory names are fixed at build time (`include_str!`), but the skill's
//! *name* — the authority in its URIs — is [`env!("CARGO_PKG_NAME")`] for this
//! server's own skill, so renaming the package renames the skill with it.

use rmcp::model::{Resource, ResourceTemplate};

/// Identifier of the MCP skills extension these resources implement.
pub const EXTENSION_ID: &str = "io.modelcontextprotocol/skills";

/// URI of the skill catalog clients read at session start.
pub const INDEX_URI: &str = "skill://index.json";

/// Scheme prefix for every skill resource URI.
const SCHEME: &str = "skill://";

/// The entrypoint filename of a skill bundle, per the SKILL.md convention.
const ENTRY: &str = "SKILL.md";

/// A supporting file inside a skill bundle, served at
/// `skill://<skill>/<path>`.
pub struct SkillFile {
    /// Path relative to the skill root, e.g. `references/TOOLS.md`. Matched
    /// verbatim against the requested URI — there is no filesystem lookup and
    /// therefore no traversal to defend against.
    pub path: &'static str,
    /// File contents, embedded at compile time.
    pub text: &'static str,
    /// MIME type reported to the client.
    pub mime_type: &'static str,
}

/// One skill: a `SKILL.md` playbook and its supporting files.
pub struct Skill {
    /// Skill name; the authority segment of its URIs. The server's own skill
    /// takes the package name, so the skill's identity always equals the
    /// server's and neither can drift from the other on a rename.
    pub name: &'static str,
    /// The playbook itself, served at `skill://<name>/SKILL.md`. Held as its
    /// own field rather than as one of `files` so a skill structurally cannot
    /// exist without its entrypoint.
    pub skill_md: &'static str,
    /// Supporting files, if any.
    pub files: &'static [SkillFile],
}

/// Skills this server serves.
///
/// `skills/server/` is this server's own manual. Nothing here needs renaming
/// when the package is renamed: the skill takes the package name, so the
/// skill's identity always equals the server's.
pub static SKILLS: &[Skill] = &[Skill {
    name: env!("CARGO_PKG_NAME"),
    skill_md: include_str!("../skills/server/SKILL.md"),
    files: &[
        SkillFile {
            path: "references/TOOLS.md",
            text: include_str!("../skills/server/references/TOOLS.md"),
            mime_type: "text/markdown",
        },
        // The Illustrator -> After Effects handoff. Its own file because it is
        // long, and because it is only needed on the one task it covers —
        // exactly what progressive disclosure is for. The counterpart lives in
        // the illustrator-mcp server's skill.
        SkillFile {
            path: "references/HANDOFF.md",
            text: include_str!("../skills/server/references/HANDOFF.md"),
            mime_type: "text/markdown",
        },
    ],
}];

impl Skill {
    /// URI of this skill's `SKILL.md`.
    pub fn entry_uri(&self) -> String {
        format!("{SCHEME}{}/{ENTRY}", self.name)
    }

    /// URI of a supporting file.
    pub fn file_uri(&self, file: &SkillFile) -> String {
        format!("{SCHEME}{}/{}", self.name, file.path)
    }

    /// The `description` from the frontmatter — the trigger text a client
    /// matches user requests against.
    pub fn description(&self) -> &'static str {
        frontmatter_field(self.skill_md, "description").unwrap_or("")
    }
}

/// The skill catalog served at [`INDEX_URI`].
///
/// Built from [`SKILLS`] on every read rather than stored, so the catalog
/// cannot fall out of step with the embedded files.
pub fn index_json() -> String {
    let skills: Vec<_> = SKILLS
        .iter()
        .map(|skill| {
            serde_json::json!({
                "name": skill.name,
                "description": skill.description(),
                "uri": skill.entry_uri(),
                "files": skill
                    .files
                    .iter()
                    .map(|file| serde_json::json!({
                        "path": file.path,
                        "uri": skill.file_uri(file),
                        "mimeType": file.mime_type,
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    let index = serde_json::json!({
        "schemaVersion": "1",
        "extension": EXTENSION_ID,
        "server": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "skills": skills,
    });
    // The value is built from `&'static str` and owned Vecs — serialization
    // cannot fail, but never panic in a component: a trap kills the instance.
    serde_json::to_string_pretty(&index).unwrap_or_else(|_| String::from(r#"{"skills":[]}"#))
}

/// Every skill resource, for `resources/list`.
pub fn resources() -> Vec<Resource> {
    let mut resources = vec![Resource::new(INDEX_URI, "skill-index")
        .with_title("Skill catalog")
        .with_description(
            "Catalog of the skills this server publishes: name, trigger description, \
             and the URI of each SKILL.md. Read this first, then read only the skills \
             whose descriptions match the task.",
        )
        .with_mime_type("application/json")];

    for skill in SKILLS {
        resources.push(
            Resource::new(skill.entry_uri(), skill.name)
                .with_title(format!("{} skill", skill.name))
                .with_description(skill.description())
                .with_mime_type("text/markdown")
                .with_size(skill.skill_md.len() as u64),
        );
        for file in skill.files {
            resources.push(
                Resource::new(
                    skill.file_uri(file),
                    format!("{}/{}", skill.name, file.path),
                )
                .with_title(format!("{} — {}", skill.name, file.path))
                .with_description(format!(
                    "Supporting file for the {} skill, referenced from its SKILL.md.",
                    skill.name
                ))
                .with_mime_type(file.mime_type)
                .with_size(file.text.len() as u64),
            );
        }
    }
    resources
}

/// RFC 6570 templates for `resources/templates/list`, so a client can build
/// skill URIs without enumerating them.
pub fn resource_templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new("skill://{skill}/SKILL.md", "skill-playbook")
            .with_title("Skill playbook")
            .with_description(
                "The SKILL.md of a named skill. Skill names come from skill://index.json.",
            )
            .with_mime_type("text/markdown"),
        ResourceTemplate::new("skill://{skill}/{+path}", "skill-file")
            .with_title("Skill supporting file")
            .with_description(
                "A file bundled with a skill. Relative Markdown links inside a SKILL.md \
                 resolve against the skill root and produce these URIs.",
            ),
    ]
}

/// Resolves a `skill://` URI to `(mime_type, contents)`, or `None` if this
/// server serves nothing at that URI.
pub fn read(uri: &str) -> Option<(&'static str, String)> {
    if uri == INDEX_URI {
        return Some(("application/json", index_json()));
    }

    let (name, path) = uri.strip_prefix(SCHEME)?.split_once('/')?;
    let skill = SKILLS.iter().find(|skill| skill.name == name)?;
    if path == ENTRY {
        return Some(("text/markdown", skill.skill_md.to_owned()));
    }
    let file = skill.files.iter().find(|file| file.path == path)?;
    Some((file.mime_type, file.text.to_owned()))
}

/// Reads a single-line field out of a Markdown file's YAML frontmatter.
///
/// Deliberately minimal — a full YAML parser would be a large dependency for
/// two fields. SKILL.md frontmatter must therefore keep `name` and
/// `description` on one line each (optionally quoted), which is what the
/// SKILL.md convention already prescribes.
fn frontmatter_field(markdown: &'static str, field: &str) -> Option<&'static str> {
    let body = markdown.strip_prefix("---\n")?;
    let end = body.find("\n---")?;
    for line in body[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != field {
            continue;
        }
        let value = value.trim();
        let unquoted = value
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
            })
            .unwrap_or(value);
        return Some(unquoted);
    }
    None
}
