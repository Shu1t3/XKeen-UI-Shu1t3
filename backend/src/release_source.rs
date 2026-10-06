use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize)]
pub struct ReleaseSource {
    pub repository: String,
    pub setup_url: String,
}

// Embedded at build time: runtime settings cannot switch the panel to another owner.
pub static UI_RELEASE_SOURCE: LazyLock<ReleaseSource> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../release-source.json")).expect("invalid embedded panel release source")
});

pub fn setup_command() -> String {
    format!("curl -fL {} | sh", UI_RELEASE_SOURCE.setup_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_entry_points_match_embedded_fork_source() {
        let source = &*UI_RELEASE_SOURCE;
        assert_eq!(source.repository, "Shu1t3/XKeen-UI-Shu1t3");
        assert_eq!(
            source.setup_url,
            format!("https://raw.githubusercontent.com/{}/main/setup.sh", source.repository)
        );
        let installer = include_str!("../../setup.sh");
        assert!(installer.contains(&format!("UI_REPOSITORY='{}'", source.repository)));
        let migration = include_str!("../../scripts/switch-to-fork.sh");
        assert!(migration.contains(&format!("REPO={}", source.repository)));
        let readme = include_str!("../../README.md");
        assert!(readme.contains(&format!("curl {} | sh", source.setup_url)));
        assert!(readme.contains(&format!("curl {} | sh -s -- beta", source.setup_url)));
        for text in [installer, migration] {
            assert!(!text.contains("zxc-rv/XKeen-UI"));
        }
    }

    #[test]
    fn cli_setup_uses_the_same_fork_installer() {
        assert_eq!(
            setup_command(),
            format!("curl -fL {} | sh", UI_RELEASE_SOURCE.setup_url)
        );
    }
}
