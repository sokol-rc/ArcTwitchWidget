use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    Exit,
}

#[cfg(windows)]
pub struct TrayController {
    _tray: tray_icon::TrayIcon,
    show_id: tray_icon::menu::MenuId,
    exit_id: tray_icon::menu::MenuId,
}

#[cfg(not(windows))]
pub struct TrayController;

#[cfg(windows)]
impl TrayController {
    pub fn new() -> Result<Self> {
        use tray_icon::menu::{Menu, MenuItem};
        use tray_icon::{Icon, TrayIconBuilder};

        let menu = Menu::new();
        let show = MenuItem::new("Открыть ARC Live", true, None);
        let exit = MenuItem::new("Выйти", true, None);
        menu.append(&show)?;
        menu.append(&exit)?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("ARC Live — статистика для OBS")
            .with_icon(Icon::from_rgba(crate::branding::icon_rgba(32), 32, 32)?)
            .build()?;
        Ok(Self {
            _tray: tray,
            show_id: show.id().clone(),
            exit_id: exit.id().clone(),
        })
    }

    pub fn poll(&self) -> Option<TrayAction> {
        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if event.id == self.show_id {
                return Some(TrayAction::Show);
            }
            if event.id == self.exit_id {
                return Some(TrayAction::Exit);
            }
        }
        while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    ..
                } | tray_icon::TrayIconEvent::DoubleClick {
                    button: tray_icon::MouseButton::Left,
                    ..
                }
            ) {
                return Some(TrayAction::Show);
            }
        }
        None
    }
}

#[cfg(not(windows))]
impl TrayController {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn poll(&self) -> Option<TrayAction> {
        None
    }
}
