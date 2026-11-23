//! CurseForge's API (api.curseforge.com/v1). It needs a key from CurseForge's developer console,
//! so this source stays off until one is given (docs/mod-browser.md#curseforge):
//! `HORIZON_CURSEFORGE_API_KEY` in the environment, else the same variable when the launcher was
//! built (the release workflow passes the repository secret). Files carry SHA-1 and MD5; the
//! download is checked against the SHA-1.
use super::{
    details::{self, Dependency, DependencyKind},
    modrinth::encode,
    FileRef, Kind, Project, Query, Source, SourceId, Version, GAME,
};
use serde_json::Value;

const API: &str = "https://api.curseforge.com/v1";
/// Minecraft's game id on CurseForge.
const MINECRAFT: u64 = 432;
/// `HashAlgo` in CurseForge's schema.
const SHA1: u64 = 1;
/// `FileRelationType::OptionalDependency`.
const OPTIONAL: u64 = 2;
/// `FileRelationType::RequiredDependency`.
const REQUIRED: u64 = 3;
/// `FileRelationType::Incompatible`.
const INCOMPATIBLE: u64 = 5;

pub struct CurseForge {
    key: String,
}

/// CurseForge's class id for a kind.
fn class(kind: Kind) -> u64 {
    match kind {
        Kind::ResourcePack => 12,
        Kind::Modpack => 4471,
        Kind::Mod => 6,
        Kind::Shader => 6552,
        Kind::DataPack => 6945,
    }
}

/// The kind of a CurseForge class; its shader packs (the shaders/ format) are not offered.
fn kind_of(class_id: u64) -> Option<Kind> {
    Kind::ALL
        .into_iter()
        .filter(|k| *k != Kind::Shader)
        .find(|k| class(*k) == class_id)
}

/// The key: the environment's, else the one built in. Empty counts as none.
pub(super) fn key(runtime: Option<String>, built: Option<&str>) -> Option<String> {
    runtime
        .or_else(|| built.map(str::to_owned))
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty())
}

impl CurseForge {
    /// The source, when a key exists; else None and CurseForge isn't offered.
    pub fn from_env() -> Option<Self> {
        key(
            std::env::var("HORIZON_CURSEFORGE_API_KEY").ok(),
            option_env!("HORIZON_CURSEFORGE_API_KEY"),
        )
        .map(|key| Self { key })
    }

    fn get(&self, url: &str) -> Result<Value, String> {
        let response = super::download::agent(url)
            .get(url)
            .set("x-api-key", &self.key)
            .set("Accept", "application/json")
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(403, _) => "CurseForge refused the launcher's API key.".into(),
                ureq::Error::Status(code, _) => format!("CurseForge answered {code}."),
                ureq::Error::Transport(t) => format!("No connection to CurseForge: {t}"),
            })?;
        response
            .into_json()
            .map_err(|e| format!("CurseForge's answer wasn't readable: {e}"))
    }
}

pub(super) fn search_url(query: &Query) -> String {
    let mut url = format!(
        "{API}/mods/search?gameId={MINECRAFT}&classId={}&sortField={}&sortOrder=desc&index={}&pageSize={}",
        class(query.kind),
        query.filters.sort(&query.text).curseforge(),
        query.offset,
        query.limit.clamp(1, 50),
    );
    if let Some(v) = &query.filters.game_version {
        url.push_str(&format!("&gameVersion={}", encode(v)));
    } else if !query.everything && matches!(query.kind, Kind::ResourcePack | Kind::Mod) {
        url.push_str(&format!("&gameVersion={GAME}"));
    }
    if !query.text.is_empty() {
        url.push_str("&searchFilter=");
        url.push_str(&encode(&query.text));
    }
    url
}

/// CurseForge mixes Minecraft versions and loader names (`Fabric`, `NeoForge`) in one
/// list: digits first is a version, the rest are loaders, in lowercase as Modrinth names them.
fn split_versions(list: &Value) -> (Vec<String>, Vec<String>) {
    let mut versions = Vec::new();
    let mut loaders = Vec::new();
    for v in list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if v.starts_with(|c: char| c.is_ascii_digit()) {
            versions.push(v.to_owned());
        } else {
            loaders.push(v.to_ascii_lowercase());
        }
    }
    versions.sort();
    versions.dedup();
    loaders.sort();
    loaders.dedup();
    (versions, loaders)
}

