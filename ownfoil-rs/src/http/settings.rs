use std::path::Path;

use crate::config::TitleDbConfig;

pub fn save_settings(data_dir: &Path, titledb: &TitleDbConfig) -> std::io::Result<()> {
    let settings_dir = data_dir;
    std::fs::create_dir_all(settings_dir)?;
    let path = settings_dir.join("settings.toml");
    let content = toml::to_string_pretty(&RuntimeSettings {
        titledb: titledb.clone(),
    })
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, content)
}

#[derive(serde::Serialize)]
struct RuntimeSettings {
    titledb: TitleDbConfig,
}
