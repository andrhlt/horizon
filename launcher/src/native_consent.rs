//! One deferred native-code action. Declining never invokes its installer or builder.
use horizon_launcher::{
    backend::Settings,
    browser::{installed::Entry, java},
    github_mods::Resolved,
};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NamedMod {
    pub(super) id: String,
    pub(super) name: String,
}

impl NamedMod {
    pub(super) fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        let id = id.into();
        let name = name.into();
        let name = if name.trim().is_empty() {
            id.clone()
        } else {
            name
        };
        Self { id, name }
    }

    pub(super) fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

pub(super) enum PendingConsent {
    Build(Vec<NamedMod>),
    Toggle(Entry),
    Install(crate::mods_page::InstallRequest),
    Github(Resolved, Option<String>),
    Local(NamedMod),
}

impl PendingConsent {
    pub(super) fn name(&self) -> String {
        match self {
            Self::Build(mods) => mods
                .iter()
                .map(NamedMod::display_name)
                .collect::<Vec<_>>()
                .join(", "),
            Self::Toggle(entry) => entry.title.clone(),
            Self::Install(request) => request.display_name().to_owned(),
            Self::Local(m) => m.display_name().to_owned(),
            Self::Github(found, _) => found.label(),
        }
    }
}

/// Consume exactly one request. Permission is persisted before returning an action to execute.
/// Cancel and Escape both use `allow = false`, and cannot return an executable action.
pub(super) fn resolve(
    pending: &mut Option<PendingConsent>,
    allow: bool,
    supported: bool,
    settings: &mut Settings,
    home: &Path,
) -> Result<Option<PendingConsent>, String> {
    if pending.is_none() {
        return Ok(None);
    }
    if allow {
        if !supported {
            return Err(crate::i18n::tr("Unavailable on this platform").into());
        }
        settings.allow_native_mods()?;
        return Ok(pending.take());
    }
    if let Some(action) = pending.take() {
        match action {
            PendingConsent::Build(mods) => {
                // One rewrite: a failure can never leave some declined mods on.
                let ids: Vec<&str> = mods.iter().map(|m| m.id.as_str()).collect();
                java::set_all_enabled(home, &ids, false)?;
            }
            PendingConsent::Toggle(entry) => {
                if let Some(id) = horizon_launcher::browser::horizon::mod_id(&entry) {
                    java::set_enabled(home, id, false)?;
                }
            }
            PendingConsent::Local(m) => java::set_enabled(home, &m.id, false)?,
            PendingConsent::Install(..) | PendingConsent::Github(..) => {}
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods_page::InstallRequest;
    use horizon_launcher::browser::{Kind, Project, SourceId};

    fn sample_project(id: &str, title: &str) -> Project {
        Project {
            source: SourceId::Horizon,
            id: id.into(),
            slug: id.into(),
            title: title.into(),
            summary: String::new(),
            author: String::new(),
            downloads: 0,
            page: String::new(),
            icon: None,
            license: String::new(),
            kind: Kind::Mod,
            game_versions: Vec::new(),
            loaders: Vec::new(),
            blocked: None,
        }
    }

    #[test]
    fn consent_name_prefers_display_title_and_falls_back_to_id() {
        let browse = PendingConsent::Install(InstallRequest::Browse(Box::new(sample_project(
            "fast_ore", "Fast Ore",
        ))));
        assert_eq!(browse.name(), "Fast Ore");

        let palette = PendingConsent::Install(InstallRequest::Palette(Box::new(sample_project(
            "hitbox_plus",
            "Hitbox Plus",
        ))));
        assert_eq!(palette.name(), "Hitbox Plus");

        let blank_title = PendingConsent::Install(InstallRequest::Browse(Box::new(
            sample_project("fast_ore", "   "),
        )));
        assert_eq!(blank_title.name(), "fast_ore");

        let installed = PendingConsent::Install(InstallRequest::Installed(
            SourceId::Horizon,
            "fast_ore".into(),
        ));
        assert_eq!(installed.name(), "fast_ore");

        let local = PendingConsent::Local(NamedMod::new("fast_ore", "Fast Ore"));
        assert_eq!(local.name(), "Fast Ore");

        let local_fallback = PendingConsent::Local(NamedMod::new("fast_ore", ""));
        assert_eq!(local_fallback.name(), "fast_ore");

        let build = PendingConsent::Build(vec![
            NamedMod::new("fast_ore", "Fast Ore"),
            NamedMod::new("greeter", "   "),
            NamedMod::new("hitbox_plus", "Hitbox Plus"),
        ]);
        assert_eq!(build.name(), "Fast Ore, greeter, Hitbox Plus");
    }

    #[test]
    fn cancel_escape_and_allow_consume_only_the_pending_toggle() {
        // Settings is process-global: exercise this transition in its own isolated process.
        if std::env::var_os("HORIZON_CONSENT_TEST_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--include-ignored", "--exact", "native_consent::tests::cancel_escape_and_allow_consume_only_the_pending_toggle"])
                .env("HORIZON_CONSENT_TEST_CHILD", "1")
                .env("HORIZON_LAUNCHER_HOME", home.path())
                .env("HORIZON_HOME", home.path())
                .status().unwrap();
            assert!(status.success());
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        // Decline returns no dispatchable action: no installer or builder can run.
        // The build layer separately tests its HTTP backstop with a mock proxy.
        for _ in ["Cancel", "Escape"] {
            let mut pending = Some(PendingConsent::Local(NamedMod::new("sample", "Sample Mod")));
            assert!(
                resolve(&mut pending, false, true, &mut settings, home.path())
                    .unwrap()
                    .is_none()
            );
            assert!(pending.is_none());
            assert!(!java::is_enabled(home.path(), "sample"));
            assert!(settings.native_mods_allowed_at.is_none());
        }
        java::set_enabled(home.path(), "sample", true).unwrap();
        java::set_enabled(home.path(), "second", true).unwrap();
        java::set_enabled(home.path(), "unrelated", true).unwrap();
        let mut pending = Some(PendingConsent::Build(vec![
            NamedMod::new("sample", "Sample Mod"),
            NamedMod::new("second", "Second Mod"),
        ]));
        resolve(&mut pending, false, true, &mut settings, home.path()).unwrap();
        assert!(
            !java::is_enabled(home.path(), "sample") && !java::is_enabled(home.path(), "second"),
            "NoConsent cancellation turns every one of the build's mods off"
        );
        assert!(java::is_enabled(home.path(), "unrelated"));
        let mut unsupported = Some(PendingConsent::Local(NamedMod::new("sample", "Sample Mod")));
        assert!(resolve(&mut unsupported, true, false, &mut settings, home.path()).is_err());
        assert!(unsupported.is_some());
        assert!(Settings::load().native_mods_allowed_at.is_none());
        if horizon_launcher::rust_toolchain::host().is_some() {
            let mut pending = Some(PendingConsent::Local(NamedMod::new("sample", "Sample Mod")));
            let action = resolve(&mut pending, true, true, &mut settings, home.path())
                .unwrap()
                .unwrap();
            assert!(Settings::load().native_mods_allowed_at.is_some());
            match action {
                PendingConsent::Local(m) => java::set_enabled(home.path(), &m.id, true).unwrap(),
                _ => panic!("wrong pending action"),
            }
            assert!(java::is_enabled(home.path(), "sample"));
            assert!(
                resolve(&mut pending, true, true, &mut settings, home.path())
                    .unwrap()
                    .is_none(),
                "the action cannot execute twice"
            );
        }
    }
}