pub(super) fn parse_mod(m: &Value) -> Option<Project> {
    let kind = kind_of(m["classId"].as_u64()?)?;
    let latest = Value::Array(
        m["latestFilesIndexes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["gameVersion"].as_str())
            .map(|v| Value::String(v.to_owned()))
            .collect(),
    );
    let (game_versions, loaders) = split_versions(&latest);
    Some(Project {
        source: SourceId::CurseForge,
        id: m["id"].as_u64()?.to_string(),
        slug: m["slug"].as_str().unwrap_or_default().to_owned(),
        title: m["name"].as_str().unwrap_or_default().to_owned(),
        summary: m["summary"].as_str().unwrap_or_default().to_owned(),
        author: m["authors"][0]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        downloads: m["downloadCount"].as_f64().unwrap_or(0.) as u64,
        page: m["links"]["websiteUrl"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        icon: m["logo"]["thumbnailUrl"].as_str().map(str::to_owned),
        // CurseForge's API doesn't say; the project page does.
        license: String::new(),
        kind,
        game_versions,
        loaders,
        blocked: None,
    })
}

pub(super) fn parse_dependencies(list: &Value) -> Vec<Dependency> {
    let mut out = Vec::new();
    for d in list.as_array().into_iter().flatten() {
        let kind = match d["relationType"].as_u64() {
            Some(REQUIRED) => DependencyKind::Required,
            Some(OPTIONAL) => DependencyKind::Optional,
            Some(INCOMPATIBLE) => DependencyKind::Incompatible,
            _ => continue,
        };
        let Some(mod_id) = d["modId"].as_u64() else {
            continue;
        };
        let id = mod_id.to_string();
        if !out
            .iter()
            .any(|existing: &Dependency| existing.id == id && existing.kind == kind)
        {
            out.push(Dependency {
                id,
                name: None,
                kind,
                version_req: None,
            });
        }
    }
    out
}

pub(super) fn parse_file(f: &Value) -> Option<Version> {
    let (game_versions, loaders) = split_versions(&f["gameVersions"]);
    let name = f["fileName"].as_str()?.to_owned();
    let sha1 = f["hashes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|h| h["algo"].as_u64() == Some(SHA1))
        .and_then(|h| h["value"].as_str())
        .map(str::to_ascii_lowercase);
    let url = f["downloadUrl"].as_str().filter(|u| !u.is_empty());
    let files = url
        .map(|url| {
            vec![FileRef {
                url: url.to_owned(),
                name: name.clone(),
                size: f["fileLength"].as_u64().unwrap_or(0),
                sha512: None,
                sha256: None,
                sha1,
                primary: true,
            }]
        })
        .unwrap_or_default();
    let dependencies = parse_dependencies(&f["dependencies"]);
    let required = dependencies
        .iter()
        .filter(|d| d.kind == DependencyKind::Required)
        .map(|d| d.id.clone())
        .collect();
    Some(Version {
        id: f["id"].as_u64()?.to_string(),
        number: f["displayName"].as_str().unwrap_or(&name).to_owned(),
        published: f["fileDate"].as_str().unwrap_or_default().to_owned(),
        game_versions,
        loaders,
        files,
        optional: Vec::new(),
        required,
        dependencies,
        blocked: url
            .is_none()
            .then(|| "Its author allows downloads from CurseForge's own site only.".to_owned()),
    })
}

impl Source for CurseForge {
    fn id(&self) -> SourceId {
        SourceId::CurseForge
    }

    fn search(&self, query: &Query) -> Result<Vec<Project>, String> {
        if query.kind == Kind::Shader {
            return Err(super::SHADERS_ON_HORIZON.into());
        }
        let found = self.get(&search_url(query))?;
        Ok(found["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_mod)
            .collect())
    }

    fn project(&self, id: &str) -> Result<Project, String> {
        let m = self.get(&format!("{API}/mods/{}", encode(id)))?;
        parse_mod(&m["data"])
            .ok_or_else(|| format!("CurseForge: {id} is not a pack Horizon loads."))
    }

    fn versions(&self, project: &Project) -> Result<Vec<Version>, String> {
        let found = self.get(&format!(
            "{API}/mods/{}/files?pageSize=50",
            encode(&project.id)
        ))?;
        let mut all: Vec<Version> = found["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["isAvailable"].as_bool() != Some(false))
            .filter_map(parse_file)
            .collect();
        all.sort_by(|a, b| b.published.cmp(&a.published));
        Ok(all)
    }

    fn details(&self, project: &Project) -> Result<details::Details, String> {
        let mut out = details::Details {
            body: project.summary.clone(),
            ..details::Details::default()
        };
        if let Ok(versions) = self.versions(project) {
            let picked = super::pick(project.kind, &versions, true).or_else(|| versions.first());
            if let Some(version) = picked {
                out.dependencies = version.dependencies.clone();
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod dependency_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_required_optional_and_incompatible_relation_types() {
        let deps = parse_dependencies(&json!([
            {"modId": 100, "relationType": 3},
            {"modId": 200, "relationType": 2},
            {"modId": 300, "relationType": 5},
            {"modId": 400, "relationType": 1},
            {"modId": 500, "relationType": 4},
            {"modId": 100, "relationType": 3}
        ]));
        assert_eq!(
            deps,
            vec![
                Dependency {
                    id: "100".into(),
                    name: None,
                    kind: DependencyKind::Required,
                    version_req: None,
                },
                Dependency {
                    id: "200".into(),
                    name: None,
                    kind: DependencyKind::Optional,
                    version_req: None,
                },
                Dependency {
                    id: "300".into(),
                    name: None,
                    kind: DependencyKind::Incompatible,
                    version_req: None,
                },
            ]
        );
    }
}
