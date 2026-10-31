//! Bundled assets: gpui-kit's default icons plus the extra Lucide icons we use.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    pub ExtraIcons,
    [
        ListMusic,
        MicVocal,
        SkipForward,
        Trash,
        Volume2,
        ListEnd,
        ListStart,
        Download,
        FileMusic,
        Music,
        Clock,
        GripVertical,
        HardDrive,
        RotateCw,
        Info,
        Github,
        Globe
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn every_icon_we_use_is_bundled() {
        for icon in [
            IconName::ListMusic,
            IconName::MicVocal,
            IconName::SkipForward,
            IconName::Trash,
            IconName::Volume2,
            IconName::ListEnd,
            IconName::ListStart,
            IconName::Download,
            IconName::FileMusic,
            IconName::Music,
            IconName::Clock,
            IconName::HardDrive,
            IconName::Sun,
            IconName::Play,
            IconName::Pause,
            IconName::FolderOpen,
            IconName::Copy,
            IconName::CircleAlert,
            IconName::EllipsisVertical,
            IconName::Search,
            IconName::Settings,
            IconName::Calendar,
            IconName::Check,
            IconName::LoaderCircle,
        ] {
            let loaded = AppAssets.load(&icon.path()).ok().flatten();
            assert!(loaded.is_some(), "missing icon asset {}", icon.path());
        }
    }
}
