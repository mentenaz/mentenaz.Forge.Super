use settings::{DockSide, RegisterSetting, Settings};

/// The Scaffold panel's user settings, read from the `"scaffold_panel"` settings key.
#[derive(Clone, Copy, Debug, PartialEq, RegisterSetting)]
pub struct ScaffoldPanelSettings {
    pub button: bool,
    pub dock: DockSide,
}

impl Settings for ScaffoldPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.scaffold_panel.as_ref().unwrap();
        Self {
            button: panel.button.unwrap(),
            dock: panel.dock.unwrap(),
        }
    }
}
